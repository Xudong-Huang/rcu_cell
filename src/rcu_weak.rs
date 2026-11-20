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
///   `upgrade` returns `None`, but `read`, `take`, `write` and `into_weak` panic since
///   they cannot build a dangling `Weak` without metadata.
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

    /// Atomicly update the value with a closure and return the old value.
    /// The closure will be called with the old value and return the new value;
    /// returning `None` clears the cell. The return value is the old weak value,
    /// or `None` if the cell was empty.
    /// The closure should not take too long time, internally it's use a spin
    /// lock to prevent other writer to update the value
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

    /// Replace the value with `new` only if the cell currently holds
    /// `current`, compared by the address of the target, and return whether the
    /// exchange happened.
    ///
    /// An empty cell and a dangling weak (`Weak::new()`, or the `Weak` returned
    /// by [`read`](Self::read) for an empty cell) are considered the same, so a
    /// dangling `new` clears the cell.
    ///
    /// This is a safe wrapper around [`compare_exchange`](Self::compare_exchange)
    /// that discards the returned pointer. If the values differ the cell is left
    /// untouched and no writer lock is taken, so it is cheaper than
    /// [`update`](Self::update) for a conditional write.
    #[inline]
    pub fn compare_and_swap(&self, current: &Weak<T>, new: Option<&Weak<T>>) -> bool {
        // Safety: the returned pointer is neither dereferenced nor turned into
        // a `Weak`, and both arguments are valid for the duration of the call.
        unsafe {
            self.compare_exchange(
                Weak::as_ptr(current),
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
        }
        .is_ok()
    }

    /// Stores the optional `Weak` ref `new` into the RcuWeak if the current
    /// value is the same as `current`, compared by the address of the target.
    ///
    /// An empty cell and a dangling weak (`Weak::new()`) are considered the same,
    /// so `current` may be a pointer to a dangling weak, and a dangling `new`
    /// clears the cell.
    ///
    /// The return value is a result indicating whether the new value was written and
    /// containing the previous value. On success this value is guaranteed to be equal
    /// to `current`.
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
    /// # Safety
    ///
    /// don't deref the returned pointer, and don't turn it back into a `Weak` with
    /// `Weak::from_raw`: the exchange releases the old weak ref, so the allocation
    /// may be deallocated by other threads
    ///
    /// # Examples
    ///
    /// ```
    /// use rcu_cell::RcuWeak;
    /// use std::sync::{Arc, Weak};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// let a = Arc::new(1234);
    /// let b = Arc::new(5678);
    /// let w = RcuWeak::from(Arc::downgrade(&a));
    /// let wb = Arc::downgrade(&b);
    ///
    /// let curr = Weak::as_ptr(&w.read());
    /// let res = unsafe { w.compare_exchange(curr, Some(&wb), SeqCst, SeqCst) };
    /// assert!(res.is_ok());
    /// assert_eq!(w.upgrade().as_deref(), Some(&5678));
    /// ```
    pub unsafe fn compare_exchange<'a>(
        &self,
        current: *const T,
        new: Option<&'a Weak<T>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*const T, *const T>
    where
        T: 'a,
    {
        // an empty cell and a dangling weak are the same, both are stored as an empty link
        let current_ptr = match NonNull::new(current as *mut T) {
            Some(ptr) if !is_dangling(ptr.as_ptr()) => Some(ptr),
            _ => None,
        };

        // Take the cell's weak ref before the pointer is published: once the link
        // holds it, a concurrent writer may swap it out and drop it, so the cell
        // must already own it. The ref is released again if the exchange fails.
        let new_ptr = new.and_then(|data| {
            let ptr = Weak::as_ptr(data);
            // a dangling weak is stored as an empty link
            if is_dangling(ptr) {
                return None;
            }
            self.link.check_meta(ptr);
            Some(unsafe { NonNull::new_unchecked(Weak::into_raw(Weak::clone(data)) as *mut T) })
        });

        let to_raw = |ptr: Option<NonNull<T>>| match ptr {
            Some(p) => p.as_ptr() as *const T,
            None => self.link.null(),
        };

        match self
            .link
            .compare_exchange(current_ptr, new_ptr, success, failure)
        {
            Ok(old) => {
                // drop the weak ref held by the cell
                let _ = ptr_to_weak(old);
                Ok(current)
            }
            Err(actual) => {
                // we failed to exchange, so give the weak ref we took back
                if let Some(ptr) = new_ptr {
                    drop(unsafe { Weak::from_raw(ptr.as_ptr()) });
                }
                Err(to_raw(actual))
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

    /// upgrade the innner weak value to an Arc value
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
