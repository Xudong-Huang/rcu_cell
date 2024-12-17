#![doc = include_str!("../README.md")]
#![no_std]

extern crate alloc;

use alloc::sync::{Arc, Weak};

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{fmt, ptr};

// we only support 64-bit platform
const _: () = assert!(usize::MAX.count_ones() == 64);
const _: () = assert!(core::mem::size_of::<*const ()>() == 8);

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
struct LinkWrapper<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*const T>,
}

impl<T> LinkWrapper<T> {
    #[inline]
    const fn new(ptr: *const T) -> Self {
        let addr = Ptr { ptr }.addr();
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        LinkWrapper {
            ptr: AtomicUsize::new(addr << LEADING_BITS),
            phantom: PhantomData,
        }
    }

    fn update(&self, ptr: *const T) -> *const T {
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
    fn unlock_update(&self, ptr: *const T) -> *const T {
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
    fn is_none(&self) -> bool {
        self.ptr.load(Ordering::Relaxed) & !REFCOUNT_MASK == 0
    }

    #[inline]
    fn inc_ref(&self) -> *const T {
        let addr = self.ptr.fetch_add(1, Ordering::Acquire);
        let refs = addr & REFCOUNT_MASK;
        assert!(refs < REFCOUNT_MASK, "Too many references");
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        Ptr { addr }.ptr()
    }

    #[inline]
    fn get_ref(&self) -> *const T {
        let addr = self.ptr.load(Ordering::Acquire);
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        Ptr { addr }.ptr()
    }

    #[inline]
    fn dec_ref(&self) {
        self.ptr.fetch_sub(1, Ordering::Release);
    }

    // read the inner Arc and increase the ref count
    // to prevet other writer to update the inner Arc
    // should be paired used with unlock_update
    #[inline]
    fn lock_read(&self) -> *const T {
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
fn ptr_to_arc<T>(ptr: *const T) -> Option<Arc<T>> {
    (!ptr.is_null()).then(|| unsafe { Arc::from_raw(ptr) })
}

#[inline]
fn ptr_to_weak<T>(ptr: *const T) -> Weak<T> {
    if ptr.is_null() {
        Weak::new()
    } else {
        unsafe { Weak::from_raw(ptr) }
    }
}

//---------------------------------------------------------------------------------------
// RcuCell
//---------------------------------------------------------------------------------------

/// RCU cell, it behaves like `RwLock<Option<Arc<T>>>`
#[derive(Debug)]
pub struct RcuCell<T> {
    link: LinkWrapper<T>,
}

unsafe impl<T: Send> Send for RcuCell<T> {}
unsafe impl<T: Send + Sync> Sync for RcuCell<T> {}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        let ptr = self.link.get_ref();
        let _ = ptr_to_arc(ptr);
    }
}

impl<T> Default for RcuCell<T> {
    fn default() -> Self {
        RcuCell::none()
    }
}

impl<T> From<Arc<T>> for RcuCell<T> {
    fn from(data: Arc<T>) -> Self {
        let arc_ptr = Arc::into_raw(data);
        RcuCell {
            link: LinkWrapper::new(arc_ptr),
        }
    }
}

impl<T> From<Option<Arc<T>>> for RcuCell<T> {
    fn from(data: Option<Arc<T>>) -> Self {
        match data {
            Some(data) => {
                let arc_ptr = Arc::into_raw(data);
                RcuCell {
                    link: LinkWrapper::new(arc_ptr),
                }
            }
            None => RcuCell::none(),
        }
    }
}

impl<T> RcuCell<T> {
    /// create an empty rcu cell instance
    #[inline]
    pub const fn none() -> Self {
        RcuCell {
            link: LinkWrapper::new(ptr::null()),
        }
    }

    /// create rcu cell from a value
    #[inline]
    pub fn some(data: T) -> Self {
        let ptr = Arc::into_raw(Arc::new(data));
        RcuCell {
            link: LinkWrapper::new(ptr),
        }
    }

    /// create rcu cell from value that can be converted to Option<T>
    #[inline]
    pub fn new(data: impl Into<Option<T>>) -> Self {
        let data = data.into();
        match data {
            Some(data) => Self::some(data),
            None => Self::none(),
        }
    }

    /// convert the rcu cell to an Arc value
    #[inline]
    pub fn into_arc(self) -> Option<Arc<T>> {
        let ptr = self.link.get_ref();
        let ret = ptr_to_arc(ptr);
        let _ = ManuallyDrop::new(self);
        ret
    }

    /// check if the rcu cell is empty
    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    /// write an option arc value to the rcu cell and return the old value
    #[inline]
    pub fn set(&self, data: Option<Arc<T>>) -> Option<Arc<T>> {
        let new_ptr = match data {
            Some(data) => Arc::into_raw(data),
            None => ptr::null_mut(),
        };
        ptr_to_arc(self.link.update(new_ptr))
    }

    /// take the value from the rcu cell, leave the rcu cell empty
    #[inline]
    pub fn take(&self) -> Option<Arc<T>> {
        self.set(None)
    }

    /// write a value to the rcu cell and return the old value
    #[inline]
    pub fn write(&self, data: impl Into<Arc<T>>) -> Option<Arc<T>> {
        let data = data.into();
        self.set(Some(data))
    }

    /// Atomicly update the value with a closure and return the old value.
    /// The closure will be called with the old value and return the new value.
    /// The closure should not take too long time, internally it's use a spin
    /// lock to prevent other writer to update the value
    pub fn update<R, F>(&self, f: F) -> Option<Arc<T>>
    where
        F: FnOnce(Option<Arc<T>>) -> Option<R>,
        R: Into<Arc<T>>,
    {
        // increase ref count to lock the inner Arc
        let ptr = self.link.lock_read();
        let old_value = ptr_to_arc(ptr);
        let new_ptr = match f(old_value.clone()) {
            Some(data) => Arc::into_raw(data.into()),
            None => ptr::null_mut(),
        };
        self.link.unlock_update(new_ptr);
        old_value
    }

    /// read out the inner Arc value
    #[inline]
    pub fn read(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_arc(ptr));
        let cloned = v.as_ref().cloned();
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// read inner ptr and check if it is the same as the given Arc
    #[inline]
    pub fn arc_eq(&self, data: &Arc<T>) -> bool {
        self.link.get_ref() == Arc::as_ptr(data)
    }

    /// check if two RcuCell instances point to the same inner Arc
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.link.get_ref() == other.link.get_ref()
    }
}

//---------------------------------------------------------------------------------------
// RcuWeak
//---------------------------------------------------------------------------------------

/// RCU weak cell, it behaves like `RwLock<Option<Weak<T>>>`
#[derive(Debug)]
pub struct RcuWeak<T> {
    link: LinkWrapper<T>,
}

unsafe impl<T: Send + Sync> Send for RcuWeak<T> {}
unsafe impl<T: Send + Sync> Sync for RcuWeak<T> {}

impl<T> Drop for RcuWeak<T> {
    fn drop(&mut self) {
        let ptr = self.link.get_ref();
        let _ = ptr_to_weak(ptr);
    }
}

impl<T> Default for RcuWeak<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> From<Weak<T>> for RcuWeak<T> {
    fn from(data: Weak<T>) -> Self {
        let weak_ptr = Weak::into_raw(data);
        RcuWeak {
            link: LinkWrapper::new(weak_ptr),
        }
    }
}

impl<T> RcuWeak<T> {
    const DUMMY_WEAK: Weak<T> = Weak::new();
    /// create an dummy rcu weak cell instance, upgrade from it will return None
    #[inline]
    pub const fn new() -> Self {
        RcuWeak {
            link: LinkWrapper::new(ptr::null()),
        }
    }

