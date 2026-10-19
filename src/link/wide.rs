use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(rcu_cell_atomic_u128)]
use core::sync::atomic::AtomicU128;

#[cfg(not(rcu_cell_atomic_u128))]
use super::atomic128::AtomicU128;

use super::{HIGHER_MASK, LEADING_BITS, LOWER_MASK, REFCOUNT_MASK, UPDATE_MASK, UPDATE_REF_MASK};

/// the content of the link: the tagged address word and the pointer metadata word
/// (the vtable or the length of an unsized `T` plus one, 0 means it is not set
/// yet, always 0 for sized `T`)
type Pair = (usize, usize);

#[inline]
fn ptr_to_addr<T: ?Sized>(ptr: *const T) -> usize {
    ptr.cast::<()>().expose_provenance()
}

#[inline]
const fn pair_to_u128((lo, hi): Pair) -> u128 {
    (lo as u128) | ((hi as u128) << 64)
}

#[inline]
fn u128_to_pair(v: u128) -> Pair {
    (v as usize, (v >> 64) as usize)
}

// sized `T` only touches the `thin` field and unsized `T` only touches the `wide` one.
// Every method takes `thin`, which must always be the `IS_THIN` of the owning link, so
// the field that is accessed is the one that was initialized.
union Storage {
    thin: ManuallyDrop<AtomicUsize>,
    wide: ManuallyDrop<AtomicU128>,
}

impl Storage {
    #[inline]
    const fn new(thin: bool, pair: Pair) -> Self {
        if thin {
            Storage {
                thin: ManuallyDrop::new(AtomicUsize::new(pair.0)),
            }
        } else {
            Storage {
                wide: ManuallyDrop::new(AtomicU128::new(pair_to_u128(pair))),
            }
        }
    }

    #[inline]
    fn load(&self, thin: bool, order: Ordering) -> Pair {
        unsafe {
            if thin {
                (self.thin.load(order), 0)
            } else {
                u128_to_pair(self.wide.load(order))
            }
        }
    }

    // the initial guess of a CAS loop, it may be torn since the CAS validates it
    #[inline]
    fn load_hint(&self, thin: bool) -> Pair {
        #[cfg(rcu_cell_atomic_u128)]
        return self.load(thin, Ordering::Relaxed);
        #[cfg(not(rcu_cell_atomic_u128))]
        unsafe {
            if thin {
                (self.thin.load(Ordering::Relaxed), 0)
            } else {
                u128_to_pair(self.wide.load_hint())
            }
        }
    }

    #[inline]
    fn cas(
        &self,
        thin: bool,
        weak: bool,
        old: Pair,
        new: Pair,
        s: Ordering,
        f: Ordering,
    ) -> Result<Pair, Pair> {
        unsafe {
            if thin {
                let r = if weak {
                    self.thin.compare_exchange_weak(old.0, new.0, s, f)
                } else {
                    self.thin.compare_exchange(old.0, new.0, s, f)
                };
                r.map(|v| (v, 0)).map_err(|v| (v, 0))
            } else {
                let (old, new) = (pair_to_u128(old), pair_to_u128(new));
                let r = if weak {
                    self.wide.compare_exchange_weak(old, new, s, f)
                } else {
                    self.wide.compare_exchange(old, new, s, f)
                };
                r.map(u128_to_pair).map_err(u128_to_pair)
            }
        }
    }

    #[inline]
    fn fetch_add(&self, thin: bool, order: Ordering) -> Pair {
        unsafe {
            if thin {
                (self.thin.fetch_add(1, order), 0)
            } else {
                u128_to_pair(self.wide.fetch_add(1, order))
            }
        }
    }

    #[inline]
    fn fetch_sub(&self, thin: bool, order: Ordering) {
        unsafe {
            if thin {
                self.thin.fetch_sub(1, order);
            } else {
                self.wide.fetch_sub(1, order);
            }
        }
    }
}

/// A wrapper of the pointer to the inner Arc data.
///
/// For a sized `T` the whole link is a single `AtomicUsize`. For an unsized `T`
/// the pointer is a fat pointer, the address (with the tag bits) and the metadata
/// are stored in one `AtomicU128`, so they are always updated together.
pub(crate) struct LinkWrapper<T: ?Sized> {
    storage: Storage,
    phantom: PhantomData<*const T>,
}

