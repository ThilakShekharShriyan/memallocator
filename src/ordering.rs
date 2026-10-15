#![deny(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A value published to another thread with release/acquire.
///
/// This is the same edge the freelist and the queue use. The writer stores
/// the payload, then Release-stores a flag. The reader Acquire-loads the flag
/// and only then reads the payload. That Acquire synchronizes with the
/// Release, so the payload store happens-before the payload load.
///
/// On Apple Silicon those are `stlr` / `ldar`. `Relaxed` would compile to a
/// plain load/store, and the reader could observe `ready == true` while still
/// seeing the old payload. `SeqCst` would also be correct here and would add a
/// heavier barrier that this pattern does not need.
pub struct Published {
    value: AtomicU64,
    ready: AtomicBool,
}

impl Published {
    pub fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
            ready: AtomicBool::new(false),
        }
    }

    pub fn publish(&self, value: u64) {
        self.value.store(value, Ordering::Relaxed);
        self.ready.store(true, Ordering::Release);
    }

    pub fn read(&self) -> Option<u64> {
        if self.ready.load(Ordering::Acquire) {
            Some(self.value.load(Ordering::Relaxed))
        } else {
            None
        }
    }
}

impl Default for Published {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn reader_sees_the_published_value() {
        let cell = Arc::new(Published::new());
        let writer = Arc::clone(&cell);
        let handle = thread::spawn(move || {
            writer.publish(0x1111_2222_3333_4444);
        });
        while cell.read().is_none() {
            std::hint::spin_loop();
        }
        assert_eq!(cell.read(), Some(0x1111_2222_3333_4444));
        handle.join().unwrap();
    }
}
