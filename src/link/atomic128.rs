//! A minimal 128-bit atomic built on `cmpxchg16b` for stable rust on x86_64, used when
//! `core::sync::atomic::AtomicU128` is not available. It is only compiled when the
//! target feature is statically enabled, so there is no runtime detection and no lock
//! fallback. All operations are sequentially consistent.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;

#[repr(C, align(16))]
pub(super) struct AtomicU128 {
    v: UnsafeCell<u128>,
}

unsafe impl Sync for AtomicU128 {}
unsafe impl Send for AtomicU128 {}

impl AtomicU128 {
    #[inline]
    pub(super) const fn new(v: u128) -> Self {
        AtomicU128 {
            v: UnsafeCell::new(v),
        }
    }

    // returns the previous value and whether the exchange happened
    #[inline]
    fn cas(&self, old: u128, new: u128) -> (u128, bool) {
        let dst = self.v.get();
        let (old_lo, old_hi) = (old as u64, (old >> 64) as u64);
        let (new_lo, new_hi) = (new as u64, (new >> 64) as u64);
        let (prev_lo, prev_hi): (u64, u64);
        let ok: u8;
        // rbx is reserved by LLVM, so it's swapped in and out around the instruction.
        // The destination is pinned to rdi, otherwise it could be allocated to a register
        // that is clobbered by the swap.
        unsafe {
            asm!(
                "xchg {rbx_tmp}, rbx",
                "lock cmpxchg16b xmmword ptr [rdi]",
                "mov rbx, {rbx_tmp}",
                "sete {ok}",
                in("rdi") dst,
                rbx_tmp = inout(reg) new_lo => _,
                ok = out(reg_byte) ok,
                in("rcx") new_hi,
                inout("rax") old_lo => prev_lo,
                inout("rdx") old_hi => prev_hi,
                options(nostack),
            );
        }
        ((prev_lo as u128) | ((prev_hi as u128) << 64), ok != 0)
    }

    #[inline]
    pub(super) fn load(&self, _order: Ordering) -> u128 {
        // a failed (or no-op) compare-exchange returns the current value
        self.cas(0, 0).0
    }

    #[inline]
    pub(super) fn compare_exchange(
        &self,
        current: u128,
        new: u128,
        _success: Ordering,
        _failure: Ordering,
    ) -> Result<u128, u128> {
        match self.cas(current, new) {
            (v, true) => Ok(v),
            (v, false) => Err(v),
        }
    }

    #[inline]
    pub(super) fn compare_exchange_weak(
        &self,
        current: u128,
        new: u128,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u128, u128> {
        self.compare_exchange(current, new, success, failure)
    }

    #[inline]
    pub(super) fn fetch_add(&self, val: u128, _order: Ordering) -> u128 {
        let mut cur = self.load(Ordering::Relaxed);
        loop {
            match self.cas(cur, cur.wrapping_add(val)) {
                (v, true) => return v,
                (v, false) => cur = v,
            }
        }
    }

    #[inline]
    pub(super) fn fetch_sub(&self, val: u128, _order: Ordering) -> u128 {
        let mut cur = self.load(Ordering::Relaxed);
        loop {
            match self.cas(cur, cur.wrapping_sub(val)) {
                (v, true) => return v,
                (v, false) => cur = v,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::AtomicU128;
    use core::sync::atomic::Ordering::SeqCst;

    #[test]
    fn test_ops() {
        let v = (7u128 << 64) | 5;
        let a = AtomicU128::new(v);
        assert_eq!(a.load(SeqCst), v);
        let n = (9u128 << 64) | 8;
        assert_eq!(a.compare_exchange(v, n, SeqCst, SeqCst), Ok(v));
        assert_eq!(a.load(SeqCst), n);
        assert_eq!(a.compare_exchange(v, 0, SeqCst, SeqCst), Err(n));
        assert_eq!(a.fetch_add(1, SeqCst), n);
        assert_eq!(a.fetch_sub(2, SeqCst), n + 1);
        assert_eq!(a.load(SeqCst), n - 1);
    }
}