impl<T: ?Sized> LinkWrapper<T> {
    const IS_THIN: bool = core::mem::size_of::<*const T>() == core::mem::size_of::<usize>();
    #[inline]
    pub(crate) const fn new() -> Self {
        LinkWrapper {
            storage: Storage::new(Self::IS_THIN, (0, 0)),
            phantom: PhantomData,
        }
    }

    const _CHECK: () = assert!(
        Self::IS_THIN || core::mem::size_of::<*const T>() == 2 * core::mem::size_of::<usize>()
    );

    // split a pointer into its address and metadata word, None is (0, 0)
    #[inline]
    fn split(ptr: Option<NonNull<T>>) -> Pair {
        let Some(ptr) = ptr else { return (0, 0) };
        let ptr = ptr.as_ptr() as *const T;
        let addr = ptr_to_addr(ptr);
        if Self::IS_THIN {
            return (addr, 0);
        }
        // the data address is the first word of a fat pointer, the metadata is the second
        let words = unsafe { core::mem::transmute_copy::<*const T, [*const (); 2]>(&ptr) };
        // the stored metadata is shifted by one so 0 can mean "not set yet", a real
        // metadata may be 0 (an empty slice). `wrapping_add` is fine: only usize::MAX wraps
        // to 0, which a vtable address or slice length can never be. Read back with `checked_sub(1)`.
        (addr, words[1].expose_provenance().wrapping_add(1))
    }

    // rebuild a raw pointer from an address and metadata word, None if the
    // metadata is needed but not set, so no invalid pointer is ever built
    #[inline]
    fn join_raw(addr: usize, meta: usize) -> Option<*const T> {
        let thin: *const () = core::ptr::with_exposed_provenance(addr);
        if Self::IS_THIN {
            return Some(unsafe { core::mem::transmute_copy::<*const (), *const T>(&thin) });
        }
        let words = [
            thin,
            core::ptr::with_exposed_provenance(meta.checked_sub(1)?),
        ];
        Some(unsafe { core::mem::transmute_copy::<[*const (); 2], *const T>(&words) })
    }

    #[inline]
    fn join(addr: usize, meta: usize) -> Option<NonNull<T>> {
        NonNull::new(Self::join_raw(addr, meta)? as *mut T)
    }

    /// a null pointer, panics if `T` is unsized and no value was ever written
    #[inline]
    pub(crate) fn null(&self) -> *const T {
        Self::join_raw(0, self.load(Ordering::Relaxed).1)
            .expect("a null pointer of an unsized type needs the metadata of a written value")
    }

    #[inline]
    fn load(&self, order: Ordering) -> Pair {
        self.storage.load(Self::IS_THIN, order)
    }

    #[inline]
    fn load_hint(&self) -> Pair {
        self.storage.load_hint(Self::IS_THIN)
    }

    #[inline]
    fn cas_weak(&self, old: Pair, new: Pair, s: Ordering, f: Ordering) -> Result<Pair, Pair> {
        self.storage.cas(Self::IS_THIN, true, old, new, s, f)
    }

    #[inline]
    fn cas(&self, old: Pair, new: Pair, s: Ordering, f: Ordering) -> Result<Pair, Pair> {
        self.storage.cas(Self::IS_THIN, false, old, new, s, f)
    }

    // the reference count lives in the lower bits of the first word, and the
    // count is checked not to overflow, so there is no carry into the metadata
    #[inline]
    fn add_ref(&self) -> Pair {
        self.storage.fetch_add(Self::IS_THIN, Ordering::Acquire)
    }

    #[inline]
    fn sub_ref(&self) {
        self.storage.fetch_sub(Self::IS_THIN, Ordering::Release);
    }

    #[inline]
    fn word_to_ptr((word, meta): Pair) -> Option<NonNull<T>> {
        let addr = (word & !REFCOUNT_MASK) >> LEADING_BITS;
        if addr == 0 {
            return None;
        }
        let ptr = Self::join(addr, meta);
        debug_assert!(ptr.is_some());
        ptr
    }

    /// the dangling pointer of a `Weak`, None if the metadata is not known yet
    #[inline]
    pub(crate) fn dangling(&self) -> Option<NonNull<T>> {
        Self::join(usize::MAX, self.load(Ordering::Relaxed).1)
    }

    // any metadata is accepted, it is stored together with the address
    #[inline]
    pub(crate) fn check_meta(&self, _ptr: *const T) {}

