//! Mutex freelist. Safe Rust: the free list lives inside the mutex.
//!
//! `spin` burns the core on `try_lock` so the critical section is not mixed
//! with thread parking. `blocking` is `Mutex::lock` and shows what happens
//! when a short freelist operation goes through the scheduler.

use super::tcache::{with_magazine, Magazine, Pool};
use super::{blocks_for, class_for, CLASSES, EMPTY};
use crate::pad::CachePadded;
use crate::slab::Slab;
use crate::syncutil::{block_lock, spin_lock};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};

struct FreeList {
    next: Vec<u32>,
    head: u32,
}

impl FreeList {
    fn new(count: u32) -> Self {
        let mut next = Vec::with_capacity(count as usize);
        for index in 0..count - 1 {
            next.push(index + 1);
        }
        next.push(EMPTY);
        Self { next, head: 0 }
    }

    fn pop_one(&mut self) -> Option<u32> {
        if self.head == EMPTY {
            return None;
        }
        let index = self.head;
        self.head = self.next[index as usize];
        Some(index)
    }

    fn push_one(&mut self, index: u32) {
        self.next[index as usize] = self.head;
        self.head = index;
    }
}

struct Class {
    slab: Slab,
    list: CachePadded<Mutex<FreeList>>,
    #[cfg(debug_assertions)]
    live: Vec<AtomicBool>,
}

struct MutexPool {
    classes: Vec<Class>,
    spin: bool,
}

impl MutexPool {
    fn new(spin: bool) -> Self {
        let classes = CLASSES
            .into_iter()
            .map(|block_size| {
                let count = blocks_for(block_size);
                Class {
                    slab: Slab::new(block_size, count),
                    list: CachePadded::new(Mutex::new(FreeList::new(count))),
                    #[cfg(debug_assertions)]
                    live: (0..count).map(|_| AtomicBool::new(false)).collect(),
                }
            })
            .collect();
        Self { classes, spin }
    }

    fn lock<'a>(&'a self, class: usize) -> MutexGuard<'a, FreeList> {
        let mutex = &self.classes[class].list.value;
        if self.spin {
            spin_lock(mutex)
        } else {
            block_lock(mutex)
        }
    }

    fn debug_claim(&self, class: usize, index: u32) {
        #[cfg(debug_assertions)]
        {
            let was_live = self.classes[class].live[index as usize].swap(true, Ordering::Relaxed);
            assert!(!was_live, "mutex slab returned a live slot {index}");
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (class, index);
        }
    }

    fn debug_release(&self, class: usize, index: u32) {
        #[cfg(debug_assertions)]
        {
            let was_live = self.classes[class].live[index as usize].swap(false, Ordering::Relaxed);
            assert!(was_live, "mutex slab double-free of slot {index}");
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (class, index);
        }
    }

    fn capacity(&self) -> usize {
        self.classes
            .iter()
            .map(|class| class.slab.count as usize)
            .sum()
    }
}

impl Pool for MutexPool {
    fn class_count(&self) -> usize {
        self.classes.len()
    }

    fn pop_one(&self, class: usize) -> Option<u32> {
        self.lock(class).pop_one()
    }

    fn push_one(&self, class: usize, index: u32) {
        self.lock(class).push_one(index);
    }

    fn pop_batch(&self, class: usize, dst: &mut Vec<u32>, max: usize) -> usize {
        let mut list = self.lock(class);
        let mut got = 0;
        while got < max {
            match list.pop_one() {
                Some(index) => {
                    dst.push(index);
                    got += 1;
                }
                None => break,
            }
        }
        got
    }

    fn push_batch(&self, class: usize, indices: &[u32]) {
        let mut list = self.lock(class);
        for &index in indices {
            list.push_one(index);
        }
    }
}

thread_local! {
    static MAGAZINE: RefCell<Magazine<MutexPool>> = RefCell::new(Magazine::new());
}

#[derive(Clone)]
pub struct MutexAlloc {
    pool: Arc<MutexPool>,
    cache: bool,
}

impl MutexAlloc {
    pub fn spin(cache: bool) -> Self {
        Self::with_mode(cache, true)
    }

    pub fn blocking(cache: bool) -> Self {
        Self::with_mode(cache, false)
    }

