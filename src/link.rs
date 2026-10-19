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
//   or when build.rs finds that the build machine, which is also the target, supports it
//   and not under Miri. This works on stable.

// the layout of the tagged address word: the address is shifted left by `LEADING_BITS`,
// the low bits hold the reader count and the highest of them is the update flag
pub(super) const LEADING_BITS: usize = 8;
pub(super) const ALIGN_BITS: usize = 3;

pub(super) const LOWER_MASK: usize = (1 << ALIGN_BITS) - 1;
pub(super) const HIGHER_MASK: usize =
    !((1 << (usize::MAX.leading_ones() as usize - LEADING_BITS)) - 1);
pub(super) const REFCOUNT_MASK: usize = (1 << (LEADING_BITS + ALIGN_BITS)) - 1;
pub(super) const UPDATE_MASK: usize = 1 << (LEADING_BITS + ALIGN_BITS - 1);
pub(super) const UPDATE_REF_MASK: usize = REFCOUNT_MASK & !UPDATE_MASK;

#[cfg(rcu_cell_cx16)]
mod atomic128;

#[cfg(rcu_cell_wide)]
mod wide;
#[cfg(rcu_cell_wide)]
pub(crate) use wide::LinkWrapper;

#[cfg(not(rcu_cell_wide))]
mod narrow;
#[cfg(not(rcu_cell_wide))]
pub(crate) use narrow::LinkWrapper;
