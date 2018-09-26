use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct RcuInner<T> {
    refs: AtomicUsize,
    data: T,
}

#[derive(Debug)]
struct Link<T> {
    ptr: AtomicUsize,
    phantom: PhantomData<*mut T>,
}

#[derive(Debug)]
pub struct RcuCell<T> {
    link: Arc<Link<RcuInner<T>>>,
}

unsafe impl<T> Send for RcuCell<T> {}
unsafe impl<T> Sync for RcuCell<T> {}

pub struct RcuReader<T> {
    inner: NonNull<RcuInner<T>>,
}

unsafe impl<T: Send> Send for RcuReader<T> {}
unsafe impl<T: Sync> Sync for RcuReader<T> {}

#[derive(Debug)]
pub struct RcuGuard<T> {
    link: Arc<Link<RcuInner<T>>>,
}

unsafe impl<T: Send> Send for RcuGuard<T> {}
unsafe impl<T: Sync> Sync for RcuGuard<T> {}

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
        let ptr = ptr & !1;
        if ptr == 0 {
            return true;
        }
        false
    }

    #[inline]
    fn is_locked(&self) -> bool {
        let ptr = self.ptr.load(Ordering::Acquire);
        ptr & 1 == 1
    }

    #[inline]
    fn get(&self) -> Option<RcuReader<T>> {
        let ptr = self.ptr.load(Ordering::Acquire);
        self._conv(ptr).map(|ptr| {
            ptr.inc_ref();
            RcuReader {
                inner: NonNull::new(ptr as *const _ as *mut _).expect("null shared"),
            }
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
            match self
                .ptr
                .compare_exchange(old, new, Ordering::AcqRel, Ordering::Relaxed)
            {
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
            match self
                .ptr
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed)
            {
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
    fn inc_ref(&self) {
        self.refs.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn dec_ref(&self) -> usize {
        let ret = self.refs.fetch_sub(1, Ordering::Relaxed);
        ret - 1
    }
}

impl<T> Drop for RcuReader<T> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            if self.inner.as_ref().dec_ref() == 0 {
                // drop the inner box
                let _: Box<RcuInner<T>> = Box::from_raw(self.inner.as_ptr());
            }
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

impl<T> Clone for RcuReader<T> {
    fn clone(&self) -> Self {
        unsafe {
            self.inner.as_ref().inc_ref();
        }
        RcuReader { inner: self.inner }
    }
}

impl<T> RcuReader<T> {
    #[inline]
    fn inc_link(&self) {
        unsafe {
            self.inner.as_ref().inc_ref();
        }
    }

    #[inline]
    fn dec_link(&self) {
        unsafe {
            self.inner.as_ref().dec_ref();
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
            link: Arc::new(Link {
                ptr: AtomicUsize::new(ptr),
                phantom: PhantomData,
            }),
        }
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.link.is_locked()
    }

    pub fn read(&self) -> Option<RcuReader<T>> {
        self.link.get()
    }

    pub fn try_lock(&self) -> Option<RcuGuard<T>> {
        if self.link.acquire() {
            return Some(RcuGuard {
                link: self.link.clone(),
            });
        }
        None
    }
}

impl<T> Clone for RcuCell<T> {
    fn clone(&self) -> Self {
        // we need to inc ref for the underlining data
        let link = self.link.clone();
        link.get().map(|d| d.inc_link());
        Self { link }
    }
}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        self.link.get().map(|d| d.dec_link());
    }
}

impl<T> RcuGuard<T> {
    // update the RcuCell with a value
    pub fn update(&mut self, data: Option<T>) {
        // the RcuCell is acquired now
        let old_link = self.link.swap(data);
        if let Some(old) = old_link {
            old.inc_ref();
            let ptr = NonNull::new(old as *const _ as *mut _).expect("null Shared");
            let d = RcuReader::<T> { inner: ptr };
            d.dec_link();
        }
    }

    pub fn as_mut(&mut self) -> Option<&mut T> {
        // since it's locked and it's safe to update the data
        // ignore the reserve bit
        let ptr = self.link.ptr.load(Ordering::Relaxed) & !1;
        if ptr == 0 {
            return None;
        }
        let inner = unsafe { &mut *(ptr as *mut RcuInner<T>) };
        Some(&mut inner.data)
    }

    pub fn as_ref(&self) -> Option<&T> {
        // it's safe the get the ref since locked
        // ignore the reserve bit
        let ptr = self.link.ptr.load(Ordering::Relaxed) & !1;
        if ptr == 0 {
            return None;
        }
        let inner = unsafe { &*(ptr as *const RcuInner<T>) };
        Some(&inner.data)
    }
}

impl<T> Drop for RcuGuard<T> {
    fn drop(&mut self) {
        self.link.release();
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
    fn single_thread_arc() {
        use std::sync::Arc;

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
        assert_eq!(t.try_lock().is_none(), true);
        drop(g);
        assert_eq!(t.read().map(|v| *v), Some(11));
    }

    #[test]
    fn test_is_none() {
        let t = RcuCell::new(Some(10));
        assert_eq!(t.is_none(), false);
        t.try_lock().unwrap().update(None);
        assert_eq!(t.is_none(), true);
    }

    #[test]
    fn test_is_locked() {
        let t = RcuCell::new(Some(10));
        assert_eq!(t.is_locked(), false);
        let mut g = t.try_lock().unwrap();
        g.update(None);
        assert_eq!(t.is_locked(), true);
        drop(g);
        assert_eq!(t.is_locked(), false);
    }

    #[test]
    fn test_as_mut() {
        let t = RcuCell::new(Some(10));
        let mut g = t.try_lock().unwrap();
        assert_eq!(g.as_ref(), Some(&10));
        // change the internal data with lock
        g.as_mut().map(|d| *d = 20);
        drop(g);
        let x = t.read().unwrap();
        assert_eq!(*x, 20);
    }

    #[test]
    fn test_clone_rcu_cell() {
        let t = RcuCell::new(Some(10));
        let t1 = t.clone();
        let t2 = t.clone();
        let t3 = t.clone();
        t1.try_lock().unwrap().as_mut().map(|d| *d = 11);
        drop(t1);
        assert_eq!(t.read().map(|v| *v), Some(11));
        t2.try_lock().unwrap().as_mut().map(|d| *d = 12);
        drop(t2);
        assert_eq!(t.read().map(|v| *v), Some(12));
        t3.try_lock().unwrap().as_mut().map(|d| *d = 13);
        drop(t3);
        assert_eq!(t.read().map(|v| *v), Some(13));
    }
}
