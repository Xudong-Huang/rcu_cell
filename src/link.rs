use alloc::sync::{Arc, Weak};
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

//---------------------------------------------------------------------------------------
// LinkWrapper
//---------------------------------------------------------------------------------------

#[repr(C)]
union Ptr<T> {
    addr: usize,
    ptr: *const T,
}

impl<T> Ptr<T> {
    #[inline]
    const fn addr(self) -> usize {
        unsafe { self.addr }
    }

    #[inline]
    const fn ptr(self) -> *const T {
        unsafe { self.ptr }
    }
}

/// A wrapper of the pointer to the inner Arc data
pub(crate) struct LinkWrapper<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*const T>,
}

impl<T> LinkWrapper<T> {
    #[inline]
    pub(crate) const fn new(ptr: *const T) -> Self {
        let addr = Ptr { ptr }.addr();
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        LinkWrapper {
            ptr: AtomicUsize::new(addr << LEADING_BITS),
            phantom: PhantomData,
        }
    }

    pub(crate) fn update(&self, ptr: *const T) -> *const T {
        use Ordering::*;
        let addr = Ptr { ptr }.addr();
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
        Ptr { addr }.ptr()
    }

    // this is only used after lock_read
    pub(crate) fn unlock_update(&self, ptr: *const T) -> *const T {
        use Ordering::*;
        let addr = Ptr { ptr }.addr();
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
        Ptr { addr }.ptr()
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
        Ptr { addr }.ptr()
    }

    #[inline]
    pub(crate) fn get_ref(&self) -> *const T {
        let addr = self.ptr.load(Ordering::Acquire);
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        Ptr { addr }.ptr()
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
        Ptr { addr }.ptr()
    }
}

impl<T: fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ptr = self.get_ref();
        f.debug_struct("Link").field("ptr", &ptr).finish()
    }
}

#[inline]
pub(crate) fn ptr_to_arc<T>(ptr: *const T) -> Option<Arc<T>> {
    (!ptr.is_null()).then(|| unsafe { Arc::from_raw(ptr) })
}

#[inline]
pub(crate) fn ptr_to_weak<T>(ptr: *const T) -> Weak<T> {
    if ptr.is_null() {
        Weak::new()
    } else {
        unsafe { Weak::from_raw(ptr) }
    }
}