    /// write a new weak value to the rcu weak cell and return the old value
    #[inline]
    pub fn write(&self, data: Weak<T>) -> Weak<T> {
        let new_ptr = if data.ptr_eq(&Self::DUMMY_WEAK) {
            ptr::null()
        } else {
            Weak::into_raw(data)
        };
        ptr_to_weak(self.link.update(new_ptr))
    }

    /// write a new `Weak` value downgrade from the `Arc`` to the cell and return the old value
    #[inline]
    pub fn write_arc(&self, data: &Arc<T>) -> Weak<T> {
        let weak = Arc::downgrade(data);
        let new_ptr = Weak::into_raw(weak);
        ptr_to_weak(self.link.update(new_ptr))
    }

    /// read out the inner weak value
    #[inline]
    pub fn read(&self) -> Weak<T> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = (*v).clone();
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// upgrade the innner weak value to an Arc value
    #[inline]
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.upgrade();
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// read inner ptr and check if it is the same as the given Arc
    #[inline]
    pub fn arc_eq(&self, data: &Arc<T>) -> bool {
        self.link.get_ref() == Arc::as_ptr(data)
    }

    /// read inner ptr and check if it is the same as the given Weak
    #[inline]
    pub fn weak_eq(&self, data: &Weak<T>) -> bool {
        self.link.get_ref() == Weak::as_ptr(data)
    }

