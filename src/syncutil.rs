#![deny(unsafe_code)]

use std::sync::{Mutex, MutexGuard, TryLockError};

/// Lock without parking.
///
/// The allocator and queue critical sections are a handful of stores. A
/// blocking `lock` would put threads to sleep and the benchmark would measure
/// the scheduler. `try_lock` on this platform is a user-space CAS that returns
/// immediately when the mutex is held, so spinning here stays in the process.
pub fn spin_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(err)) => return err.into_inner(),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
}

pub fn block_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}
