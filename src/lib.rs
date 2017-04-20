#![feature(shared)]

use std::ops::Deref;
use std::ptr::Shared;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct RcuInner<T> {
    refs: AtomicUsize,
    data: T,
}

unsafe impl<T: Send> Send for RcuInner<T> {}
unsafe impl<T: Send> Sync for RcuInner<T> {}

#[derive(Debug)]
struct Link<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*mut T>,
}

#[derive(Debug)]
pub struct RcuCell<T> {
    link: Link<RcuInner<T>>,
}

unsafe impl<T: Send> Send for RcuCell<T> {}
unsafe impl<T: Send> Sync for RcuCell<T> {}

pub struct RcuReader<T> {
    inner: Shared<RcuInner<T>>,
}

unsafe impl<T: Send> Send for RcuReader<T> {}
unsafe impl<T: Send> Sync for RcuReader<T> {}

#[derive(Debug)]
pub struct RcuGuard<'a, T: 'a> {
    inner: &'a RcuCell<T>,
}

impl<T> Link<RcuInner<T>> {
    // convert from usize to ref
    #[inline]
    fn _conv(&self, ptr: usize) -> Option<&RcuInner<T>> {
        // ignore the reserve bit
        let ptr = ptr & !1;
        if ptr == 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const RcuInner<T>) })
    }

    #[inline]
    fn is_none(&self) -> bool {
        let ptr = self.ptr.load(Ordering::Acquire);
        self._conv(ptr).is_none()
    }

    #[inline]
    fn get(&self) -> Option<RcuReader<T>> {
        let ptr = self.ptr.load(Ordering::Acquire);
        self._conv(ptr)
            .map(|ptr| unsafe {
                     (*ptr).add_ref();
                     RcuReader { inner: Shared::new(ptr) }
                 })
    }

    #[inline]
    fn swap(&self, data: Option<T>) -> Option<&RcuInner<T>> {
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

        let mut old = self.ptr.load(Ordering::Acquire);

        loop {
            // should not change the reserve bit
            // if old & 1 == 1 {
            //     new |= 1;
            // } else {
            //     new &= !1;
            // }
            match self.ptr
                      .compare_exchange(old, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => break,
                Err(x) => old = x,
            }
        }

        self._conv(old)
    }

    // only one thread can acquire the link successfully
    fn acquire(&self) -> bool {
        let mut old = self.ptr.load(Ordering::Acquire);
        if old & 1 != 0 {
            return false;
        }

        loop {
            let new = old | 1;
            match self.ptr
                      .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed) {
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
        let ptr = self.ptr.load(Ordering::Acquire) & !1;
        self.ptr.store(ptr, Ordering::Release);
    }
}

impl<T> RcuInner<T> {
    #[inline]
    fn new(data: T) -> Self {
        RcuInner {
            refs: AtomicUsize::new(1),
            data: data,
        }

    }

    #[inline]
    fn add_ref(&self) {
        self.refs.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn release(&self) -> usize {
        let ret = self.refs.fetch_sub(1, Ordering::Relaxed);
        ret - 1
    }
}

impl<T> Drop for RcuReader<T> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            if (**self.inner).release() == 0 {
                // drop the inner box
                let _: Box<RcuInner<T>> = Box::from_raw(self.inner.as_mut_ptr());
            }
        }
    }
}

impl<T> Deref for RcuReader<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { &(**self.inner).data }
    }
}

impl<T> Clone for RcuReader<T> {
    fn clone(&self) -> Self {
        unsafe {
            &(**self.inner).add_ref();
        }
        RcuReader { inner: self.inner }
    }
}

impl<T> RcuReader<T> {
    #[inline]
    fn unlink(&self) {
        unsafe {
            (**self.inner).release();
        }
    }
}

impl<T> RcuCell<T> {
    pub fn new(data: Option<T>) -> Self {
        let ptr = match data {
            Some(data) => {
                let data = Box::new(RcuInner::new(data));
                Box::into_raw(data) as usize
            }
            None => 0,
        };

        RcuCell {
            link: Link {
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

    // only work after get the guard
    fn update(&self, data: Option<T>) {
        let old = self.link.swap(data);
        match old {
            Some(old) => {
                old.add_ref();
                let d = RcuReader { inner: unsafe { Shared::new(old) } };
                d.unlink();
            }
            _ => (),
        }
    }

    pub fn acquire(&self) -> Option<RcuGuard<T>> {
        if self.link.acquire() {
            return Some(RcuGuard { inner: self });
        }
        None
    }
}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        self.link.get().map(|d| d.unlink());
    }
}

impl<'a, T> RcuGuard<'a, T> {
    // update the RcuCell with a value
    pub fn update(&mut self, data: Option<T>) {
        // the RcuCell is acquired now
        self.inner.update(data);
    }

    // remove data from the RcuCell
    pub fn remove(&mut self) {}
}

impl<'a, T> Drop for RcuGuard<'a, T> {
    fn drop(&mut self) {
        self.inner.link.release();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn simple_drop() {
        let _ = RcuCell::new(Some(10));
    }

    #[test]
    fn single_thread() {
        let t = RcuCell::new(Some(10));
        let x = t.read();
        let y = t.read();
        t.update(None);
        let z = t.read();
        let a = z.clone();
        drop(t); // t can be dropped before reader
        assert_eq!(x.map(|v| *v), Some(10));
        assert_eq!(y.map(|v| *v), Some(10));
        assert_eq!(z.map(|v| *v), None);
        assert_eq!(a.map(|v| *v), None);
    }

    #[test]
    fn single_thread_arc() {
        use std::sync::Arc;

        let t = Arc::new(RcuCell::new(Some(10)));
        let t1 = t.clone();
        assert!(t1.read().map(|v| *v) == Some(10));
        t1.update(Some(5));
        assert!(t.read().map(|v| *v) == Some(5));
    }

    #[test]
    fn test_rcu_guard() {
        let t = RcuCell::new(Some(10));
        let x = t.read().map(|v| *v);
        let mut g = t.acquire().unwrap();
        let y = x.map(|v| v + 1);
        g.update(y);
        assert_eq!(t.acquire().is_none(), true);
        drop(g);
        assert_eq!(t.read().map(|v| *v), Some(11));
    }

    #[test]
    fn test_is_none() {
        let t = RcuCell::new(Some(10));
        assert_eq!(t.is_none(), false);
        t.acquire().unwrap().update(None);
        assert_eq!(t.is_none(), true);
    }
}
