use alloc::sync::{Arc, Weak};
use core::mem::ManuallyDrop;
use core::sync::atomic::Ordering;

use crate::link::LinkWrapper;

#[inline]
fn ptr_to_weak<T: ?Sized>(ptr: *const T) -> Weak<T> {
    // a null ptr stands for the dangling weak, the metadata is kept
    let ptr = if ptr.is_null() {
        ptr.with_addr(usize::MAX)
    } else {
        ptr
    };
    unsafe { Weak::from_raw(ptr) }
}

#[inline]
fn is_dangling<T: ?Sized>(ptr: *const T) -> bool {
    ptr.addr() == usize::MAX
}

/// RCU weak cell, it behaves like `RwLock<Weak<T>>`
///
/// # Unsized types
///
/// `T` may be unsized (`dyn Trait`, `[U]`, `str`), created from an existing `Arc`:
///
/// ```
/// use rcu_cell::RcuWeak;
/// use std::sync::Arc;
///
/// trait Shape: Send + Sync {}
/// struct Square(u32);
/// impl Shape for Square {}
///
/// let a: Arc<dyn Shape> = Arc::new(Square(1));
/// let cell: RcuWeak<dyn Shape> = RcuWeak::from(Arc::downgrade(&a));
/// ```
///
/// Limitations:
/// - An unsized cell can't be created empty (there is no metadata to start
///   with), so `none()`, `some()`, `new()` and `Default` need `T: Sized`. It can
///   be emptied later with `take()` and written again.
/// - Values written later need the same pointer metadata (the slice length or
///   the `dyn Trait` vtable) as the initial one, otherwise the write panics.
///   It is never undefined behavior, but the vtable address of a type is not
///   guaranteed to be unique (e.g. across codegen units), so writing the same
///   concrete type may also panic.
/// - On `x86_64` with the `cmpxchg16b` target feature enabled statically (e.g.
///   `RUSTFLAGS="-C target-cpu=native"` or `-C target-feature=+cmpxchg16b`) the
///   address and the metadata are stored in one lock-free 128-bit atomic, then
///   values with different concrete types or lengths can be swapped freely and
///   the metadata check above doesn't apply. Other targets (and Miri) use the
///   metadata check.
/// - Sized cells always use a single `AtomicUsize`, they pay nothing for this.
#[derive(Debug)]
pub struct RcuWeak<T: ?Sized> {
    link: LinkWrapper<T>,
}

unsafe impl<T: ?Sized + Send + Sync> Send for RcuWeak<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RcuWeak<T> {}

impl<T: ?Sized> Drop for RcuWeak<T> {
    fn drop(&mut self) {
        let ptr = self.link.get_ref();
        let _ = ptr_to_weak(ptr);
    }
}

impl<T> Default for RcuWeak<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> From<Weak<T>> for RcuWeak<T> {
    fn from(data: Weak<T>) -> Self {
        let mut weak_ptr = Weak::into_raw(data);
        if is_dangling(weak_ptr) {
            weak_ptr = weak_ptr.with_addr(0);
        }
        RcuWeak {
            link: LinkWrapper::from_ptr(weak_ptr),
        }
    }
}

impl<T> RcuWeak<T> {
    /// create an dummy rcu weak cell instance, upgrade from it will return None
    #[inline]
    pub const fn new() -> Self {
        RcuWeak {
            link: LinkWrapper::new(),
        }
    }
}

impl<T: ?Sized> RcuWeak<T> {
    /// convert the rcu weak to a `Weak`` value
    #[inline]
    pub fn into_weak(self) -> Weak<T> {
        let ptr = self.link.get_ref();
        let ret = ptr_to_weak(ptr);
        let _ = ManuallyDrop::new(self);
        ret
    }

    fn weak_to_ptr(&self, data: Weak<T>) -> *const T {
        let ptr = Weak::as_ptr(&data);
        if is_dangling(ptr) {
            return self.link.null();
        }
        self.link.check_meta(ptr);
        Weak::into_raw(data)
    }

    /// take the value from the rcu weak, leave the rcu weak with default value
    #[inline]
    pub fn take(&self) -> Weak<T> {
        ptr_to_weak(self.link.update(self.link.null()))
    }

    /// write a new weak value to the rcu weak cell and return the old value
    #[inline]
    pub fn write(&self, data: Weak<T>) -> Weak<T> {
        let new_ptr = self.weak_to_ptr(data);
        ptr_to_weak(self.link.update(new_ptr))
    }

    /// write a new `Weak` value downgrade from the `Arc`` to the cell and return the old value
    #[inline]
    pub fn write_arc(&self, data: &Arc<T>) -> Weak<T> {
        let new_ptr = self.weak_to_ptr(Arc::downgrade(data));
        ptr_to_weak(self.link.update(new_ptr))
    }

    /// read out the inner weak value
    #[inline]
    pub fn read(&self) -> Weak<T> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = (*v).clone();
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// upgrade the innner weak value to an Arc value
    #[inline]
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.upgrade();
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// read inner ptr and check if it is the same as the given Arc
    #[inline]
    pub fn arc_eq(&self, data: &Arc<T>) -> bool {
        core::ptr::addr_eq(self.link.get_ref(), Arc::as_ptr(data))
    }

    /// read inner ptr and check if it is the same as the given Weak
    #[inline]
    pub fn weak_eq(&self, data: &Weak<T>) -> bool {
        core::ptr::addr_eq(self.link.get_ref(), Weak::as_ptr(data))
    }

    /// check if two RcuWeak instances point to the same inner Weak
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        core::ptr::addr_eq(this.link.get_ref(), other.link.get_ref())
    }
}