    /// check if two RcuWeak instances point to the same inner Weak
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.link.get_ref() == other.link.get_ref()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::sync::Arc;

    #[test]
    fn test_default() {
        let x = RcuCell::<u32>::default();
        assert!(x.read().is_none());
    }

    #[test]
    fn simple_drop() {
        static REF: AtomicUsize = AtomicUsize::new(0);
        struct Foo(usize);
        impl Foo {
            fn new(data: usize) -> Self {
                REF.fetch_add(data, Ordering::Relaxed);
                Foo(data)
            }
        }
        impl Drop for Foo {
            fn drop(&mut self) {
                REF.fetch_sub(self.0, Ordering::Relaxed);
            }
        }
        let a = RcuCell::new(Foo::new(10));
        let b = a.read().unwrap();
        assert_eq!(REF.load(Ordering::Relaxed), 10);
        drop(b);
        assert_eq!(REF.load(Ordering::Relaxed), 10);
        drop(a);
        assert_eq!(REF.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn single_thread() {
        let t = RcuCell::new(Some(10));
        let x = t.read();
        let y = t.read();
        t.take();
        let z = t.read();
        let a = z.clone();
        drop(t); // t can be dropped before reader
        assert_eq!(x.map(|v| *v), Some(10));
        assert_eq!(y.map(|v| *v), Some(10));
        assert_eq!(z.map(|v| *v), None);
        assert_eq!(a.map(|v| *v), None);
    }

    #[test]
    fn single_thread_clone() {
        let t = Arc::new(RcuCell::new(Some(10)));
        let t1 = t.clone();
        assert_eq!(t1.read().map(|v| *v), Some(10));
        t1.write(5);
        assert_eq!(t.read().map(|v| *v), Some(5));
    }

    #[test]
    fn test_rcu_update() {
        let t = RcuCell::new(Some(10));
        let old = t.update(|v| v.map(|x| *x + 1));
        assert_eq!(t.read().map(|v| *v), Some(11));
        assert_eq!(old.map(|v| *v), Some(10));
        let old = t.update(|v| match v {
            Some(x) if *x == 11 => None,
            _ => Some(42),
        });
        assert!(t.read().is_none());
        assert_eq!(old.map(|v| *v), Some(11));
    }

    #[test]
    fn test_is_none() {
        let t = RcuCell::new(10);
        assert!(!t.is_none());
        t.take();
        assert!(t.is_none());
    }

    #[test]
    fn test_clone_rcu_cell() {
        let t = Arc::new(RcuCell::new(Some(10)));
        let t1 = t.clone();
        let t2 = t.clone();
        let t3 = t.clone();
        t1.write(11);
        drop(t1);
        assert_eq!(t.read().map(|v| *v), Some(11));
        t2.write(12);
        drop(t2);
        assert_eq!(t.read().map(|v| *v), Some(12));
        t3.write(13);
        drop(t3);
        assert_eq!(t.read().map(|v| *v), Some(13));
    }

    #[test]
    fn test_rcu_reader() {
        let t = Arc::new(RcuCell::new(10));
        let t1 = t.clone();
        let t2 = t.clone();
        let t3 = t.clone();
        let d1 = t1.read().unwrap();
        let d3 = t3.read().unwrap();
        t1.write(11);
        let d2 = t2.read().unwrap();
        assert_ne!(d1, d2);
        assert_eq!(d1, d3);
        assert_ne!(d2, d3);
    }

    #[test]
    fn test_rcu_take() {
        let t = Arc::new(RcuCell::new(10));
        let t1 = t.clone();
        let t2 = t.clone();
        let d1 = t1.take().unwrap();
        assert_eq!(*d1, 10);
        assert_eq!(t1.read(), None);
        let d2 = t2.write(42);
        assert!(d2.is_none());
        let d3 = t2.read().unwrap();
        assert_eq!(*d3, 42);
    }

    #[test]
    fn test_arc_eq() {
        let t = RcuCell::new(10);
        let v = t.read().unwrap();
        assert!(t.arc_eq(&v));
        t.write(11);
        assert!(!t.arc_eq(&v));
        let t1 = RcuCell::from(v.clone());
        assert!(t1.arc_eq(&v));
        let v2 = t.write(v);
        let t2 = RcuCell::from(v2.clone());
        assert!(RcuCell::ptr_eq(&t, &t1));
        assert!(t2.arc_eq(v2.as_ref().unwrap()));
    }
}
