// With a lock free 128-bit atomic, the data address and the pointer metadata of an
// unsized `T` are updated together. Otherwise only the address is atomic, and the
// metadata is fixed when the link is created.
//
// `cmpxchg16b` is only used when it's statically enabled (for example with
// `-C target-cpu=native` or `-C target-feature=+cmpxchg16b`), and never under Miri.

#[cfg(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri)))]
mod atomic128;
#[cfg(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri)))]
mod wide;
#[cfg(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri)))]
pub(crate) use wide::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri)))]
#[cfg(test)]
pub(crate) const WIDE_ATOMIC: bool = true;

#[cfg(not(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))))]
mod narrow;
#[cfg(not(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))))]
pub(crate) use narrow::LinkWrapper;
/// whether unsized values with different pointer metadata can be swapped in a cell
#[cfg(not(all(target_arch = "x86_64", target_feature = "cmpxchg16b", not(miri))))]
#[cfg(test)]
pub(crate) const WIDE_ATOMIC: bool = false;
