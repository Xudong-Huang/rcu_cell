#![feature(test)]

extern crate test;

use rcu_cell::RcuCell;
use test::Bencher;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[bench]
fn read_write_1(b: &mut Bencher) {
    static REF: AtomicUsize = AtomicUsize::new(0);

    struct Foo(usize);

    impl Drop for Foo {
        fn drop(&mut self) {
            REF.fetch_add(1, Ordering::Relaxed);
        }
    }

    b.iter(|| {
        REF.store(0, Ordering::Relaxed);
        let rcu_cell = Arc::new(RcuCell::new(Foo(42)));
        std::thread::scope(|s| {
            let rcu = rcu_cell.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.write(Foo(i));
                }
            });
            let readers = 8;
            for _ in 0..readers {
                let rcu = rcu_cell.clone();
                s.spawn(move || {
                    for _i in 0..1000 {
                        let v = rcu.read().unwrap();
                        test::black_box(&*v);
                    }
                });
            }
        });
        assert_eq!(rcu_cell.read().unwrap().0, 999);
        drop(rcu_cell);
        assert_eq!(REF.load(Ordering::Relaxed), 1001);
    });
}

#[bench]
fn read_write_2(b: &mut Bencher) {
    static REF: AtomicUsize = AtomicUsize::new(0);

    struct Foo(usize);

    impl Drop for Foo {
        fn drop(&mut self) {
            REF.fetch_add(1, Ordering::Relaxed);
        }
    }

    b.iter(|| {
        REF.store(0, Ordering::Relaxed);
        let rcu_cell = Arc::new(RcuCell::new(Foo(42)));
        std::thread::scope(|s| {
            let rcu = rcu_cell.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.write(Foo(i));
                }
            });

            let rcu = rcu_cell.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.write(Foo(i));
                }
            });

            let readers = 8;
            for _ in 0..readers {
                let rcu = rcu_cell.clone();
                s.spawn(move || {
                    for _i in 0..1000 {
                        let v = rcu.read().unwrap();
                        test::black_box(&*v);
                    }
                });
            }
        });
        assert_eq!(rcu_cell.read().unwrap().0, 999);
        drop(rcu_cell);
        assert_eq!(REF.load(Ordering::Relaxed), 2001);
    });
}

#[bench]
fn arc_swap(b: &mut Bencher) {
    use arc_swap::ArcSwap;
    static REF: AtomicUsize = AtomicUsize::new(0);

    struct Foo(usize);

    impl Drop for Foo {
        fn drop(&mut self) {
            REF.fetch_add(1, Ordering::Relaxed);
        }
    }

    b.iter(|| {
        REF.store(0, Ordering::Relaxed);
        let arc_swap = Arc::new(ArcSwap::new(Arc::new(Foo(42))));
        std::thread::scope(|s| {
            let rcu = arc_swap.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.store(Arc::new(Foo(i)));
                }
            });
            let readers = 8;
            for _ in 0..readers {
                let rcu = arc_swap.clone();
                s.spawn(move || {
                    for _i in 0..1000 {
                        let v = rcu.load();
                        test::black_box(&*v);
                    }
                });
            }
        });
        assert_eq!(arc_swap.load().0, 999);
        drop(arc_swap);
        assert_eq!(REF.load(Ordering::Relaxed), 1001);
    });
}
