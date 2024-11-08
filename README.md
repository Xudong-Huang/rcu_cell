[![Build Status](https://github.com/Xudong-Huang/rcu_cell/workflows/CI/badge.svg)](https://github.com/Xudong-Huang/rcu_cell/actions?query=workflow%3ACI+branch%3Amaster)
[![Current Crates.io Version](https://img.shields.io/crates/v/rcu_cell.svg)](https://crates.io/crates/rcu_cell)
[![Document](https://img.shields.io/badge/doc-rcu_cell-green.svg)](https://docs.rs/rcu_cell)

# RcuCell

A lockless rcu cell implementation that can be used safely in multithread context.

## Features

- Support multi-thread read and write operations.
- The read operation would not block other read operation.
- The read operation is something like Arc::clone
- The write operation is something like Atomic Swap.
- The write operation would block all read operations.
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
