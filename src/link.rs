// With a lock free 128-bit atomic, the data address and the pointer metadata of an
// unsized `T` are updated together. Otherwise only the address is atomic, and the
// metadata is fixed when the link is created.
//
// `core::sync::atomic::AtomicU128` is unstable (`integer_atomics`) and only exists on
// targets with `target_has_atomic = "128"` (e.g. aarch64, s390x, but not x86_64), so
// it is used only with the `nightly` feature on those targets.

#[cfg(all(feature = "nightly", target_has_atomic = "128"))]
mod wide;
#[cfg(all(feature = "nightly", target_has_atomic = "128"))]
pub(crate) use wide::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(all(feature = "nightly", target_has_atomic = "128"), test))]
pub(crate) const WIDE_ATOMIC: bool = true;

#[cfg(not(all(feature = "nightly", target_has_atomic = "128")))]
mod narrow;
#[cfg(not(all(feature = "nightly", target_has_atomic = "128")))]
pub(crate) use narrow::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(not(all(feature = "nightly", target_has_atomic = "128")), test))]
pub(crate) const WIDE_ATOMIC: bool = false;
