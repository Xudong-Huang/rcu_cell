#![doc = include_str!("../README.md")]
#![no_std]

extern crate alloc;

mod link;
mod rcu_cell;
mod rcu_weak;

pub use rcu_cell::RcuCell;
pub use rcu_weak::RcuWeak;

// we only support 64-bit platform
const _: () = assert!(usize::MAX.count_ones() == 64);
const _: () = assert!(core::mem::size_of::<*const ()>() == 8);

use alloc::sync::Arc;

pub trait ArcPointer<T> {
    fn as_ptr(&self) -> *const T;
    fn into_raw(self) -> *const T;
    /// # Safety
    /// you must ensure the pointer is valid
    unsafe fn from_raw(ptr: *const T) -> Self;
}

impl<T> ArcPointer<T> for Option<Arc<T>> {
    fn as_ptr(&self) -> *const T {
        match self {
            Some(v) => Arc::as_ptr(v),
            None => core::ptr::null(),
        }
    }

    fn into_raw(self) -> *const T {
        match self {
            Some(v) => Arc::into_raw(v),
            None => core::ptr::null(),
        }
    }

    unsafe fn from_raw(ptr: *const T) -> Self {
        (!ptr.is_null()).then(|| Arc::from_raw(ptr))
    }
}

#[cfg(test)]
mod test {
    use super::RcuCell;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn cas_test() {
        use super::ArcPointer;
        use Ordering::SeqCst;

        let a = RcuCell::new(1234);

        let curr = a.read().as_ptr();
        let res1 = unsafe { a.compare_exchange(curr, None, SeqCst, SeqCst) }.unwrap();
        assert_eq!(res1, curr);
        assert!(a.is_none());
        let res2 = unsafe { a.compare_exchange(res1, Some(&Arc::new(5678)), SeqCst, SeqCst) };
        assert!(res2.is_err());

        let null = core::ptr::null();
        let res2 = unsafe { a.compare_exchange(null, Some(&Arc::new(5678)), SeqCst, SeqCst) };
        assert!(res2.is_ok());
        assert_eq!(a.read().map(|v| *v), Some(5678));
    }
}
