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
    #[inline]
    fn get(&self) -> RcuReader<T> {
        let ptr = self.ptr.load(Ordering::Acquire);
        // clear the reserve bit
        let ptr = (ptr & !1) as *mut RcuInner<T>;
        unsafe {
            (*ptr).add_ref();
            RcuReader { inner: Shared::new(ptr) }
        }
    }

    #[inline]
    fn swap(&self, data: T) -> &RcuInner<T> {
        // we can sure that the update is
        // only possible after get the guard
        // in which the reserve bit must be true
        let data = Box::new(RcuInner::new(data));
        let new = Box::into_raw(data) as usize | 1;
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

        // let old = self.ptr.swap(new, Ordering::AcqRel);
        let old = (old & !1) as *const RcuInner<T>;
        unsafe { &*old }
    }

    // only one thread can acquire the link successfully
    fn acquire(&self) -> bool {
        let mut old = self.ptr.load(Ordering::Acquire);
        if old & 1 != 0 {
            return false;
        }

        loop {
            let new = old | 1;
            println!("old={:x}, new={:x}", old, new);
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
    pub fn new(data: T) -> Self {
        let data = Box::new(RcuInner::new(data));
        RcuCell {
            link: Link {
                ptr: AtomicUsize::new(Box::into_raw(data) as usize),
                phantom: PhantomData,
            },
        }
    }

    pub fn read(&self) -> RcuReader<T> {
        self.link.get()
    }

    // only work after get the guard
    fn update(&self, data: T) {
        let old = self.link.swap(data);
        old.add_ref();
        let d = RcuReader { inner: unsafe { Shared::new(old) } };
        d.unlink();
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
        let d = self.link.get();
        d.unlink();
    }
}

impl<'a, T> RcuGuard<'a, T> {
    pub fn update(&mut self, data: T) {
        // the RcuCell is acquired now
        self.inner.update(data);
    }
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
        let _ = RcuCell::new(10);
    }

    #[test]
    fn single_thread() {
        let t = RcuCell::new(10);
        let x = t.read();
        let y = t.read();
        t.update(5);
        let z = t.read();
        let a = z.clone();
        assert!(*x == 10);
        assert!(*y == 10);
        assert!(*z == 5);
        assert!(*a == 5);
    }

    #[test]
    fn single_thread_arc() {
        use std::sync::Arc;

        let t = Arc::new(RcuCell::new(10));
        let t1 = t.clone();
        assert!(*t1.read() == 10);
        t1.update(5);
        assert!(*t.read() == 5);
    }

    #[test]
    fn test_rcu_guard() {
        let t = RcuCell::new(10);
        let x = t.read();
        let mut g = t.acquire().unwrap();
        let y = *x + 1;
        g.update(y);
        assert_eq!(t.acquire().is_none(), true);
        drop(g);
        assert_eq!(*t.read(), 11);
    }
}
