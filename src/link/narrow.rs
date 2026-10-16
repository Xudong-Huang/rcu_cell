use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

const LEADING_BITS: usize = 8;
const ALIGN_BITS: usize = 3;

const LOWER_MASK: usize = (1 << ALIGN_BITS) - 1;
const HIGHER_MASK: usize = !((1 << (usize::MAX.leading_ones() as usize - LEADING_BITS)) - 1);
const REFCOUNT_MASK: usize = (1 << (LEADING_BITS + ALIGN_BITS)) - 1;
const UPDTATE_MASK: usize = 1 << (LEADING_BITS + ALIGN_BITS - 1);
const UPDATE_REF_MASK: usize = REFCOUNT_MASK & !UPDTATE_MASK;

#[inline]
fn ptr_to_addr<T: ?Sized>(ptr: *const T) -> usize {
    ptr.cast::<()>().expose_provenance()
}

/// A wrapper of the pointer to the inner Arc data
pub(crate) struct LinkWrapper<T: ?Sized> {
    ptr: AtomicUsize,
    // the pointer metadata (vtable or length) of unsized `T` plus one, 0 means it is
    // not set yet. It is set once by the first value written and never changes.
    meta: AtomicUsize,
    phantom: PhantomData<*const T>,
}

impl<T: ?Sized> LinkWrapper<T> {
    const IS_THIN: bool = core::mem::size_of::<*const T>() == core::mem::size_of::<usize>();

    #[inline]
    pub(crate) const fn new() -> Self {
        LinkWrapper {
            ptr: AtomicUsize::new(0),
            meta: AtomicUsize::new(0),
            phantom: PhantomData,
        }
    }

    // the metadata word of a fat pointer, the data address is the first word
    #[inline]
    fn meta_of(ptr: *const T) -> usize {
        let words = unsafe { core::mem::transmute_copy::<*const T, [*const (); 2]>(&ptr) };
        words[1].expose_provenance()
    }

    #[inline]
    fn opt_addr(ptr: Option<NonNull<T>>) -> usize {
        ptr.map_or(0, |p| ptr_to_addr(p.as_ptr()))
    }

    // rebuild a raw pointer from an address with the metadata of the link,
    // None if the metadata is needed but not set yet, so no invalid pointer is built
    #[inline]
    fn raw_at(&self, addr: usize) -> Option<*const T> {
        let thin: *const () = core::ptr::with_exposed_provenance(addr);
        if Self::IS_THIN {
            return Some(unsafe { core::mem::transmute_copy::<*const (), *const T>(&thin) });
        }
        let meta = self.meta.load(Ordering::Acquire).checked_sub(1)?;
        let words = [thin, core::ptr::with_exposed_provenance(meta)];
        Some(unsafe { core::mem::transmute_copy::<[*const (); 2], *const T>(&words) })
    }

    #[inline]
    fn ptr_at(&self, addr: usize) -> Option<NonNull<T>> {
        NonNull::new(self.raw_at(addr)? as *mut T)
    }

    /// a null pointer, panics if `T` is unsized and no value was ever written
    #[inline]
    pub(crate) fn null(&self) -> *const T {
        self.raw_at(0)
            .expect("a null pointer of an unsized type needs the metadata of a written value")
    }

    // the metadata is set before the first address is published
    #[inline]
    fn addr_to_ptr(&self, addr: usize) -> Option<NonNull<T>> {
        if addr == 0 {
            return None;
        }
        let ptr = self.ptr_at(addr);
        debug_assert!(ptr.is_some());
        ptr
    }

    /// the dangling pointer of a `Weak`, None if the metadata is not known yet
    #[inline]
    pub(crate) fn dangling(&self) -> Option<NonNull<T>> {
        self.ptr_at(usize::MAX)
    }

    /// the first value fixes the pointer metadata (vtable or length), later ones must
    /// have the same, otherwise a reader would pair the new address with a wrong
    /// metadata, so panic. Vtable addresses are not guaranteed to be unique for a
    /// type, so this may panic for a valid value, but it never accepts a wrong one.
    #[inline]
    pub(crate) fn check_meta(&self, ptr: *const T) {
        if Self::IS_THIN {
            return;
        }
        // the stored metadata is shifted by one so 0 can mean "not set yet", a real
        // metadata may be 0 (an empty slice). `wrapping_add` is fine: only usize::MAX wraps
        // to 0, which a vtable address or slice length can never be. Read back with `checked_sub(1)`.
        let meta = Self::meta_of(ptr).wrapping_add(1);
        let cur = match self
            .meta
            .compare_exchange(0, meta, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return,
            Err(cur) => cur,
        };
        assert!(
            cur == meta,
            "the pointer metadata must be the same as the one the cell was created with"
        );
    }

