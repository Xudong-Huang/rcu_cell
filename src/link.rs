// With a lock free 128-bit atomic, the data address and the pointer metadata of an
// unsized `T` are updated together. Otherwise only the address is atomic, and the
// metadata is fixed when the link is created.
//
// `core::sync::atomic::AtomicU128` is unstable (`integer_atomics`) and only exists on
// targets with `target_has_atomic = "128"` (e.g. aarch64, s390x, but not x86_64), so
// it is used only when build.rs finds that the compiler and the target support it.

#[cfg(rcu_cell_atomic_u128)]
mod wide;
#[cfg(rcu_cell_atomic_u128)]
pub(crate) use wide::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(rcu_cell_atomic_u128, test))]
pub(crate) const WIDE_ATOMIC: bool = true;

#[cfg(not(rcu_cell_atomic_u128))]
mod narrow;
#[cfg(not(rcu_cell_atomic_u128))]
pub(crate) use narrow::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(not(rcu_cell_atomic_u128), test))]
pub(crate) const WIDE_ATOMIC: bool = false;
