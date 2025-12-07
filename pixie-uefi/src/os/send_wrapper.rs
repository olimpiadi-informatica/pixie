use core::ops::{Deref, DerefMut};

pub struct SendWrapper<T>(pub T);

// SAFETY: there are no threads.
unsafe impl<T> Send for SendWrapper<T> {}

impl<T> Deref for SendWrapper<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for SendWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
