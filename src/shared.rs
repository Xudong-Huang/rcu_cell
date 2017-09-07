use std::fmt;
use std::marker::PhantomData;

pub struct Shared<T> {
    pointer: *mut T,
    _marker: PhantomData<T>,
}

impl<T> Shared<T> {
    /// Creates a new `Shared` if `ptr` is non-null.
    pub fn new(ptr: *mut T) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Shared {
                pointer: ptr,
                _marker: PhantomData,
            })
        }
    }

    /// Acquires the underlying `*mut` pointer.
    pub fn as_ptr(self) -> *mut T {
        self.pointer
    }

    /// Dereferences the content.
    pub unsafe fn as_ref(&self) -> &T {
        &*self.as_ptr()
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Shared<T> {}

impl<T> fmt::Pointer for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Pointer::fmt(&self.as_ptr(), f)
    }
}
