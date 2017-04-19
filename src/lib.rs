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


struct Link<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*mut T>,
}

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

    fn swap(&self, data: T) -> &RcuInner<T> {
        let data = Box::new(RcuInner::new(data));
        let new = Box::into_raw(data) as usize;
        let old = self.ptr.swap(new, Ordering::AcqRel);
        let old = (old & !1) as *const RcuInner<T>;
        unsafe { &*old }
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
    #[inline]
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

    pub fn update(&self, data: T) {
        let old = self.link.swap(data);
        old.add_ref();
        let d = RcuReader { inner: unsafe { Shared::new(old) } };
        d.unlink();
    }
}

impl<T> Drop for RcuCell<T> {
    #[inline]
    fn drop(&mut self) {
        let d = self.link.get();
        d.unlink();
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
}
