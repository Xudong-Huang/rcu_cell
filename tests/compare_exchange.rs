//! Soundness regressions around `compare_exchange`, they only show as
//! use-after-free / wrong-layout frees under Miri.

use core::sync::atomic::Ordering::SeqCst;
use rcu_cell::RcuCell;
use std::sync::{Arc, Barrier};

struct Value {
    blockers: Option<(Arc<Barrier>, Arc<Barrier>)>,
}

impl Drop for Value {
    fn drop(&mut self) {
        if let Some((started, resume)) = &self.blockers {
            // park the drop so a concurrent writer sees the published pointer
            started.wait();
            resume.wait();
        }
    }
}

fn plain() -> Value {
    Value { blockers: None }
}

#[test]
fn cell_owns_the_new_ref_before_publishing_it() {
    let started = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let cell = RcuCell::from(Arc::new(Value {
        blockers: Some((started.clone(), resume.clone())),
    }));

    let new = Arc::new(plain());
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let cur = Arc::as_ptr(cell.read().as_ref().unwrap());
            let res = unsafe { cell.compare_exchange(cur, Some(&new), SeqCst, SeqCst) };
            assert!(res.is_ok());
        });
        // the old value is being dropped, so the new pointer is published
        started.wait();
        // a concurrent writer takes the new value over, the cell must already
        // own its own ref even though compare_exchange has not returned yet
        let swapped = cell.write(plain());
        let count = Arc::strong_count(&new);
        resume.wait();
        handle.join().unwrap();
        // the caller's `new` plus the one the cell owns
        assert_eq!(count, 2);
        drop(swapped);
    });

    assert!(cell.read().is_some());
}

#[test]
fn compare_exchange_drops_the_value_with_the_stored_metadata() {
    let value: Arc<[u8]> = Arc::from(&[1u8, 2, 3][..]);
    let cell = RcuCell::from(value);
    let addr = Arc::as_ptr(cell.read().as_ref().unwrap()) as *const u8;

    // the comparison is by address, so the same address with a different
    // length must still drop the old value with the metadata of the link
    let current = std::ptr::slice_from_raw_parts(addr, 0);
    let res = unsafe { cell.compare_exchange(current, None, SeqCst, SeqCst) };
    assert!(res.is_ok());
    assert!(cell.read().is_none());
}
