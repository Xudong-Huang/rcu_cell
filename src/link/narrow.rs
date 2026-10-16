use core::fmt;
use core::marker::PhantomData;
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
    // the pointer metadata (vtable or length) of unsized `T`, it never changes
    template: *const T,
    phantom: PhantomData<*const T>,
}

impl<T> LinkWrapper<T> {
    #[inline]
    pub(crate) const fn new() -> Self {
        LinkWrapper {
            ptr: AtomicUsize::new(0),
            template: core::ptr::null(),
            phantom: PhantomData,
        }
    }
}

impl<T: ?Sized> LinkWrapper<T> {
    const IS_THIN: bool = core::mem::size_of::<*const T>() == core::mem::size_of::<usize>();

    // rebuild a pointer from an address, the metadata is taken from the template
    #[inline]
    fn addr_to_ptr(&self, addr: usize) -> *const T {
        let thin: *const () = core::ptr::with_exposed_provenance(addr);
        if Self::IS_THIN {
            // sized `T` has no metadata, skip reading the template
            return unsafe { core::mem::transmute_copy::<*const (), *const T>(&thin) };
        }
        let mut ptr = self.template;
        // the data address is the first word of a (fat) pointer
        unsafe { *(&mut ptr as *mut *const T as *mut *const ()) = thin };
        ptr
    }

    /// a null pointer that carries the metadata of this link
    #[inline]
    pub(crate) fn null(&self) -> *const T {
        self.addr_to_ptr(0)
    }

    /// panic if the pointer metadata (vtable or length) differs from the one this
    /// link was built with, otherwise a reader would pair the new address with a
    /// wrong metadata. Vtable addresses are not guaranteed to be unique for a type,
    /// so this may panic for a valid value, but it never accepts a wrong one.
    #[inline]
    #[allow(ambiguous_wide_pointer_comparisons)]
    pub(crate) fn check_meta(&self, ptr: *const T) {
        if Self::IS_THIN || self.template.is_null() {
            return;
        }
        assert!(
            core::ptr::eq(ptr, self.addr_to_ptr(ptr_to_addr(ptr))),
            "the pointer metadata must be the same as the one the cell was created with"
        );
    }

    #[inline]
    pub(crate) fn from_ptr(ptr: *const T) -> Self {
        let addr = ptr_to_addr(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        LinkWrapper {
            ptr: AtomicUsize::new(addr << LEADING_BITS),
            template: ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) unsafe fn compare_exchange(
        &self,
        current: *const T,
        new: *const T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*const T, *const T> {
        let new_addr = ptr_to_addr(new);
        let new = new_addr << LEADING_BITS;

        let old_addr = ptr_to_addr(current);
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

    pub(crate) fn update(&self, ptr: *const T) -> *const T {
        use Ordering::*;
        let addr = ptr_to_addr(ptr);
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
    pub(crate) fn unlock_update(&self, ptr: *const T) -> *const T {
        use Ordering::*;
        let addr = ptr_to_addr(ptr);
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
    pub(crate) fn inc_ref(&self) -> *const T {
        let addr = self.ptr.fetch_add(1, Ordering::Acquire);
        let refs = addr & REFCOUNT_MASK;
        assert!(refs < REFCOUNT_MASK, "Too many references");
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        self.addr_to_ptr(addr)
    }

    #[inline]
    pub(crate) fn get_ref(&self) -> *const T {
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
    pub(crate) fn lock_read(&self) -> *const T {
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
