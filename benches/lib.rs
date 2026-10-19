#![feature(test)]

extern crate test;

use rcu_cell::RcuCell;
use test::Bencher;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[bench]
fn rcu_read(b: &mut Bencher) {
    let rcu_cell = Arc::new(RcuCell::new(10));
    b.iter(|| {
        let v = rcu_cell.read().unwrap();
        test::black_box(&*v);
    });
}

#[bench]
fn rcu_write(b: &mut Bencher) {
    let rcu_cell = Arc::new(RcuCell::new(0));
    let mut i = 0;
    b.iter(|| {
        i += 1;
        let v = rcu_cell.write(i).unwrap();
        assert_eq!(*v, i - 1);
    });
}

#[bench]
fn read_update_1(b: &mut Bencher) {
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
                    rcu.update(|_| Some(Foo(i)));
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
fn read_update_2(b: &mut Bencher) {
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
                    rcu.update(|_| Some(Foo(i)));
                }
            });

            let rcu = rcu_cell.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.update(|_| Some(Foo(i)));
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

#[bench]
fn rwlock_arc(b: &mut Bencher) {
    use spin::RwLock;
    static REF: AtomicUsize = AtomicUsize::new(0);

    struct Foo(usize);

    impl Drop for Foo {
        fn drop(&mut self) {
            REF.fetch_add(1, Ordering::Relaxed);
        }
    }

    b.iter(|| {
        REF.store(0, Ordering::Relaxed);
        let arc_swap = Arc::new(RwLock::new(Arc::new(Foo(42))));
        std::thread::scope(|s| {
            let rcu = arc_swap.clone();
            s.spawn(move || {
                for i in 0..1000 {
                    *rcu.write() = Arc::new(Foo(i));
                }
            });
            let readers = 8;
            for _ in 0..readers {
                let rcu = arc_swap.clone();
                s.spawn(move || {
                    for _i in 0..1000 {
                        let _v: Arc<_> = rcu.read().clone();
                    }
                });
            }
        });
        assert_eq!(arc_swap.read().0, 999);
        drop(arc_swap);
        assert_eq!(REF.load(Ordering::Relaxed), 1001);
    });
}

trait Val: Send + Sync {
    fn get(&self) -> usize;
}

struct Num(usize);

impl Val for Num {
    fn get(&self) -> usize {
        self.0
    }
}

// all values are coerced in this one function so they share a single vtable address,
// which is required when the 128-bit atomic is not available
#[inline(never)]
fn new_val(v: usize) -> Arc<dyn Val> {
    Arc::new(Num(v))
}

#[bench]
fn rcu_dyn_read(b: &mut Bencher) {
    let a = new_val(10);
    let rcu_cell: RcuCell<dyn Val> = RcuCell::from(a);
    b.iter(|| {
        let v = rcu_cell.read().unwrap();
        test::black_box(v.get());
    });
}

#[bench]
fn rcu_dyn_write(b: &mut Bencher) {
    let a = new_val(0);
    let rcu_cell: RcuCell<dyn Val> = RcuCell::from(a);
    let mut i = 0;
    b.iter(|| {
        i += 1;
        let v = rcu_cell.write(new_val(i)).unwrap();
        assert_eq!(v.get(), i - 1);
    });
}

#[bench]
fn read_write_dyn(b: &mut Bencher) {
    b.iter(|| {
        let a = new_val(42);
        let rcu_cell: RcuCell<dyn Val> = RcuCell::from(a);
        std::thread::scope(|s| {
            let rcu = &rcu_cell;
            s.spawn(move || {
                for i in 0..1000 {
                    rcu.write(new_val(i));
                }
            });
            let readers = 8;
            for _ in 0..readers {
                let rcu = &rcu_cell;
                s.spawn(move || {
                    for _i in 0..1000 {
                        let v = rcu.read().unwrap();
                        test::black_box(v.get());
                    }
                });
            }
        });
        assert_eq!(rcu_cell.read().unwrap().get(), 999);
    });
}
