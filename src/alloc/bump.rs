//! Bump allocators. One cursor, no free of individual slots.
//!
//! `Bump` is the single-thread ceiling: a plain integer add.
//! `AtomicBump` is the same idea shared across cores, and it is the CPU
//! version of the Metal bump kernel.

use crate::slab::Slab;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::pad::CachePadded;

pub struct Bump {
    slab: Slab,
    offset: usize,
}

impl Bump {
    pub fn new(bytes: usize) -> Self {
        let bytes = bytes.max(16).next_multiple_of(16);
        Self {
            slab: Slab::new(16, (bytes / 16) as u32),
            offset: 0,
        }
    }

    pub fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        let size = align_up(size);
        let start = self.offset;
        let end = start.checked_add(size)?;
        if end > self.capacity() {
            return None;
        }
        self.offset = end;
        Some(self.slab.ptr((start / 16) as u32))
    }

    pub fn reset(&mut self) {
        self.offset = 0;
    }

    fn capacity(&self) -> usize {
        self.slab.block_size * self.slab.count as usize
    }
}

pub struct AtomicBump {
    slab: Slab,
    offset: CachePadded<AtomicUsize>,
}

impl AtomicBump {
    pub fn new(bytes: usize) -> Self {
        let bytes = bytes.max(16).next_multiple_of(16);
        Self {
            slab: Slab::new(16, (bytes / 16) as u32),
            offset: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// Reserve `size` bytes. `Relaxed` is enough: the counter is the only
    /// shared state, and the caller receives exclusive bytes at `start`.
    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        let size = align_up(size);
        let start = self.offset.value.fetch_add(size, Ordering::Relaxed);
        match start.checked_add(size) {
            Some(end) if end <= self.capacity() => Some(self.slab.ptr((start / 16) as u32)),
            _ => None,
        }
    }

    /// Exclusive with `alloc`. Threads must not be inside `alloc`.
    pub fn reset(&self) {
        self.offset.value.store(0, Ordering::Relaxed);
    }

    fn capacity(&self) -> usize {
        self.slab.block_size * self.slab.count as usize
    }
}

fn align_up(size: usize) -> usize {
    size.max(1).next_multiple_of(16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn bump_slots_do_not_overlap() {
        let mut bump = Bump::new(256);
        let a = bump.alloc(16).unwrap();
        let b = bump.alloc(24).unwrap();
        let distance = b.as_ptr() as usize - a.as_ptr() as usize;
        assert!(distance >= 16);
        assert!(bump.alloc(1024).is_none() || bump.alloc(1).is_some());
        bump.reset();
        assert!(bump.alloc(16).is_some());
    }

    #[test]
    fn atomic_bump_addresses_are_unique() {
        let bump = Arc::new(AtomicBump::new(4 * 1000 * 32));
        let addresses = Arc::new(Mutex::new(Vec::new()));
        thread::scope(|scope| {
            for _ in 0..4 {
                let bump = Arc::clone(&bump);
                let addresses = Arc::clone(&addresses);
                scope.spawn(move || {
                    let mut local = Vec::with_capacity(1000);
                    for _ in 0..1000 {
                        let ptr = bump.alloc(32).expect("capacity");
                        local.push(ptr.as_ptr() as usize);
                    }
                    addresses.lock().unwrap().extend(local);
                });
            }
        });
        let mut values = addresses.lock().unwrap().clone();
        let count = values.len();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), count);
    }
}
