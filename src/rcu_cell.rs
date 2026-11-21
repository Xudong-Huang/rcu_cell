use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::link::{LinkWrapper, UnlockOnPanic};

#[inline]
fn ptr_to_arc<T: ?Sized>(ptr: Option<NonNull<T>>) -> Option<Arc<T>> {
    ptr.map(|p| unsafe { Arc::from_raw(p.as_ptr()) })
}

#[inline]
fn arc_into_ptr<T: ?Sized>(data: Arc<T>) -> NonNull<T> {
    unsafe { NonNull::new_unchecked(Arc::into_raw(data) as *mut T) }
}

#[inline]
fn same_addr<T: ?Sized>(a: Option<NonNull<T>>, b: Option<NonNull<T>>) -> bool {
    a.map(|p| p.cast::<()>()) == b.map(|p| p.cast::<()>())
}

/// RCU cell, it behaves like `RwLock<Option<Arc<T>>>`
///
/// # Unsized types
///
/// `T` may be unsized (`dyn Trait`, `[U]`, `str`), created from an existing `Arc`:
///
/// ```
/// use rcu_cell::RcuCell;
/// use std::sync::Arc;
///
/// trait Shape: Send + Sync {}
/// struct Square(u32);
/// impl Shape for Square {}
///
/// let a: Arc<dyn Shape> = Arc::new(Square(1));
/// let cell: RcuCell<dyn Shape> = RcuCell::from(a);
/// ```
///
/// Limitations:
/// - `some()` and `new()` need `T: Sized`, an unsized cell is created from an
///   existing `Arc`, or empty with `none()` (or `Default`). An empty unsized cell
///   has no pointer metadata yet, the first value written decides it.
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
pub struct RcuCell<T: ?Sized> {
    link: LinkWrapper<T>,
}

unsafe impl<T: ?Sized + Send + Sync> Send for RcuCell<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RcuCell<T> {}

impl<T: ?Sized> Drop for RcuCell<T> {
    fn drop(&mut self) {
        let ptr = self.link.get_ref();
        let _ = ptr_to_arc(ptr);
    }
}

impl<T: ?Sized> Default for RcuCell<T> {
    fn default() -> Self {
        RcuCell::none()
    }
}

impl<T: ?Sized> From<Arc<T>> for RcuCell<T> {
    fn from(data: Arc<T>) -> Self {
        let arc_ptr = Some(arc_into_ptr(data));
        RcuCell {
            link: LinkWrapper::from_ptr(arc_ptr),
        }
    }
}

impl<T> From<Option<Arc<T>>> for RcuCell<T> {
    fn from(data: Option<Arc<T>>) -> Self {
        let ptr = data.map(arc_into_ptr);
        RcuCell {
            link: LinkWrapper::from_ptr(ptr),
        }
    }
}

impl<T> RcuCell<T> {
    /// create rcu cell from a value
    #[inline]
    pub fn some(data: T) -> Self {
        let ptr = Some(arc_into_ptr(Arc::new(data)));
        RcuCell {
            link: LinkWrapper::from_ptr(ptr),
        }
    }

    /// create rcu cell from value that can be converted to Option<T>
    #[inline]
    pub fn new(data: impl Into<Option<T>>) -> Self {
        let data = data.into();
        match data {
            Some(data) => Self::some(data),
            None => Self::none(),
        }
    }
}

impl<T: ?Sized> RcuCell<T> {
    /// create an empty rcu cell instance
    #[inline]
    pub const fn none() -> Self {
        RcuCell {
            link: LinkWrapper::new(),
        }
    }

    /// convert the rcu cell to an Arc value
    #[inline]
    pub fn into_arc(self) -> Option<Arc<T>> {
        let ptr = self.link.get_ref();
        let ret = ptr_to_arc(ptr);
        let _ = ManuallyDrop::new(self);
        ret
    }

    /// check if the rcu cell is empty
    #[inline]
    pub fn is_none(&self) -> bool {
        self.link.is_none()
    }

    /// write an option arc value to the rcu cell and return the old value
    #[inline]
    pub fn set(&self, data: Option<Arc<T>>) -> Option<Arc<T>> {
        let new_ptr = self.arc_to_ptr(data);
        ptr_to_arc(self.link.update(new_ptr))
    }

    #[inline]
    fn arc_to_ptr(&self, data: Option<Arc<T>>) -> Option<NonNull<T>> {
        data.map(|data| {
            self.link.check_meta(Arc::as_ptr(&data));
            arc_into_ptr(data)
        })
    }

    /// take the value from the rcu cell, leave the rcu cell empty
    #[inline]
    pub fn take(&self) -> Option<Arc<T>> {
        self.set(None)
    }

