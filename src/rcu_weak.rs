use alloc::sync::{Arc, Weak};
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::link::LinkWrapper;

#[inline]
fn ptr_to_weak<T>(ptr: *const T) -> Weak<T> {
    if ptr.is_null() {
        Weak::new()
    } else {
        unsafe { Weak::from_raw(ptr) }
    }
}

/// RCU weak cell, it behaves like `RwLock<Weak<T>>`
#[derive(Debug)]
pub struct RcuWeak<T> {
    link: LinkWrapper<T>,
}

unsafe impl<T: Send + Sync> Send for RcuWeak<T> {}
unsafe impl<T: Send + Sync> Sync for RcuWeak<T> {}

impl<T> Drop for RcuWeak<T> {
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

impl<T> From<Weak<T>> for RcuWeak<T> {
    fn from(data: Weak<T>) -> Self {
        let weak_ptr = Weak::into_raw(data);
        RcuWeak {
            link: LinkWrapper::new(weak_ptr),
        }
    }
}

impl<T> RcuWeak<T> {
    /// create an dummy rcu weak cell instance, upgrade from it will return None
    #[inline]
    pub const fn new() -> Self {
        RcuWeak {
            link: LinkWrapper::new(ptr::null()),
        }
    }

    /// convert the rcu weak to a `Weak`` value
    #[inline]
    pub fn into_weak(self) -> Weak<T> {
        let ptr = self.link.get_ref();
        let ret = ptr_to_weak(ptr);
        let _ = ManuallyDrop::new(self);
        ret
    }

    /// take the value from the rcu weak, leave the rcu weak with default value
    #[inline]
    pub fn take(&self) -> Weak<T> {
        ptr_to_weak(self.link.update(ptr::null()))
    }

    /// write a new weak value to the rcu weak cell and return the old value
    #[inline]
    pub fn write(&self, data: Weak<T>) -> Weak<T> {
        let new_ptr = if data.ptr_eq(&Weak::new()) {
            ptr::null()
        } else {
            Weak::into_raw(data)
        };
        ptr_to_weak(self.link.update(new_ptr))
    }

    /// write a new `Weak` value downgrade from the `Arc`` to the cell and return the old value
    #[inline]
    pub fn write_arc(&self, data: &Arc<T>) -> Weak<T> {
        let weak = Arc::downgrade(data);
        let new_ptr = Weak::into_raw(weak);
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
        core::ptr::eq(self.link.get_ref(), Arc::as_ptr(data))
    }

    /// read inner ptr and check if it is the same as the given Weak
    #[inline]
    pub fn weak_eq(&self, data: &Weak<T>) -> bool {
        core::ptr::eq(self.link.get_ref(), Weak::as_ptr(data))
    }

    /// check if two RcuWeak instances point to the same inner Weak
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        core::ptr::eq(this.link.get_ref(), other.link.get_ref())
    }
}
