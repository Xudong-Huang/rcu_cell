use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::link::LinkWrapper;
use crate::ArcPointer;

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
///   (`target_has_atomic = "128"`, e.g. aarch64 and s390x, but not x86_64) the
///   address and the metadata are stored in one `core::sync::atomic::AtomicU128`,
///   so values with different concrete types or lengths can be swapped freely and
///   the metadata check doesn't apply. `build.rs` detects this automatically.
/// - Sized cells always use a single `AtomicUsize`, they pay nothing for this.
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
    pub fn update<R, F>(&self, f: F) -> Option<Arc<T>>
    where
        F: FnOnce(Option<Arc<T>>) -> Option<R>,
        R: Into<Arc<T>>,
    {
        // increase ref count to lock the inner Arc
        let ptr = self.link.lock_read();
        // the cell still owns this Arc until the update is done, so a panic
        // in `f` or in the metadata check must not drop it
        let old_value = ManuallyDrop::new(ptr_to_arc(ptr));
        let new_ptr = f((*old_value).clone()).map(|data| {
            let data = data.into();
            self.link.check_meta(Arc::as_ptr(&data));
            arc_into_ptr(data)
        });
        self.link.unlock_update(new_ptr);
        ManuallyDrop::into_inner(old_value)
    }

    /// Stores the optional Arc ref `new` into the RcuCell if the current
    /// value is the same as `current`. The tag is also taken into account, so two pointers to the
    /// same object, but with different tags, will not be considered equal.
    ///
    /// The return value is a result indicating whether the new value was written and containing the previous value.
    /// On success this value is guaranteed to be equal to current.
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
    /// don't deref the returned pointer, it's may be dropped by other threads
    ///
    /// # Examples
    ///
    /// ```
    /// use rcu_cell::{RcuCell, ArcPointer};
    ///
    /// use std::sync::Arc;
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// let a = RcuCell::new(1234);
    ///
    /// let curr = a.read();
    /// let res1 = unsafe { a.compare_exchange(curr.as_ptr(), None, SeqCst, SeqCst) }.unwrap();
    /// let res2 = unsafe { a.compare_exchange(res1, Some(&Arc::new(5678)), SeqCst, SeqCst) };
    /// ```
    pub unsafe fn compare_exchange<'a>(
        &self,
        current: *const T,
        new: Option<&'a Arc<T>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*const T, *const T>
    where
        T: 'a,
    {
        let new_ptr = new.map(|data| {
            let ptr = Arc::as_ptr(data);
            self.link.check_meta(ptr);
            unsafe { NonNull::new_unchecked(ptr as *mut T) }
        });
        let current = NonNull::new(current as *mut T);

        let to_raw = |ptr: Option<NonNull<T>>| match ptr {
            Some(p) => p.as_ptr() as *const T,
            None => self.link.null(),
        };

        self.link
            .compare_exchange(current, new_ptr, success, failure)
            .inspect(|ptr| {
                // drop the old arc in the rcu cell
                let _ = ptr_to_arc(*ptr);
                // we have succeed to exchange the arc
                if let Some(v) = new {
                    // clone and forget the arc that hold by rcu cell
                    let _ = Arc::into_raw(Arc::clone(v));
                }
            })
            .map(to_raw)
            .map_err(to_raw)
    }

    /// read out the inner Arc value
    #[inline]
    pub fn read(&self) -> Option<Arc<T>> {
        let ptr = self.link.inc_ref();
        let v = ManuallyDrop::new(ptr_to_arc(ptr));
        let cloned = v.as_ref().cloned();
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

    /// check if two RcuCell instances point to the same inner Arc
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        same_addr(this.link.get_ref(), other.link.get_ref())
    }
}