    /// write a value to the rcu cell and return the old value
    #[inline]
    pub fn write(&self, data: impl Into<Arc<T>>) -> Option<Arc<T>> {
        let data = data.into();
        self.set(Some(data))
    }

    /// Atomicly update the value with a closure and return the old value.
    /// The closure will be called with the old value and return the new value.
    /// The closure should not take too long time, internally it's use a spin
    /// lock to prevent other writer to update the value
    ///
    /// The closure borrows the old value, so no `Arc` refcount increment or
    /// decrement is needed while the lock is held. If you need to own the old
    /// `Arc`, call `.cloned()` on it inside the closure.
    pub fn update<R, F>(&self, f: F) -> Option<Arc<T>>
    where
        F: FnOnce(Option<&Arc<T>>) -> Option<R>,
        R: Into<Arc<T>>,
    {
        // increase ref count to lock the inner Arc
        let ptr = self.link.lock_read();
        // the cell still owns this Arc until the update is done, so a panic
        // in `f` or in the metadata check must not drop it
        let old_value = ManuallyDrop::new(ptr_to_arc(ptr));
        // if `f` panics, put the old value back and release the lock, otherwise
        // every later writer would spin forever
        let unlock = UnlockOnPanic {
            link: &self.link,
            ptr,
        };
        let new_ptr = f((*old_value).as_ref()).map(|data| {
            let data = data.into();
            self.link.check_meta(Arc::as_ptr(&data));
            arc_into_ptr(data)
        });
        // if `f` panics, the `UnlockOnPanic` will release the link
        // and prevent other writers from spinning forever.
        // Otherwise, we manually unlock the link after updating it.
        core::mem::forget(unlock);
        self.link.unlock_update(new_ptr);
        ManuallyDrop::into_inner(old_value)
    }

    /// Stores the optional Arc ref `new` into the RcuCell if the cell currently
    /// holds `current`, compared by the address of the target. `current` is
    /// `None` for an empty cell.
    ///
    /// `new` is taken by value, so the caller's ref is moved into the cell
    /// without a refcount change.
    ///
    /// On success the previous value is returned as an owned `Arc` (an empty
    /// cell is `None`), so the caller drops it instead of the cell.
    ///
    /// On failure the cell is left untouched and `new` is returned back, so the
    /// caller can reuse it for another attempt. `Err(None)` means the caller
    /// passed `None` as `new`.
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
    /// use rcu_cell::RcuCell;
    ///
    /// use std::sync::Arc;
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// let a = RcuCell::new(1234);
    ///
    /// let curr = a.read();
    /// let res1 = a.compare_exchange(curr.as_ref(), None, SeqCst, SeqCst).unwrap();
    /// let res2 = a.compare_exchange(res1.as_ref(), Some(Arc::new(5678)), SeqCst, SeqCst);
    /// ```
    pub fn compare_exchange(
        &self,
        current: Option<&Arc<T>>,
        new: Option<Arc<T>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Option<Arc<T>>, Option<Arc<T>>> {
        // The caller hands its ref over before the pointer is published: once the
        // link holds it, a concurrent writer may swap it out and drop it, and that
        // is fine because the cell owns that ref. If the exchange fails, the ref
        // is taken back and dropped.
        let new_ptr = new.map(|data| {
            let ptr = Arc::as_ptr(&data);
            self.link.check_meta(ptr);
            unsafe { NonNull::new_unchecked(Arc::into_raw(data) as *mut T) }
        });
        // a reference to an Arc is never null, so the address is a valid non-null pointer
        let current = current.map(|data| NonNull::from(&**data));

        // Safety: `new_ptr` was built by `Arc::into_raw` and is owned by the
        // cell (or dropped below on failure), and `current` is only compared as
        // an address, never dereferenced.
        match unsafe {
            self.link
                .compare_exchange(current, new_ptr, success, failure)
        } {
            Ok(old) => Ok(ptr_to_arc(old)),
            Err(_) => {
                // the exchange failed, so the cell never took the ref we made
                // with `Arc::into_raw`; hand the caller's `new` back unwrapped
                Err(new_ptr.map(|ptr| unsafe { Arc::from_raw(ptr.as_ptr()) }))
            }
        }
    }

    /// read out the inner Arc value
    #[inline]
    pub fn read(&self) -> Option<Arc<T>> {
        // `inc_ref`/`dec_ref` is the read-side critical section: the pointer is only
        // borrowed, and a concurrent writer could drop the old `Arc` after swapping it
        // out. The clone must stay inside so the writer waits for it to finish.
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_arc(ptr));
        let cloned = v.as_ref().cloned();
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

    /// check if two RcuCell instances point to the same inner Arc
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        same_addr(this.link.get_ref(), other.link.get_ref())
    }
}