    #[inline]
    pub(crate) fn from_ptr(ptr: Option<NonNull<T>>) -> Self {
        let () = Self::_CHECK;
        let (addr, meta) = Self::split(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let word = addr << LEADING_BITS;
        LinkWrapper {
            storage: Storage::new(Self::IS_THIN, (word, meta)),
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
        let (new_addr, new_meta) = Self::split(new);
        let old_word = Self::split(current).0 << LEADING_BITS;

        let backoff = crossbeam_utils::Backoff::new();
        loop {
            // only the address identifies the value, the metadata follows it
            let (cur_word, cur_meta) = self.load(Ordering::Relaxed);
            if cur_word & !REFCOUNT_MASK != old_word {
                return Err(Self::word_to_ptr((cur_word, cur_meta)));
            }
            // a null target keeps the metadata
            let new_meta = if new_addr == 0 { cur_meta } else { new_meta };
            let new = (new_addr << LEADING_BITS, new_meta);
            match self.cas((old_word, cur_meta), new, success, failure) {
                Ok(_) => return Ok(Self::word_to_ptr((old_word, cur_meta))),
                Err(_) => backoff.snooze(),
            }
        }
    }

    pub(crate) fn update(&self, ptr: Option<NonNull<T>>) -> Option<NonNull<T>> {
        use Ordering::*;
        let (addr, meta) = Self::split(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let new_word = addr << LEADING_BITS;
        let mut cur = self.load_hint();

        let backoff = crossbeam_utils::Backoff::new();
        // wait all reader release
        loop {
            let old = (cur.0 & !REFCOUNT_MASK, cur.1);
            // a null target keeps the metadata
            let new = (new_word, if addr == 0 { cur.1 } else { meta });
            match self.cas_weak(old, new, Release, Relaxed) {
                Ok(_) => {
                    core::sync::atomic::fence(Ordering::Acquire);
                    return Self::word_to_ptr(old);
                }
                Err(c) => {
                    cur = c;
                    backoff.snooze();
                }
            }
        }
    }

    // this is only used after lock_read
    pub(crate) fn unlock_update(&self, ptr: Option<NonNull<T>>) -> Option<NonNull<T>> {
        use Ordering::*;
        let (addr, meta) = Self::split(ptr);
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let new_word = addr << LEADING_BITS;
        let mut cur = self.load_hint();

        let backoff = crossbeam_utils::Backoff::new();
        // wait all reader release
        loop {
            let old = (cur.0 & !UPDATE_REF_MASK | UPDATE_MASK, cur.1);
            let new = (new_word, if addr == 0 { cur.1 } else { meta });
            match self.cas_weak(old, new, Release, Relaxed) {
                Ok(_) => {
                    core::sync::atomic::fence(Ordering::Acquire);
                    return Self::word_to_ptr((old.0 & !UPDATE_MASK, old.1));
                }
                Err(c) => {
                    cur = c;
                    backoff.snooze();
                }
            }
        }
    }

    #[inline]
    pub(crate) fn is_none(&self) -> bool {
        self.load(Ordering::Relaxed).0 & !REFCOUNT_MASK == 0
    }

    #[inline]
    pub(crate) fn inc_ref(&self) -> Option<NonNull<T>> {
        let old = self.add_ref();
        let refs = old.0 & REFCOUNT_MASK;
        assert!(refs < REFCOUNT_MASK, "Too many references");
        Self::word_to_ptr(old)
    }

    #[inline]
    pub(crate) fn get_ref(&self) -> Option<NonNull<T>> {
        Self::word_to_ptr(self.load(Ordering::Acquire))
    }

    #[inline]
    pub(crate) fn dec_ref(&self) {
        self.sub_ref();
    }

    // read the inner Arc and increase the ref count
    // to prevet other writer to update the inner Arc
    // should be paired used with unlock_update
    #[inline]
    pub(crate) fn lock_read(&self) -> Option<NonNull<T>> {
        use Ordering::*;

        let mut cur = self.load_hint();

        let backoff = crossbeam_utils::Backoff::new();
        loop {
            let old = (cur.0 & !UPDATE_MASK, cur.1); // clear the update flag
            let new = (cur.0 | UPDATE_MASK, cur.1); // set the update flag

            let refs = old.0 & UPDATE_REF_MASK;
            assert!(refs < UPDATE_REF_MASK, "Too many references");

            match self.cas_weak(old, new, Release, Relaxed) {
                Ok(_) => {
                    core::sync::atomic::fence(Ordering::Acquire);
                    return Self::word_to_ptr(old);
                }
                Err(c) => {
                    cur = c;
                    backoff.snooze();
                }
            }
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ptr = self.get_ref();
        f.debug_struct("Link").field("ptr", &ptr).finish()
    }
}
