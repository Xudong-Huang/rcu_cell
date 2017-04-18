#![feature(shared)]

use std::ops::Deref;
use std::ptr::Shared;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

#[derive(Debug)]
struct RcuInner<T> {
    refs: AtomicUsize,
    data: T,
}

unsafe impl<T: Send> Send for RcuInner<T> {}
unsafe impl<T: Send> Sync for RcuInner<T> {}


struct Link<T> {
    // AtomicPtr not support ?Sized
    // cause the RcuCell can't support ?Sized
    ptr: AtomicPtr<T>,
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

impl<T> Deref for Link<T> {
    type Target = AtomicPtr<T>;

    #[inline]
    fn deref(&self) -> &AtomicPtr<T> {
        &self.ptr
    }
}

impl<T> Link<RcuInner<T>> {
    #[inline]
    fn get(&self) -> RcuReader<T> {
        let ptr = self.load(Ordering::Acquire);
        unsafe {
            (*ptr).add_ref();
            RcuReader { inner: Shared::new(ptr) }
        }
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
        RcuCell { link: Link { ptr: AtomicPtr::new(Box::into_raw(data)) } }
    }

    pub fn read(&self) -> RcuReader<T> {
        self.link.get()
    }

    pub fn update(&self, data: T) {
        let data = Box::new(RcuInner::new(data));
        let old = self.link.swap(Box::into_raw(data), Ordering::AcqRel);

        // release the old data
        unsafe {
            (*old).add_ref();
            let d = RcuReader { inner: Shared::new(old) };
            d.unlink();
        }
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
