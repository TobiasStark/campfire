//! Locks without poison cascades (I-4).
//!
//! `.lock().unwrap()` turns one panic while a lock is held into a panic on every later access.
//! The protected data here is always left in a consistent state between statements, so the
//! guard of a poisoned lock is used as is.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait Locked<T: ?Sized> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub trait RwLocked<T: ?Sized> {
    fn read_locked(&self) -> RwLockReadGuard<'_, T>;
    fn write_locked(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLocked<T> for RwLock<T> {
    fn read_locked(&self) -> RwLockReadGuard<'_, T> {
        self.read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn write_locked(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_lock_stays_usable() {
        let m = std::sync::Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(m.is_poisoned());
        *m.locked() += 1;
        assert_eq!(*m.locked(), 2);
    }
}
