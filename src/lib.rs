#![doc = include_str!("../README.md")]
#![no_std]
#![cfg_attr(rcu_cell_atomic_u128, feature(integer_atomics))]

extern crate alloc;

mod link;
mod rcu_cell;
mod rcu_weak;

pub use rcu_cell::RcuCell;
pub use rcu_weak::RcuWeak;

// the pointer and the tag bits are packed into one word, so only 64-bit platforms are supported
#[cfg(not(target_pointer_width = "64"))]
compile_error!("rcu_cell only supports 64-bit platforms");

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
    extern crate std;
    use super::RcuCell;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::panic::{catch_unwind, AssertUnwindSafe};

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

    // Miri gives every unsizing its own vtable, which the fallback exact vtable check rejects
    #[cfg(not(miri))]
    #[test]
    fn test_unsized() {
        trait B: Send + Sync {
            fn id(&self) -> u32;
        }
        struct A(u32);
        impl B for A {
            fn id(&self) -> u32 {
                self.0
            }
        }
        fn mk(v: u32) -> Arc<dyn B> {
            Arc::new(A(v))
        }
        let c: RcuCell<dyn B> = RcuCell::from(mk(7));
        assert_eq!(c.read().unwrap().id(), 7);
        let old = c.write(mk(8)).unwrap();
        assert_eq!(old.id(), 7);
        assert_eq!(c.read().unwrap().id(), 8);
        assert!(c.take().is_some());
        assert!(c.is_none());
        assert!(c.read().is_none());

        let s: RcuCell<[u8]> = RcuCell::from(Arc::<[u8]>::from(&[1u8, 2, 3][..]));
        assert_eq!(&*s.read().unwrap(), &[1, 2, 3]);
    }

    // Miri gives every unsizing its own vtable, which the fallback exact vtable check rejects
    #[cfg(not(miri))]
    #[test]
    fn test_unsized_weak() {
        use super::RcuWeak;
        use alloc::sync::Weak;
        trait B: Send + Sync {
            fn id(&self) -> u32;
        }
        struct A(u32);
        impl B for A {
            fn id(&self) -> u32 {
                self.0
            }
        }
        fn mk(v: u32) -> Arc<dyn B> {
            Arc::new(A(v))
        }
        let a1 = mk(1);
        let a2 = mk(2);
        let w: RcuWeak<dyn B> = RcuWeak::from(Arc::downgrade(&a1));
        assert_eq!(w.upgrade().unwrap().id(), 1);
        assert!(w.arc_eq(&a1));
        let old = w.write_arc(&a2);
        assert_eq!(old.upgrade().unwrap().id(), 1);
        assert_eq!(w.upgrade().unwrap().id(), 2);
        assert!(w.take().upgrade().is_some());
        assert!(w.upgrade().is_none());
        assert!(w.read().upgrade().is_none());
        w.write_arc(&a1);
        assert_eq!(w.upgrade().unwrap().id(), 1);
        drop(a1);
        assert!(w.upgrade().is_none());

        let dangling: Weak<dyn B> = Weak::<A>::new();
        let w2: RcuWeak<dyn B> = RcuWeak::from(dangling);
        assert!(w2.upgrade().is_none());
    }

    trait Id: Send + Sync {
        fn id(&self) -> u32;
    }
    struct Small(u32);
    struct Big(u32, u64);
    impl Id for Small {
        fn id(&self) -> u32 {
            self.0
        }
    }
    impl Id for Big {
        fn id(&self) -> u32 {
            let _ = self.1;
            self.0 + 100
        }
    }

    use crate::link::WIDE_ATOMIC;

    #[test]
    fn test_cell_slice_len_changed() {
        let c: RcuCell<[u8]> = RcuCell::from(Arc::<[u8]>::from(&[1u8, 2, 3][..]));
        let short = Arc::<[u8]>::from(&[4u8, 5][..]);
        if !WIDE_ATOMIC {
            let r = catch_unwind(AssertUnwindSafe(|| c.write(short)));
            assert!(r.is_err());
            assert_eq!(&*c.read().unwrap(), &[1, 2, 3]);
            return;
        }
        let old = c.write(short).unwrap();
        assert_eq!(&*old, &[1, 2, 3]);
        assert_eq!(&*c.read().unwrap(), &[4, 5]);
        assert_eq!(&*c.take().unwrap(), &[4, 5]);
        assert!(c.read().is_none());
        c.write(Arc::<[u8]>::from(&[9u8][..]));
        assert_eq!(&*c.read().unwrap(), &[9]);
    }

    #[test]
    fn test_cell_dyn_swap_implementors() {
        let c: RcuCell<dyn Id> = RcuCell::from(Arc::new(Small(1)) as Arc<dyn Id>);
        assert_eq!(c.read().unwrap().id(), 1);
        if !WIDE_ATOMIC {
            // writes with a different vtable are rejected, never accepted
            let r = catch_unwind(AssertUnwindSafe(|| {
                c.write(Arc::new(Big(2, 3)) as Arc<dyn Id>)
            }));
            assert!(r.is_err());
            let r = catch_unwind(AssertUnwindSafe(|| {
                c.update(|_| Some(Arc::new(Big(2, 3)) as Arc<dyn Id>))
            }));
            assert!(r.is_err());
            assert_eq!(c.read().unwrap().id(), 1);
            return;
        }
        assert_eq!(c.write(Arc::new(Big(2, 3)) as Arc<dyn Id>).unwrap().id(), 1);
        assert_eq!(c.read().unwrap().id(), 102);
        let old = c.update(|old| {
            assert_eq!(old.unwrap().id(), 102);
            Some(Arc::new(Small(5)) as Arc<dyn Id>)
        });
        assert_eq!(old.unwrap().id(), 102);
        assert_eq!(c.read().unwrap().id(), 5);
        assert!(c.take().is_some());
        assert!(c.read().is_none());
        c.write(Arc::new(Big(7, 7)) as Arc<dyn Id>);
        assert_eq!(c.read().unwrap().id(), 107);
    }

    #[test]
    fn test_unsized_none() {
        use super::RcuWeak;
        let c: RcuCell<dyn Id> = RcuCell::none();
        assert!(c.is_none());
        assert!(c.read().is_none());
        assert!(c.take().is_none());
        let d: RcuCell<[u8]> = RcuCell::default();
        assert!(d.read().is_none());
        assert!(d.write(Arc::<[u8]>::from(&[1u8, 2][..])).is_none());
        assert_eq!(&*d.read().unwrap(), &[1, 2]);
        assert!(d.take().is_some());
        assert!(d.read().is_none());

        // the first write decides the metadata
        assert!(c.write(Arc::new(Big(1, 2)) as Arc<dyn Id>).is_none());
        assert_eq!(c.read().unwrap().id(), 101);
        if !WIDE_ATOMIC {
            let r = catch_unwind(AssertUnwindSafe(|| {
                c.write(Arc::new(Small(1)) as Arc<dyn Id>)
            }));
            assert!(r.is_err());
        } else {
            c.write(Arc::new(Small(1)) as Arc<dyn Id>);
            assert_eq!(c.read().unwrap().id(), 1);
        }

        let w: RcuWeak<dyn Id> = RcuWeak::new();
        assert!(w.upgrade().is_none());
        let a = Arc::new(Small(3)) as Arc<dyn Id>;
        w.write_arc(&a);
        assert_eq!(w.upgrade().unwrap().id(), 3);
        assert!(w.take().upgrade().is_some());
        assert!(w.upgrade().is_none());
        let w2: RcuWeak<[u8]> = RcuWeak::default();
        assert!(w2.upgrade().is_none());
    }

    #[test]
    fn test_cell_dyn_compare_exchange() {
        use Ordering::SeqCst;
        let c: RcuCell<dyn Id> = RcuCell::from(Arc::new(Small(1)) as Arc<dyn Id>);
        let cur = Arc::as_ptr(&c.read().unwrap());
        let new = Arc::new(Big(2, 3)) as Arc<dyn Id>;
        if !WIDE_ATOMIC {
            let r = catch_unwind(AssertUnwindSafe(|| unsafe {
                c.compare_exchange(cur, Some(&new), SeqCst, SeqCst)
            }));
            assert!(r.is_err());
            assert_eq!(c.read().unwrap().id(), 1);
            return;
        }
        assert!(unsafe { c.compare_exchange(cur, Some(&new), SeqCst, SeqCst) }.is_ok());
        assert_eq!(c.read().unwrap().id(), 102);
        assert!(c.arc_eq(&new));
        // the old pointer is stale now
        assert!(unsafe { c.compare_exchange(cur, None, SeqCst, SeqCst) }.is_err());
        let cur = Arc::as_ptr(&new);
        assert!(unsafe { c.compare_exchange(cur, None, SeqCst, SeqCst) }.is_ok());
        assert!(c.is_none());
    }

    #[test]
    fn test_weak_dyn_swap_implementors() {
        use super::RcuWeak;
        let a = Arc::new(Small(1)) as Arc<dyn Id>;
        let b = Arc::new(Big(2, 3)) as Arc<dyn Id>;
        let w = RcuWeak::from(Arc::downgrade(&a));
        assert_eq!(w.upgrade().unwrap().id(), 1);
        if !WIDE_ATOMIC {
            let r = catch_unwind(AssertUnwindSafe(|| w.write(Arc::downgrade(&b))));
            assert!(r.is_err());
            assert_eq!(w.upgrade().unwrap().id(), 1);
            return;
        }
        w.write(Arc::downgrade(&b));
        assert_eq!(w.upgrade().unwrap().id(), 102);
        w.write_arc(&a);
        assert_eq!(w.upgrade().unwrap().id(), 1);
    }

    #[test]
    fn test_dyn_concurrent_swap() {
        extern crate std;
        if !WIDE_ATOMIC {
            return;
        }
        const N: u64 = if cfg!(miri) { 50 } else { 20000 };
        let c: Arc<RcuCell<dyn Id>> = Arc::new(RcuCell::from(Arc::new(Small(1)) as Arc<dyn Id>));
        let writer = {
            let c = c.clone();
            std::thread::spawn(move || {
                for i in 0..N {
                    if i % 2 == 0 {
                        c.write(Arc::new(Big(1, i)) as Arc<dyn Id>);
                    } else {
                        c.write(Arc::new(Small(1)) as Arc<dyn Id>);
                    }
                }
            })
        };
        let readers: std::vec::Vec<_> = (0..3)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || {
                    for _ in 0..N {
                        // a torn address/vtable pair would return a wrong id or crash
                        let id = c.read().unwrap().id();
                        assert!(id == 1 || id == 101);
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }
}
