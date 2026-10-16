// With a lock free 128-bit atomic, the data address and the pointer metadata of an
// unsized `T` are updated together. Otherwise only the address is atomic, and the
// metadata is fixed once by the first value written.
//
// The 128-bit atomic is, in order of preference:
// - `core::sync::atomic::AtomicU128`: unstable (`integer_atomics`) and only on targets
//   with `target_has_atomic = "128"` (e.g. aarch64, s390x), so it is used only when
//   build.rs finds that the compiler and the target support it (nightly).
// - our own `cmpxchg16b` based one on x86_64, only when the target feature is
//   statically enabled (e.g. `-C target-cpu=native` or `-C target-feature=+cmpxchg16b`)
//   and not under Miri. This works on stable.

#[cfg(all(
    not(rcu_cell_atomic_u128),
    target_arch = "x86_64",
    target_feature = "cmpxchg16b",
    not(miri)
))]
mod atomic128;

#[cfg(any(
    rcu_cell_atomic_u128,
    all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
))]
mod wide;
#[cfg(any(
    rcu_cell_atomic_u128,
    all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
))]
pub(crate) use wide::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(
    test,
    any(
        rcu_cell_atomic_u128,
        all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
    )
))]
pub(crate) const WIDE_ATOMIC: bool = true;

#[cfg(not(any(
    rcu_cell_atomic_u128,
    all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
)))]
mod narrow;
#[cfg(not(any(
    rcu_cell_atomic_u128,
    all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
)))]
pub(crate) use narrow::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(
    test,
    not(any(
        rcu_cell_atomic_u128,
        all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))
    ))
))]
pub(crate) const WIDE_ATOMIC: bool = false;
