use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::link::{ptr_to_arc, LinkWrapper};

//---------------------------------------------------------------------------------------
// RcuCell
//---------------------------------------------------------------------------------------

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
        match data {
            Some(data) => {
                let arc_ptr = Arc::into_raw(data);
                RcuCell {
                    link: LinkWrapper::new(arc_ptr),
                }
            }
            None => RcuCell::none(),
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
        let new_ptr = match data {
            Some(data) => Arc::into_raw(data),
            None => ptr::null_mut(),
        };
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