    #[inline]
    pub(crate) fn from_ptr(ptr: Option<NonNull<T>>) -> Self {
        let addr = Self::opt_addr(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        // metadata is stored plus one, see `check_meta`
        let meta = match ptr {
            Some(p) if !Self::IS_THIN => Self::meta_of(p.as_ptr()).wrapping_add(1),
            _ => 0,
        };
        LinkWrapper {
            ptr: AtomicUsize::new(addr << LEADING_BITS),
            meta: AtomicUsize::new(meta),
            phantom: PhantomData,
        }
    }

    pub(crate) unsafe fn compare_exchange(
        &self,
        current: Option<NonNull<T>>,
        new: Option<NonNull<T>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Option<NonNull<T>>, Option<NonNull<T>>> {
        let new_addr = Self::opt_addr(new);
        let new = new_addr << LEADING_BITS;

        let old_addr = Self::opt_addr(current);
        let old = old_addr << LEADING_BITS;

        let backoff = crossbeam_utils::Backoff::new();
        loop {
            match self.ptr.compare_exchange(old, new, success, failure) {
                Ok(_addr) => {
                    // assert_eq!(old, addr);
                    return Ok(current);
                }
                Err(addr) => {
                    let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
                    if addr != old {
                        return Err(self.addr_to_ptr(addr));
                    }
                    backoff.snooze();
                }
            }
        }
    }

    pub(crate) fn update(&self, ptr: Option<NonNull<T>>) -> Option<NonNull<T>> {
        use Ordering::*;
        let addr = Self::opt_addr(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let new = addr << LEADING_BITS;
        let mut old = self.ptr.load(Relaxed) & !REFCOUNT_MASK;

        let backoff = crossbeam_utils::Backoff::new();
        // wait all reader release
        while let Err(addr) = self.ptr.compare_exchange_weak(old, new, Release, Relaxed) {
            old = addr & !REFCOUNT_MASK;
            backoff.snooze();
        }

        core::sync::atomic::fence(Ordering::Acquire);
        let addr = old >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }

    // this is only used after lock_read
    pub(crate) fn unlock_update(&self, ptr: Option<NonNull<T>>) -> Option<NonNull<T>> {
        use Ordering::*;
        let addr = Self::opt_addr(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let new = addr << LEADING_BITS;
        let mut old = self.ptr.load(Relaxed) & !UPDATE_REF_MASK | UPDTATE_MASK;

        let backoff = crossbeam_utils::Backoff::new();
        // wait all reader release
        while let Err(addr) = self.ptr.compare_exchange_weak(old, new, Release, Relaxed) {
            old = addr & !UPDATE_REF_MASK | UPDTATE_MASK;
            backoff.snooze();
        }

        core::sync::atomic::fence(Ordering::Acquire);
        let addr = (old & !UPDTATE_MASK) >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }

    #[inline]
    pub(crate) fn is_none(&self) -> bool {
        self.ptr.load(Ordering::Relaxed) & !REFCOUNT_MASK == 0
    }

    #[inline]
    pub(crate) fn inc_ref(&self) -> Option<NonNull<T>> {
        let addr = self.ptr.fetch_add(1, Ordering::Acquire);
        let refs = addr & REFCOUNT_MASK;
        assert!(refs < REFCOUNT_MASK, "Too many references");
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }

    #[inline]
    pub(crate) fn get_ref(&self) -> Option<NonNull<T>> {
        let addr = self.ptr.load(Ordering::Acquire);
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }

    #[inline]
    pub(crate) fn dec_ref(&self) {
        self.ptr.fetch_sub(1, Ordering::Release);
    }

    // read the inner Arc and increase the ref count
    // to prevet other writer to update the inner Arc
    // should be paired used with unlock_update
    #[inline]
    pub(crate) fn lock_read(&self) -> Option<NonNull<T>> {
        use Ordering::*;

        let addr = self.ptr.load(Relaxed);
        let mut old = addr & !UPDTATE_MASK; // clear the update flag
        let mut new = addr | UPDTATE_MASK; // set the update flag

        let refs = old & UPDATE_REF_MASK;
        assert!(refs < UPDATE_REF_MASK, "Too many references");

        let backoff = crossbeam_utils::Backoff::new();
        while let Err(addr) = self.ptr.compare_exchange_weak(old, new, Release, Relaxed) {
            old = addr & !UPDTATE_MASK;
            new = addr | UPDTATE_MASK;
            backoff.snooze();
        }

        core::sync::atomic::fence(Ordering::Acquire);

        let addr = (old & !REFCOUNT_MASK) >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ptr = self.get_ref();
        f.debug_struct("Link").field("ptr", &ptr).finish()
    }
}
