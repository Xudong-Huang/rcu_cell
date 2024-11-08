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
                    let mut w = rcu.try_lock().unwrap();
                    w.update(Foo(i));
                }
            });
            let readers = 8;
            for _ in 0..readers {
                let rcu = rcu_cell.clone();
                s.spawn(move || {
                    for _i in 0..1000 {
                        let _v = rcu.read().unwrap();
                    }
                });
            }
        });
        assert_eq!(rcu_cell.read().unwrap().0, 999);
        drop(rcu_cell);
        assert_eq!(REF.load(Ordering::Relaxed), 1001);
    });
}
