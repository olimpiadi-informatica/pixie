use core::cell::RefCell;
use core::ops::Deref;

pub struct SyncRefCell<T>(RefCell<T>);

impl<T> SyncRefCell<T> {
    pub const fn new(value: T) -> Self {
        Self(RefCell::new(value))
    }
}

unsafe impl<T> Sync for SyncRefCell<T> {}

impl<T> Deref for SyncRefCell<T> {
    type Target = RefCell<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
