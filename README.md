[![Build Status](https://github.com/Xudong-Huang/rcu_cell/workflows/CI/badge.svg)](https://github.com/Xudong-Huang/rcu_cell/actions?query=workflow%3ACI+branch%3Amaster)
[![Current Crates.io Version](https://img.shields.io/crates/v/rcu_cell.svg)](https://crates.io/crates/rcu_cell)
[![Document](https://img.shields.io/badge/doc-rcu_cell-green.svg)](https://docs.rs/rcu_cell)

# RcuCell

A lockless RCU cell implementation that can be used safely in a multithreaded context.

## Features

- Supports multi-threaded reads and writes.
- Reads do not block other reads.
- A read is similar to `Arc::clone` and is wait-free for a sized cell. An unsized cell in the wide mode updates a 128-bit reference count with a compare-exchange loop, which is lock-free but may retry.
- Writes do not block reads.
- Writes are lockless.
- A write is similar to an atomic swap.
- The cell can be empty.
- Works in `no_std` environments with `alloc`.
- Only 64-bit platforms are supported (the tag and reference count bits are taken from the pointer word, and the fallback packs the address and metadata into one 64-bit word); a 32-bit build fails with a compile error.


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

Writing a value of a *different* concrete type (e.g. a `Circle` into a cell holding a `Square`) needs the 128-bit atomic described below, otherwise the write panics. So this example is only compiled, not run:

```rust,no_run
    use rcu_cell::RcuCell;
    use std::sync::Arc;

    trait Shape: Send + Sync {
        fn area(&self) -> u32;
    }
    struct Square(u32);
    struct Circle(u32);
    impl Shape for Square {
        fn area(&self) -> u32 {
            self.0 * self.0
        }
    }
    impl Shape for Circle {
        fn area(&self) -> u32 {
            3 * self.0 * self.0
        }
    }

    let cell: RcuCell<dyn Shape> = RcuCell::from(Arc::new(Square(3)) as Arc<dyn Shape>);
    // the vtable changes together with the address
    let old = cell.write(Arc::new(Circle(2)) as Arc<dyn Shape>);
    assert_eq!(old.unwrap().area(), 9);
    assert_eq!(cell.read().unwrap().area(), 12);
```

- `RcuCell::some`/`new` need `T: Sized`; an unsized `RcuCell` is created from an existing `Arc` and an unsized `RcuWeak` from an existing `Weak`. Either can start empty with `RcuCell::none()` / `RcuWeak::new()` (or `Default`); an empty unsized cell has no pointer metadata yet, the first value written decides it. `RcuWeak::read/take/into_weak` must return a dangling `Weak<T>`, so they panic on a cell that never held an unsized value; `RcuWeak::write` and `RcuWeak::compare_exchange` take the metadata from their `new` value first, so they only panic when that value is dangling and the cell has no metadata yet (they then have to return a dangling `Weak`, as the old value or as the failed `new`); a live value sets the metadata before it is stored (`upgrade`/`read` of `RcuCell` and `RcuCell::compare_exchange` are fine).
- By default later writes need the same pointer metadata (slice length or `dyn` vtable) as the first value, otherwise they panic. This is never undefined behavior, but vtable addresses are not guaranteed to be unique for a type, so writing the same concrete type may also panic.
- With a nightly compiler, on targets that have 128-bit atomics (`target_has_atomic = "128"`, e.g. aarch64 and s390x, and x86_64 on recent nightlies) the address and the metadata live in one `core::sync::atomic::AtomicU128`, so different concrete types or lengths can be swapped freely and the check doesn't apply. `build.rs` probes for this first, there is nothing to enable.
- On x86_64, when the core atomic is not available, the same applies if `cmpxchg16b` is enabled at compile time, e.g. `RUSTFLAGS="-C target-cpu=native"` or `-C target-feature=+cmpxchg16b`, or when the build machine (also the target) supports it, which `build.rs` detects. A small built-in `cmpxchg16b` atomic is used there (not under Miri). Without either, the single-word fallback above is used. There is no runtime check in the binary, so a binary built for the built-in atomic needs a cpu with `cmpxchg16b`.
- Sized cells use a single `AtomicUsize` as the atomic, but in the wide mode above the cell is still 16 bytes, the size of the 128-bit atomic. In the single-word fallback a sized cell is one word.