    fn with_mode(cache: bool, spin: bool) -> Self {
        Self {
            pool: Arc::new(MutexPool::new(spin)),
            cache,
        }
    }

    pub fn block_capacity(&self) -> usize {
        self.pool.capacity()
    }

    pub fn allocate(&self, size: usize) -> Option<NonNull<u8>> {
        let class = class_for(size)?;
        let index = if self.cache {
            MAGAZINE.with(|slot| {
                with_magazine(slot, &self.pool, |magazine| magazine.alloc_index(class))
            })?
        } else {
            self.pool.pop_one(class)?
        };
        self.pool.debug_claim(class, index);
        Some(self.pool.classes[class].slab.ptr(index))
    }

    pub fn deallocate(&self, ptr: NonNull<u8>, size: usize) {
        let class = class_for(size).expect("size is outside the slab classes");
        let index = self.pool.classes[class].slab.index_of(ptr);
        self.pool.debug_release(class, index);
        if self.cache {
            MAGAZINE.with(|slot| {
                with_magazine(slot, &self.pool, |magazine| {
                    magazine.free_index(class, index);
                });
            });
        } else {
            self.pool.push_one(class, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::blocks_for;
    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn every_block_is_returned_once() {
        for cache in [false, true] {
            let alloc = MutexAlloc::spin(cache);
            let mut seen = HashSet::new();
            let mut ptrs = Vec::new();
            while let Some(ptr) = alloc.allocate(16) {
                assert!(seen.insert(ptr.as_ptr() as usize));
                ptrs.push(ptr);
            }
            assert_eq!(ptrs.len(), blocks_for(16) as usize);
            for ptr in ptrs {
                alloc.deallocate(ptr, 16);
            }
            let mut again = 0;
            while alloc.allocate(16).is_some() {
                again += 1;
            }
            assert_eq!(again, blocks_for(16) as usize);
        }
    }

    #[test]
    fn stress_recovers_the_whole_pool() {
        let alloc = MutexAlloc::spin(true);
        thread::scope(|scope| {
            for seed in 0..4 {
                let alloc = alloc.clone();
                scope.spawn(move || stress(&alloc, seed));
            }
        });
        assert_eq!(drain(&alloc), alloc.block_capacity());
    }

    #[test]
    fn free_on_another_thread() {
        let alloc = MutexAlloc::spin(true);
        let producer = alloc.clone();
        let consumer = alloc.clone();
        let (tx, rx) = mpsc::channel();
        let make = thread::spawn(move || {
            for _ in 0..1_000 {
                let ptr = producer.allocate(64).unwrap();
                tx.send(ptr.as_ptr() as usize).unwrap();
            }
        });
        let drop_them = thread::spawn(move || {
            for _ in 0..1_000 {
                let addr = rx.recv().unwrap();
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                consumer.deallocate(ptr, 64);
            }
        });
        make.join().unwrap();
        drop_them.join().unwrap();
        assert_eq!(drain(&alloc), alloc.block_capacity());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn double_free_panics() {
        let alloc = MutexAlloc::spin(false);
        let ptr = alloc.allocate(32).unwrap();
        alloc.deallocate(ptr, 32);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            alloc.deallocate(ptr, 32);
        }));
        assert!(result.is_err());
    }

    fn stress(alloc: &MutexAlloc, seed: u64) {
        let mut state = seed.wrapping_add(0x9E37);
        let mut live = Vec::new();
        for _ in 0..20_000 {
            state = xorshift(state);
            if !live.is_empty() && state & 3 == 0 {
                let (ptr, size) = live.pop().unwrap();
                alloc.deallocate(ptr, size);
            } else if live.len() < 128 {
                let size = [16usize, 64, 256, 1024][((state >> 8) & 3) as usize];
                if let Some(ptr) = alloc.allocate(size) {
                    unsafe {
                        // SAFETY: the slot is exclusively ours until deallocate.
                        ptr.as_ptr().write(1);
                    }
                    live.push((ptr, size));
                }
            }
        }
        for (ptr, size) in live {
            alloc.deallocate(ptr, size);
        }
    }

    fn drain(alloc: &MutexAlloc) -> usize {
        let mut count = 0;
        for &size in &CLASSES {
            while alloc.allocate(size).is_some() {
                count += 1;
            }
        }
        count
    }

    fn xorshift(mut state: u64) -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}
