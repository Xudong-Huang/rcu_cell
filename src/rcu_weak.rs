use alloc::sync::{Arc, Weak};
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::link::LinkWrapper;

#[inline]
fn ptr_to_weak<T: ?Sized>(ptr: Option<NonNull<T>>) -> Option<Weak<T>> {
    ptr.map(|p| unsafe { Weak::from_raw(p.as_ptr()) })
}

#[inline]
fn same_addr<T: ?Sized>(a: Option<NonNull<T>>, b: Option<NonNull<T>>) -> bool {
    a.map(|p| p.cast::<()>()) == b.map(|p| p.cast::<()>())
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
/// - `some()` and `new()` need `T: Sized`, an unsized cell is created from an
///   existing `Arc`, or empty with `none()` (or `Default`). An empty unsized cell
///   has no pointer metadata yet, the first value written decides it. Until then
///   `upgrade` returns `None`, but `read`, `take`, `write` and `into_weak` panic since
///   they cannot build a dangling `Weak` without metadata.
/// - By default later writes need the same pointer metadata (the slice length or
///   the `dyn Trait` vtable) as the first value, otherwise the write panics.
///   It is never undefined behavior, but the vtable address of a type is not
///   guaranteed to be unique (e.g. across codegen units), so writing the same
///   concrete type may also panic.
/// - With a nightly compiler, on targets that have 128-bit atomics
///   (`target_has_atomic = "128"`, e.g. aarch64 and s390x, but not x86_64) the
///   address and the metadata are stored in one `core::sync::atomic::AtomicU128`,
///   so values with different concrete types or lengths can be swapped freely and
///   the metadata check doesn't apply. `build.rs` detects this automatically.
/// - Sized cells always use a single `AtomicUsize`, they pay nothing for this.
#[derive(Debug)]
pub struct RcuWeak<T: ?Sized> {
    link: LinkWrapper<T>,
}

unsafe impl<T: ?Sized + Send + Sync> Send for RcuWeak<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RcuWeak<T> {}

impl<T: ?Sized> Drop for RcuWeak<T> {
    fn drop(&mut self) {
        let _ = ptr_to_weak(self.link.get_ref());
    }
}

impl<T: ?Sized> Default for RcuWeak<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> From<Weak<T>> for RcuWeak<T> {
    fn from(data: Weak<T>) -> Self {
        let weak_ptr = Weak::into_raw(data);
        // a dangling weak is stored as an empty link
        let ptr =
            (!is_dangling(weak_ptr)).then(|| unsafe { NonNull::new_unchecked(weak_ptr as *mut T) });
        RcuWeak {
            link: LinkWrapper::from_ptr(ptr),
        }
    }
}

impl<T: ?Sized> RcuWeak<T> {
    /// create an dummy rcu weak cell instance, upgrade from it will return None
    #[inline]
    pub const fn new() -> Self {
        RcuWeak {
            link: LinkWrapper::new(),
        }
    }
    /// convert the rcu weak to a `Weak`` value
    #[inline]
    pub fn into_weak(self) -> Weak<T> {
        let ret = self.weak_from_link(self.link.get_ref());
        let _ = ManuallyDrop::new(self);
        ret
    }

    // an empty link stands for the dangling weak, which needs a valid pointer
    // metadata for an unsized `T`, so it panics if no value was ever written
    fn weak_from_link(&self, ptr: Option<NonNull<T>>) -> Weak<T> {
        let ptr = ptr
            .or_else(|| self.link.dangling())
            .expect("a dangling weak of an unsized type needs the metadata of a written value");
        unsafe { Weak::from_raw(ptr.as_ptr()) }
    }

    fn weak_to_ptr(&self, data: Weak<T>) -> Option<NonNull<T>> {
        let ptr = Weak::as_ptr(&data);
        if is_dangling(ptr) {
            return None;
        }
        self.link.check_meta(ptr);
        Some(unsafe { NonNull::new_unchecked(Weak::into_raw(data) as *mut T) })
    }

    /// take the value from the rcu weak, leave the rcu weak with default value
    #[inline]
    pub fn take(&self) -> Weak<T> {
        self.weak_from_link(self.link.update(None))
    }

    /// write a new weak value to the rcu weak cell and return the old value
    #[inline]
    pub fn write(&self, data: Weak<T>) -> Weak<T> {
        let new_ptr = self.weak_to_ptr(data);
        self.weak_from_link(self.link.update(new_ptr))
    }

    /// write a new `Weak` value downgrade from the `Arc`` to the cell and return the old value
    #[inline]
    pub fn write_arc(&self, data: &Arc<T>) -> Weak<T> {
        let new_ptr = self.weak_to_ptr(Arc::downgrade(data));
        self.weak_from_link(self.link.update(new_ptr))
    }

    /// read out the inner weak value
    #[inline]
    pub fn read(&self) -> Weak<T> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.as_ref().cloned();
        self.link.dec_ref();
        let cloned = cloned.unwrap_or_else(|| self.weak_from_link(None));
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// upgrade the innner weak value to an Arc value
    #[inline]
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.as_ref().and_then(Weak::upgrade);
        self.link.dec_ref();
        core::sync::atomic::fence(Ordering::Acquire);
        cloned
    }

    /// read inner ptr and check if it is the same as the given Arc
    #[inline]
    pub fn arc_eq(&self, data: &Arc<T>) -> bool {
        same_addr(
            self.link.get_ref(),
            NonNull::new(Arc::as_ptr(data) as *mut T),
        )
    }

    /// read inner ptr and check if it is the same as the given Weak
    #[inline]
    pub fn weak_eq(&self, data: &Weak<T>) -> bool {
        same_addr(
            self.link.get_ref(),
            NonNull::new(Weak::as_ptr(data) as *mut T),
        )
    }

    /// check if two RcuWeak instances point to the same inner Weak
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        same_addr(this.link.get_ref(), other.link.get_ref())
    }
}
