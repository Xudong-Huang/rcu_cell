#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{cmp, fmt};

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
    ptr: AtomicUsize,
    phantom: PhantomData<*const RcuInner<T>>,
}

impl<T> Deref for LinkWrapper<T> {
    type Target = AtomicUsize;

    fn deref(&self) -> &AtomicUsize {
        &self.ptr
    }
}

impl<T> LinkWrapper<T> {
    // convert from usize to ptr
    #[inline]
    fn conv(ptr: usize) -> Option<NonNull<RcuInner<T>>> {
        // ignore the reserve bit and read bit
        let ptr = ptr & !3;
        NonNull::new(ptr as *mut RcuInner<T>)
    }

    #[inline]
    fn is_none(&self) -> bool {
        self.load(Ordering::Acquire) & !3 == 0
    }

    #[inline]
    fn get(&self) -> Option<RcuReader<T>> {
        let ptr = self.read_lock();

        let ret = Self::conv(ptr).map(|inner| {
            // we are sure that the data is still in memroy with the read lock
            unsafe { inner.as_ref().inc_ref() };
            RcuReader { inner }
        });

        self.read_unlock();
        ret
    }

    #[inline]
    fn swap(&self, data: Option<T>) -> Option<NonNull<RcuInner<T>>> {
        // we can sure that the update is
        // only possible after get the guard
        // in which case the reserve bit must be set
        let new = match data {
            Some(v) => {
                let data = Box::new(RcuInner::new(v));
                Box::into_raw(data) as usize | 1
            }
            None => 1,
        };

        // should wait until there is no read for the ptr
        let mut old = self.load(Ordering::Acquire) & !2;

        loop {
            match self.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(x) => {
                    old = x & !2;
                    core::hint::spin_loop();
                }
            }
        }

        Self::conv(old)
    }

    // only one thread can acquire the link successfully
    fn acquire(&self) -> bool {
        let mut old = self.load(Ordering::Acquire);
        if old & 1 != 0 {
            return false;
        }

        loop {
            let new = old | 1;
            match self.compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire) {
                // successfully reserved
                Ok(_) => return true,
                // only try again if old value is still false
                Err(x) if x & 1 == 0 => old = x,
                // otherwise return false, which means the link is reserved by others
                _ => return false,
            }
        }
    }

    // release only happened after acquire
    fn release(&self) {
        self.fetch_and(!1, Ordering::Release);
    }

    // only one thread can access the ptr when read/write
    // return the current value after read lock
    fn read_lock(&self) -> usize {
        let mut old = self.load(Ordering::Acquire) & !2;

        loop {
            let new = old | 2;
            match self.compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire) {
                // successfully reserved
                Ok(_) => return new,
                // otherwise the link is reserved by others, just spin wait
                Err(x) => {
                    old = x & !2;
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn read_unlock(&self) {
        self.fetch_and(!2, Ordering::Release);
    }
}

impl<T> Drop for LinkWrapper<T> {
    fn drop(&mut self) {
        let ptr = self.load(Ordering::Acquire);
        if let Some(inner) = Self::conv(ptr) {
            drop(RcuReader { inner })
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for LinkWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ptr = self.load(Ordering::Acquire);
        let inner = Self::conv(ptr);
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
// RcuGuard
//---------------------------------------------------------------------------------------
pub struct RcuGuard<'a, T: 'a> {
    link: &'a LinkWrapper<T>,
}

// unsafe impl<'a, T> !Send for RcuGuard<'a, T> {}
unsafe impl<T: Sync> Sync for RcuGuard<'_, T> {}

impl<T> RcuGuard<'_, T> {
    // update the RcuCell with a new value
    // this would not change the value that hold by readers
    pub fn update(&mut self, data: impl Into<Option<T>>) {
        let data = data.into();
        // the RcuCell is acquired now
        let old_link = self.link.swap(data);
        if let Some(inner) = old_link {
            // drop the old value as a RcuReader
            drop(RcuReader::<T> { inner });
        }
    }

    // get the mut ref of the underlying data
    // this would change the value that hold by readers so it's not safe
    // we can't safely update the data when still hold readers
    // the reader garantee that the data would not change you can read from them
    // pub unsafe fn as_mut(&mut self) -> Option<&mut T> {
    //     // since it's locked and it's safe to update the data
    //     // ignore the reserve bit
    //     let ptr = self.link.load(Ordering::Acquire) & !1;
    //     if ptr == 0 {
    //         return None;
    //     }
    //     let inner = { &mut *(ptr as *mut RcuInner<T>) };
    //     Some(&mut inner.data)
    // }

    pub fn as_ref(&self) -> Option<&T> {
        // it's safe to get the ref since locked
        // ignore the reserve bit
        let ptr = self.link.load(Ordering::Acquire);
        let link = LinkWrapper::conv(ptr);
        link.map(|p| unsafe { &p.as_ref().data })
    }
}

impl<T> Drop for RcuGuard<'_, T> {
    fn drop(&mut self) {
        self.link.release();
    }
}

impl<T: fmt::Debug> fmt::Debug for RcuGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.as_ref(), f)
    }
}

