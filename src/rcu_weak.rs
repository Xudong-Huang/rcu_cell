use alloc::sync::{Arc, Weak};
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::link::{LinkWrapper, UnlockOnPanic};

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
///   `upgrade` returns `None`, while `read`, `take` and `into_weak` panic since
///   they must return a dangling `Weak` and have no metadata to build it with.
///   `write` and `compare_exchange` set the metadata from their `new` value
///   first, so they only panic when that value is dangling and no metadata was
///   set yet; they then have to return a dangling `Weak`, as the old value or
///   as the failed `new`.
/// - By default later writes need the same pointer metadata (the slice length or
///   the `dyn Trait` vtable) as the first value, otherwise the write panics.
///   It is never undefined behavior, but the vtable address of a type is not
///   guaranteed to be unique (e.g. across codegen units), so writing the same
///   concrete type may also panic.
/// - With a nightly compiler, on targets that have 128-bit atomics
///   (`target_has_atomic = "128"`, e.g. aarch64 and s390x) the address and the metadata
///   are stored in one `core::sync::atomic::AtomicU128`, so values with different
///   concrete types or lengths can be swapped freely and the metadata check doesn't
///   apply. `build.rs` detects this automatically.
/// - On x86_64 the same applies when `cmpxchg16b` is enabled at compile time or
///   when `build.rs` finds that the build machine (also the target) supports it.
///   The binary then needs a cpu with `cmpxchg16b`.
/// - Sized cells use a single `AtomicUsize` as the atomic, but in this wide mode the
///   cell is still 16 bytes, the size of the 128-bit atomic.
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
    /// create a dummy rcu weak cell instance, upgrade from it will return None
    #[inline]
    pub const fn new() -> Self {
        RcuWeak {
            link: LinkWrapper::new(),
        }
    }
    /// convert the rcu weak to a `Weak` value
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

    /// write a new `Weak` value downgraded from the `Arc` to the cell and return the old value
    #[inline]
    pub fn write_arc(&self, data: &Arc<T>) -> Weak<T> {
        let new_ptr = self.weak_to_ptr(Arc::downgrade(data));
        self.weak_from_link(self.link.update(new_ptr))
    }

    /// Atomically update the value with a closure and return the old value.
    /// The closure will be called with the old value and return the new value;
    /// returning `None` clears the cell. The return value is the old weak value,
    /// or `None` if the cell was empty.
    /// The closure should not take too long, internally it uses a spin lock to
    /// prevent other writers from updating the value
    ///
    /// The closure borrows the old value, so no `Weak` refcount increment or
    /// decrement is needed while the lock is held. If you need to own the old
    /// `Weak`, call `.cloned()` on it inside the closure.
    pub fn update<R, F>(&self, f: F) -> Option<Weak<T>>
    where
        F: FnOnce(Option<&Weak<T>>) -> Option<R>,
        R: Into<Weak<T>>,
    {
        // increase ref count to lock the inner Weak
        let ptr = self.link.lock_read();
        // the cell still owns this Weak until the update is done, so a panic
        // in `f` or in the metadata check must not drop it
        let old_value = ManuallyDrop::new(ptr_to_weak(ptr));
        // if `f` panics, put the old value back and release the lock, otherwise
        // every later writer would spin forever
        let unlock = UnlockOnPanic {
            link: &self.link,
            ptr,
        };
        // a dangling weak is stored as an empty link
        let new_ptr = f((*old_value).as_ref()).and_then(|data| self.weak_to_ptr(data.into()));
        // if `f` panics, the `UnlockOnPanic` will release the link
        // and prevent other writers from spinning forever.
        // Otherwise, we manually unlock the link after updating it.
        core::mem::forget(unlock);
        self.link.unlock_update(new_ptr);
        ManuallyDrop::into_inner(old_value)
    }

    /// Stores `new` into the RcuWeak if the cell currently holds `current`,
    /// compared by the address of the target. An empty cell and a dangling weak
    /// (`Weak::new()`, or the `Weak` returned by [`read`](Self::read) for an
    /// empty cell) are considered the same, so a dangling `current` matches an
    /// empty cell, and a dangling `new` clears it.
    ///
    /// `new` is taken by value, so the caller's weak ref is moved into the cell
    /// without a refcount change.
    ///
    /// On success the previous value is returned as an owned `Weak`, a dangling
    /// weak if the cell was empty.
    ///
    /// On failure the cell is left untouched and `new` is returned back, so the
    /// caller can reuse it for another attempt.
    ///
    /// This method takes two `Ordering` arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcu_cell::RcuWeak;
    /// use std::sync::Arc;
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// let a = Arc::new(1234);
    /// let b = Arc::new(5678);
    /// let w = RcuWeak::from(Arc::downgrade(&a));
    ///
    /// let curr = w.read();
    /// let res = w.compare_exchange(&curr, Arc::downgrade(&b), SeqCst, SeqCst);
    /// assert!(res.is_ok());
    /// assert_eq!(w.upgrade().as_deref(), Some(&5678));
    /// ```
    pub fn compare_exchange(
        &self,
        current: &Weak<T>,
        new: Weak<T>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Weak<T>, Weak<T>> {
        // an empty cell and a dangling weak are the same, both are stored as an empty link
        let current_ptr = {
            let ptr = Weak::as_ptr(current);
            (!is_dangling(ptr)).then(|| unsafe { NonNull::new_unchecked(ptr as *mut T) })
        };

        // a dangling `new` clears the cell, so it is stored as an empty link
        let ptr = Weak::as_ptr(&new);
        let dangling = is_dangling(ptr);
        if !dangling {
            self.link.check_meta(ptr);
        }
        // the caller's weak ref moves into the raw pointer; a failed exchange
        // turns it back into a `Weak` and hands it back
        let new_ptr = self.weak_to_ptr(new);

        // Safety: `new_ptr` was built by `Weak::into_raw` and is owned by the cell
        // on success (or turned back into the caller's `Weak` below on failure),
        // and `current_ptr` is only compared as an address, never dereferenced.
        match unsafe {
            self.link
                .compare_exchange(current_ptr, new_ptr, success, failure)
        } {
            Ok(old) => Ok(self.weak_from_link(old)),
            Err(_) => {
                // the exchange failed, so the cell never took the weak ref we made
                // with `Weak::into_raw`; hand the caller's `new` back
                Err(self.weak_from_link(new_ptr))
            }
        }
    }

    /// read out the inner weak value
    #[inline]
    pub fn read(&self) -> Weak<T> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.as_ref().cloned();
        self.link.dec_ref();
        cloned.unwrap_or_else(|| self.weak_from_link(None))
    }

    /// upgrade the inner weak value to an Arc value
    #[inline]
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_weak(ptr));
        let cloned = v.as_ref().and_then(Weak::upgrade);
        self.link.dec_ref();
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
