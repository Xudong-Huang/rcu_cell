[![Build Status](https://github.com/Xudong-Huang/rcu_cell/workflows/CI/badge.svg)](https://github.com/Xudong-Huang/rcu_cell/actions?query=workflow%3ACI+branch%3Amaster)
[![Current Crates.io Version](https://img.shields.io/crates/v/rcu_cell.svg)](https://crates.io/crates/rcu_cell)
[![Document](https://img.shields.io/badge/doc-rcu_cell-green.svg)](https://docs.rs/rcu_cell)

# RcuCell

A lockless rcu cell implementation that can be used safely in multithread context.

## Features

- Support multi-thread read and write operations.
- The read operation would not block other read operations.
- The read operation is always waitless.
- The read operation is something like Arc::clone.
- The write operation would not block other read operations.
- The write operation is lockless.
- The write operation is something like Atomic Swap.
- The RcuCell could contain no data
- Could be compiled with no_std


## Usage

```rust
    use rcu_cell::RcuCell;
    use std::sync::Arc;

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
```

## Unsized types

`RcuCell<T>` and `RcuWeak<T>` accept unsized `T` (`dyn Trait`, `[U]`, `str`), created from an existing `Arc`/`Weak`, e.g. `RcuCell::from(Arc::new(A) as Arc<dyn B>)`.

- `some()` and `new()` need `T: Sized`. An unsized cell is created from an existing `Arc`/`Weak`, or empty with `none()`/`Default`; an empty unsized cell has no pointer metadata yet, the first value written decides it. `RcuWeak::read/take/write/into_weak` and `RcuCell::compare_exchange` need a valid `*const T`/`Weak<T>` to return, so they panic on a cell that never held an unsized value (`upgrade`/`read` of `RcuCell` are fine).
- By default later writes need the same pointer metadata (slice length or `dyn` vtable) as the first value, otherwise they panic. This is never undefined behavior, but vtable addresses are not guaranteed to be unique for a type, so writing the same concrete type may also panic.
- With a nightly compiler, on targets that have 128-bit atomics (`target_has_atomic = "128"`, e.g. aarch64 and s390x, but not x86_64) the address and the metadata live in one `core::sync::atomic::AtomicU128`, so different concrete types or lengths can be swapped freely and the check doesn't apply. `build.rs` detects this automatically, there is nothing to enable.
- Sized cells always use a single `AtomicUsize`.