//---------------------------------------------------------------------------------------
// RcuCell
//---------------------------------------------------------------------------------------
#[derive(Debug)]
pub struct RcuCell<T> {
    link: LinkWrapper<T>,
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
            link: LinkWrapper {
                ptr: AtomicUsize::new(0),
                phantom: PhantomData,
            },
        }
    }

    /// create from a value
    pub fn some(data: T) -> Self {
        let data = Box::new(RcuInner::new(data));
        let ptr = Box::into_raw(data) as usize;
        RcuCell {
            link: LinkWrapper {
                ptr: AtomicUsize::new(ptr),
                phantom: PhantomData,
            },
        }
    }

    /// create from an option
    pub fn new(data: impl Into<Option<T>>) -> Self {
        let data = data.into();
        let ptr = match data {
            Some(data) => {
                let data = Box::new(RcuInner::new(data));
                Box::into_raw(data) as usize
            }
            None => 0,
        };

        RcuCell {
            link: LinkWrapper {
                ptr: AtomicUsize::new(ptr),
                phantom: PhantomData,
            },
        }
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    pub fn read(&self) -> Option<RcuReader<T>> {
        self.link.get()
    }

    pub fn try_lock(&self) -> Option<RcuGuard<T>> {
        self.link.acquire().then(|| RcuGuard { link: &self.link })
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
        t.try_lock().unwrap().update(None);
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
        assert!(t1.read().map(|v| *v) == Some(10));
        t1.try_lock().unwrap().update(Some(5));
        assert!(t.read().map(|v| *v) == Some(5));
    }

    #[test]
    fn test_rcu_guard() {
        let t = RcuCell::new(Some(10));
        let x = t.read().map(|v| *v);
        let mut g = t.try_lock().unwrap();
        let y = x.map(|v| v + 1);
        g.update(y);
        assert!(t.try_lock().is_none());
        drop(g);
        assert_eq!(t.read().map(|v| *v), Some(11));
    }

    #[test]
    fn test_is_none() {
        let t = RcuCell::new(10);
        assert!(!t.is_none());
        t.try_lock().unwrap().update(None);
        assert!(t.is_none());
    }

    #[test]
    fn test_clone_rcu_cell() {
        let t = Arc::new(RcuCell::new(Some(10)));
        let t1 = t.clone();
        let t2 = t.clone();
        let t3 = t.clone();
        t1.try_lock().unwrap().update(Some(11));
        drop(t1);
        assert_eq!(t.read().map(|v| *v), Some(11));
        t2.try_lock().unwrap().update(Some(12));
        drop(t2);
        assert_eq!(t.read().map(|v| *v), Some(12));
        t3.try_lock().unwrap().update(Some(13));
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
        let mut g = t1.try_lock().unwrap();
        g.update(11);
        drop(g);
        let d2 = t2.read().unwrap();
        assert_ne!(d1, d2);
        assert_eq!(d1, d3);
        assert_ne!(d2, d3);
    }
}
