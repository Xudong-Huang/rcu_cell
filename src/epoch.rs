use core::{ops::Deref, sync::atomic::Ordering};
use crossbeam_epoch::{Atomic, Guard, Owned, Shared};

pub struct RcuCell<T> {
    data: Atomic<T>,
}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        drop(self.take());
        for _ in 0..128 {
            crossbeam_epoch::pin().flush();
        }
    }
}

impl<T> Default for RcuCell<T> {
    fn default() -> Self {
        Self::none()
    }
}

impl<T> RcuCell<T> {
    pub const fn none() -> Self {
        RcuCell {
            data: Atomic::null(),
        }
    }

    pub fn some(data: T) -> Self {
        RcuCell {
            data: Atomic::new(data),
        }
    }

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
        let guard = crossbeam_epoch::pin();
        self.data.load(Ordering::Acquire, &guard).is_null()
    }

    #[inline]
    fn inner_update(&self, data: Option<T>) -> Option<RcuReader<T>> {
        let guard = crossbeam_epoch::pin();
        let new_ptr = match data {
            Some(data) => Owned::new(data).into_shared(&guard),
            None => Shared::null(),
        };

        let old = self.data.swap(new_ptr, Ordering::AcqRel, &guard);
        if old.is_null() {
            None
        } else {
            let ptr = old.as_raw();
            unsafe { guard.defer_destroy(old) };
            Some(RcuReader { _guard: guard, ptr })
        }
    }

    /// take the value from the rcu cell
    #[inline]
    pub fn take(&self) -> Option<RcuReader<T>> {
        self.inner_update(None)
    }

    /// write a value to the rcu cell and return the old value
    #[inline]
    pub fn write(&self, data: T) -> Option<RcuReader<T>> {
        self.inner_update(Some(data))
    }

    /// update the value with a closure and return the old value
    pub fn update<F>(&self, f: F) -> Option<RcuReader<T>>
    where
        F: FnOnce(&T) -> T,
    {
        let v = self.read();
        let data = v.as_ref().map(|v| f(v))?;
        self.write(data)
    }

    /// read out the inner Arc value
    #[inline]
    pub fn read(&self) -> Option<RcuReader<T>> {
        let guard = crossbeam_epoch::pin();
        let ptr = self.data.load(Ordering::Acquire, &guard).as_raw();
        if ptr.is_null() {
            None
        } else {
            Some(RcuReader { _guard: guard, ptr })
        }
    }
}

#[derive(Debug)]
pub struct RcuReader<T> {
    // hold the guard to ensure the data is valid
    _guard: Guard,
    ptr: *const T,
}

impl<T> PartialEq for RcuReader<T> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T> Deref for RcuReader<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the guard ensures the data is valid
        unsafe { self.ptr.as_ref().unwrap() }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;

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
        drop(t); // t can be dropped before reader
        assert_eq!(x.map(|v| *v), Some(10));
        assert_eq!(y.map(|v| *v), Some(10));
        assert_eq!(z.map(|v| *v), None);
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
