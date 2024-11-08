#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use parking_lot::RwLock;

use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use core::{cmp, fmt, ptr};

//---------------------------------------------------------------------------------------
// RcuInner
//---------------------------------------------------------------------------------------
#[derive(Debug)]
struct RcuInner<T> {
    refs: AtomicUsize,
    data: T,
}

impl<T> RcuInner<T> {
    #[inline]
    fn new(data: T) -> Self {
        RcuInner {
            refs: AtomicUsize::new(1),
            data,
        }
    }

    #[inline]
    fn inc_ref(&self) -> usize {
        self.refs.fetch_add(1, Ordering::Release)
    }

    #[inline]
    fn dec_ref(&self) -> usize {
        self.refs.fetch_sub(1, Ordering::Release) - 1
    }
}

//---------------------------------------------------------------------------------------
// LinkWrapper
//---------------------------------------------------------------------------------------
struct LinkWrapper<T> {
    ptr: AtomicPtr<RcuInner<T>>,
}

impl<T> Deref for LinkWrapper<T> {
    type Target = AtomicPtr<RcuInner<T>>;

    fn deref(&self) -> &AtomicPtr<RcuInner<T>> {
        &self.ptr
    }
}

impl<T> LinkWrapper<T> {
    #[inline]
    const fn new(ptr: *mut RcuInner<T>) -> Self {
        LinkWrapper {
            ptr: AtomicPtr::new(ptr),
        }
    }

    #[inline]
    fn is_none(&self) -> bool {
        self.load(Ordering::Acquire).is_null()
    }

    #[inline]
    fn get_inner(&self) -> Option<NonNull<RcuInner<T>>> {
        let ptr = self.load(Ordering::Acquire);
        NonNull::new(ptr)
    }
}

impl<T> Drop for LinkWrapper<T> {
    fn drop(&mut self) {
        let ptr = self.load(Ordering::Acquire);
        if let Some(inner) = NonNull::new(ptr) {
            drop(RcuReader { inner })
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.get_inner();
        f.debug_struct("Link").field("inner", &inner).finish()
    }
}

//---------------------------------------------------------------------------------------
// RcuReader
//---------------------------------------------------------------------------------------
pub struct RcuReader<T> {
    inner: NonNull<RcuInner<T>>,
}

unsafe impl<T: Send> Send for RcuReader<T> {}
unsafe impl<T: Sync> Sync for RcuReader<T> {}

impl<T> Drop for RcuReader<T> {
    #[inline]
    fn drop(&mut self) {
        let inner = unsafe { self.inner.as_mut() };
        if inner.dec_ref() == 0 {
            core::sync::atomic::fence(Ordering::Acquire);
            // drop the inner box
            let _ = unsafe { Box::from_raw(inner) };
        }
    }
}

impl<T> Deref for RcuReader<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { &self.inner.as_ref().data }
    }
}

impl<T> AsRef<T> for RcuReader<T> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

impl<T> Clone for RcuReader<T> {
    fn clone(&self) -> Self {
        let cnt = unsafe { self.inner.as_ref().inc_ref() };
        assert!(cnt > 0);
        RcuReader { inner: self.inner }
    }
}

impl<T: PartialEq> PartialEq for RcuReader<T> {
    fn eq(&self, other: &RcuReader<T>) -> bool {
        *(*self) == *(*other)
    }
}

impl<T: PartialOrd> PartialOrd for RcuReader<T> {
    fn partial_cmp(&self, other: &RcuReader<T>) -> Option<cmp::Ordering> {
        (**self).partial_cmp(&**other)
    }

    fn lt(&self, other: &RcuReader<T>) -> bool {
        *(*self) < *(*other)
    }

    fn le(&self, other: &RcuReader<T>) -> bool {
        *(*self) <= *(*other)
    }

    fn gt(&self, other: &RcuReader<T>) -> bool {
        *(*self) > *(*other)
    }

    fn ge(&self, other: &RcuReader<T>) -> bool {
        *(*self) >= *(*other)
    }
}

impl<T: Ord> Ord for RcuReader<T> {
    fn cmp(&self, other: &RcuReader<T>) -> cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl<T: Eq> Eq for RcuReader<T> {}

impl<T: fmt::Debug> fmt::Debug for RcuReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T> fmt::Pointer for RcuReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Pointer::fmt(&(&**self as *const T), f)
    }
}

//---------------------------------------------------------------------------------------
// RcuCell
//---------------------------------------------------------------------------------------
#[derive(Debug)]
pub struct RcuCell<T> {
    link: LinkWrapper<T>,
    ptr_lock: RwLock<()>,
}

unsafe impl<T: Send> Send for RcuCell<T> {}
unsafe impl<T: Sync> Sync for RcuCell<T> {}

impl<T> Default for RcuCell<T> {
    fn default() -> Self {
        RcuCell::new(None)
    }
}

impl<T> RcuCell<T> {
    /// create an empty instance
    pub const fn none() -> Self {
        RcuCell {
            link: LinkWrapper::new(ptr::null_mut()),
            ptr_lock: RwLock::new(()),
        }
    }

    /// create from a value
    pub fn some(data: T) -> Self {
        let data = Box::new(RcuInner::new(data));
        let ptr = Box::into_raw(data);
        RcuCell {
            link: LinkWrapper::new(ptr),
            ptr_lock: RwLock::new(()),
        }
    }

    /// create from an option
    pub fn new(data: impl Into<Option<T>>) -> Self {
        let data = data.into();
        let ptr = match data {
            Some(data) => {
                let data = Box::new(RcuInner::new(data));
                Box::into_raw(data)
            }
            None => ptr::null_mut(),
        };

        RcuCell {
            link: LinkWrapper::new(ptr),
            ptr_lock: RwLock::new(()),
        }
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    fn inner_update(&self, data: Option<T>) -> Option<RcuReader<T>> {
        let new = match data {
            Some(v) => {
                let data = Box::new(RcuInner::new(v));
                Box::into_raw(data)
            }
            None => ptr::null_mut(),
        };

        let w_lock = self.ptr_lock.write();
        let old = self.link.swap(new, Ordering::AcqRel);
        drop(w_lock);

        let old_link = NonNull::new(old);
        old_link.map(|inner| RcuReader::<T> { inner })
    }

    pub fn take(&self) -> Option<RcuReader<T>> {
        self.inner_update(None)
    }

    pub fn write(&self, data: T) -> Option<RcuReader<T>> {
        self.inner_update(Some(data))
    }

    pub fn update<F>(&self, f: F) -> Option<RcuReader<T>>
    where
        F: FnOnce(&T) -> T,
    {
        let v = self.read();
        let data = v.as_ref().map(|v| f(v));
        self.inner_update(data)
    }

    pub fn read(&self) -> Option<RcuReader<T>> {
        let r_lock = self.ptr_lock.read();

        let ret = self.link.get_inner().map(|inner| {
            // we are sure that the data is still in memroy with the read lock
            unsafe { inner.as_ref().inc_ref() };
            RcuReader { inner }
        });

        drop(r_lock);
        ret
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
