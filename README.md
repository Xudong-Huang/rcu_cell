[![Build Status](https://travis-ci.org/Xudong-Huang/rcu_cell.svg?branch=master)](https://travis-ci.org/Xudong-Huang/rcu_cell)
[![Current Crates.io Version](https://img.shields.io/crates/v/rcu_cell.svg)](https://crates.io/crates/rcu_cell)
[![Document](https://img.shields.io/badge/doc-rcu_cell-green.svg)](https://docs.rs/rcu_cell)

# RcuCell

A lockless rcu cell implementation that can be used safely in
multithread context.

## Features

- The write operation would not block the read operation.
- The write operation would "block" the write operation.
- The RcuCell could contain no data


## Usage

```rust
   fn single_thread() {
        let t = RcuCell::new(Some(10));
        let x = t.read();
        let y = t.read();
        t.try_lock().unwrap().update(None);
        let z = t.read();
        let a = z.clone();
        drop(t); // t can be dropped before reader
        assert_eq!(x.map(|v| *v), Some(10));
        assert_eq!(y.map(|v| *v), Some(10));
        assert_eq!(z.map(|v| *v), None);
        assert_eq!(a.map(|v| *v), None);
    }
```

