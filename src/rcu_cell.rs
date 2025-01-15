use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::link::LinkWrapper;
use crate::ArcPointer;

#[inline]
fn ptr_to_arc<T>(ptr: *const T) -> Option<Arc<T>> {
    unsafe { ArcPointer::from_raw(ptr) }
}

/// RCU cell, it behaves like `RwLock<Option<Arc<T>>>`
#[derive(Debug)]
pub struct RcuCell<T> {
    link: LinkWrapper<T>,
}

unsafe impl<T: Send> Send for RcuCell<T> {}
unsafe impl<T: Send + Sync> Sync for RcuCell<T> {}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        let ptr = self.link.get_ref();
        let _ = ptr_to_arc(ptr);
    }
}

impl<T> Default for RcuCell<T> {
    fn default() -> Self {
        RcuCell::none()
    }
}

impl<T> From<Arc<T>> for RcuCell<T> {
    fn from(data: Arc<T>) -> Self {
        let arc_ptr = Arc::into_raw(data);
        RcuCell {
            link: LinkWrapper::new(arc_ptr),
        }
    }
}

impl<T> From<Option<Arc<T>>> for RcuCell<T> {
    fn from(data: Option<Arc<T>>) -> Self {
        let ptr = data.into_raw();
        RcuCell {
            link: LinkWrapper::new(ptr),
        }
    }
}

impl<T> RcuCell<T> {
    /// create an empty rcu cell instance
    #[inline]
    pub const fn none() -> Self {
        RcuCell {
            link: LinkWrapper::new(ptr::null()),
        }
    }

    /// create rcu cell from a value
    #[inline]
    pub fn some(data: T) -> Self {
        let ptr = Arc::into_raw(Arc::new(data));
        RcuCell {
            link: LinkWrapper::new(ptr),
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
        let new_ptr = data.into_raw();
        ptr_to_arc(self.link.update(new_ptr))
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
        let old_value = ptr_to_arc(ptr);
        let new_ptr = match f(old_value.clone()) {
            Some(data) => Arc::into_raw(data.into()),
            None => ptr::null_mut(),
        };
        self.link.unlock_update(new_ptr);
        old_value
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
        let new_ptr = match new {
            Some(data) => Arc::as_ptr(data),
            None => ptr::null(),
        };

        self.link
            .compare_exchange(current, new_ptr, success, failure)
            .inspect(|ptr| {
                // we have succeed to exchange the arc
                if let Some(v) = new {
                    // clone and forget the arc that hold by rcu cell
                    core::mem::forget(Arc::clone(v));
                    // drop the old arc in the ruc cell
                    let _ = ptr_to_arc(ptr);
                }
            })
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
        self.link.get_ref() == Arc::as_ptr(data)
    }

    /// check if two RcuCell instances point to the same inner Arc
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.link.get_ref() == other.link.get_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_test() {
        use Ordering::SeqCst;
        let a = RcuCell::new(1234);

        let curr = a.read().as_ptr();
        let res1 = unsafe { a.compare_exchange(curr, None, SeqCst, SeqCst) }.unwrap();
        assert_eq!(res1, curr);
        let res2 = unsafe { a.compare_exchange(res1, Some(&Arc::new(5678)), SeqCst, SeqCst) };
        assert!(res2.is_err());
    }
}
