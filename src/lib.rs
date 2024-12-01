#![doc = include_str!("../README.md")]
#![no_std]

pub mod epoch;
pub mod list;
pub mod seize;

extern crate alloc;
use alloc::sync::Arc;

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{fmt, ptr};

#[cfg(target_pointer_width = "64")]
const LEADING_BITS: usize = 8;
#[cfg(target_pointer_width = "64")]
const ALIGN_BITS: usize = 3;
#[cfg(not(target_pointer_width = "64"))]
const LEADING_BITS: usize = 0;
#[cfg(not(target_pointer_width = "64"))]
const ALIGN_BITS: usize = 2;

const LOWER_MASK: usize = (1 << ALIGN_BITS) - 1;
const HIGHER_MASK: usize = !((1 << (usize::MAX.leading_ones() as usize - LEADING_BITS)) - 1);
const REFCOUNT_MASK: usize = (1 << (LEADING_BITS + ALIGN_BITS)) - 1;

//---------------------------------------------------------------------------------------
// LinkWrapper
//---------------------------------------------------------------------------------------

#[repr(C)]
union Ptr<T> {
    addr: usize,
    ptr: *const T,
}

/// A wrapper of the pointer to the inner Arc data
struct LinkWrapper<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*const T>,
}

impl<T> LinkWrapper<T> {
    #[inline]
    const fn new(ptr: *const T) -> Self {
        let addr: usize = unsafe { Ptr { ptr }.addr };
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        LinkWrapper {
            ptr: AtomicUsize::new(addr << LEADING_BITS),
            phantom: PhantomData,
        }
    }

    fn update(&self, ptr: *const T) -> Option<Arc<T>> {
        let addr = unsafe { Ptr { ptr }.addr };
        debug_assert!(addr & LOWER_MASK == 0);
        debug_assert!(addr & HIGHER_MASK == 0);
        let new = addr << LEADING_BITS;
        let mut old = self.ptr.load(Ordering::Relaxed) & !REFCOUNT_MASK;

        while let Err(addr) =
            self.ptr
                .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
        {
            old = addr & !REFCOUNT_MASK;
            core::hint::spin_loop();
        }

        debug_assert!(old & LOWER_MASK == 0);
        debug_assert!(old & HIGHER_MASK == 0);
        let addr = old >> LEADING_BITS;
        let ptr = unsafe { Ptr { addr }.ptr };
        Self::ptr_to_arc(ptr)
    }

    #[inline]
    fn is_none(&self) -> bool {
        self.ptr.load(Ordering::Relaxed) & !REFCOUNT_MASK == 0
    }

    #[inline]
    fn ptr_to_arc(ptr: *const T) -> Option<Arc<T>> {
        (!ptr.is_null()).then(|| unsafe { Arc::from_raw(ptr) })
    }

    #[inline]
    fn inc_ref(&self) -> *const T {
        let addr = self.ptr.fetch_add(1, Ordering::Release);
        let refs = addr & REFCOUNT_MASK;
        assert!(refs < REFCOUNT_MASK, "Too many references");
        let addr = (addr & !REFCOUNT_MASK) >> LEADING_BITS;
        unsafe { Ptr { addr }.ptr }
    }

    #[inline]
    fn dec_ref(&self) {
        self.ptr.fetch_sub(1, Ordering::Release);
    }

    #[inline]
    fn clone_inner(&self) -> Option<Arc<T>> {
        let ptr = self.inc_ref();
        let ret = Self::ptr_to_arc(ptr);
        let _ = ManuallyDrop::new(ret.clone());
        self.dec_ref();
        ret
    }
}

impl<T: fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.clone_inner();
        f.debug_struct("Link").field("inner", &inner).finish()
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
        self.take();
    }
}

impl<T> Default for RcuCell<T> {
    fn default() -> Self {
        RcuCell::none()
    }
}

impl<T> RcuCell<T> {
    /// create an empty rcu cell instance
    pub const fn none() -> Self {
        RcuCell {
            link: LinkWrapper::new(ptr::null()),
        }
    }

    /// create rcu cell from a value
    pub fn some(data: T) -> Self {
        let ptr = Arc::into_raw(Arc::new(data));
        RcuCell {
            link: LinkWrapper::new(ptr),
        }
    }

    /// create rcu cell from value that can be converted to Option<T>
    pub fn new(data: impl Into<Option<T>>) -> Self {
        let data = data.into();
        match data {
            Some(data) => Self::some(data),
            None => Self::none(),
        }
    }

    /// check if the rcu cell is empty
    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    #[inline]
    fn inner_update(&self, data: Option<Arc<T>>) -> Option<Arc<T>> {
        let new_ptr = match data {
            Some(data) => Arc::into_raw(data),
            None => ptr::null_mut(),
        };
        self.link.update(new_ptr)
    }

    /// take the value from the rcu cell
    #[inline]
    pub fn take(&self) -> Option<Arc<T>> {
        self.inner_update(None)
    }

    /// write a value to the rcu cell and return the old value
    #[inline]
    pub fn write(&self, data: impl Into<Arc<T>>) -> Option<Arc<T>> {
        let data = data.into();
        self.inner_update(Some(data))
    }

    /// update the value with a closure and return the old value
    pub fn update<R, F>(&self, f: F) -> Option<Arc<T>>
    where
        F: FnOnce(&T) -> R,
        R: Into<Arc<T>>,
    {
        let v = self.read();
        let data = v.as_ref().map(|v| f(v))?;
        self.write(data)
    }

    /// read out the inner Arc value
    #[inline]
    pub fn read(&self) -> Option<Arc<T>> {
        self.link.clone_inner()
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
        t.update(|v| v + 1);
        assert_eq!(t.read().map(|v| *v), Some(11));
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
}
