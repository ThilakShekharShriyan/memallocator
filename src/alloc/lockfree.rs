//! Tagged Treiber freelist.
//!
//! The stack is indices into a slab that never moves, so this file does not
//! need `unsafe`. The ABA hazard is a thread popping index `i`, stalling,
//! and later CAS-ing a stale head that again says `i` after `i` was freed
//! and reused. The high 32 bits of the head are a generation. The CAS
//! compares both, so a recycled index with a new generation does not match.
//!
//! A pop Acquire-loads the head, then reads `next`. The matching push stored
//! `next` before its Release CAS on the head. That is the same edge as
//! `ordering::Published`: the link is visible once the head publish is visible.
//! `SeqCst` is not required; nothing here needs a total order across cores.

use super::tcache::{with_magazine, Magazine, Pool};
use super::{blocks_for, class_for, CLASSES, EMPTY};
use crate::pad::CachePadded;
use crate::slab::Slab;
use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(debug_assertions)]
use std::sync::atomic::AtomicBool;

struct Class {
    slab: Slab,
    next: Vec<AtomicU32>,
    /// Padded so the hot head does not share a line with the slab pointer
    /// that every successful pop reads.
    head: CachePadded<AtomicU64>,
    #[cfg(debug_assertions)]
    live: Vec<AtomicBool>,
}

struct LockFreePool {
    classes: Vec<Class>,
}

impl LockFreePool {
    fn new() -> Self {
        let classes = CLASSES
            .into_iter()
            .map(|block_size| {
                let count = blocks_for(block_size);
                let mut next = Vec::with_capacity(count as usize);
                for index in 0..count - 1 {
                    next.push(AtomicU32::new(index + 1));
                }
                next.push(AtomicU32::new(EMPTY));
                Class {
                    slab: Slab::new(block_size, count),
                    next,
                    head: CachePadded::new(AtomicU64::new(pack(0, 0))),
                    #[cfg(debug_assertions)]
                    live: (0..count).map(|_| AtomicBool::new(false)).collect(),
                }
            })
            .collect();
        Self { classes }
    }

    fn debug_claim(&self, class: usize, index: u32) {
        #[cfg(debug_assertions)]
        {
            let was_live = self.classes[class].live[index as usize].swap(true, Ordering::Relaxed);
            assert!(!was_live, "lock-free slab returned a live slot {index}");
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
            assert!(was_live, "lock-free slab double-free of slot {index}");
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

impl Pool for LockFreePool {
    fn class_count(&self) -> usize {
        self.classes.len()
    }

    fn pop_one(&self, class: usize) -> Option<u32> {
        let class = &self.classes[class];
        let mut head = class.head.value.load(Ordering::Acquire);
        loop {
            let (index, tag) = split(head);
            if index == EMPTY {
                return None;
            }
            // The Acquire on `head` synchronizes with the Release that
            // published this node, so this `next` was stored by that push.
            let next = class.next[index as usize].load(Ordering::Relaxed);
            let new_head = pack(next, tag.wrapping_add(1));
            match class.head.value.compare_exchange_weak(
                head,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(index),
                Err(actual) => {
                    head = actual;
                    std::hint::spin_loop();
                }
            }
        }
    }

    fn push_one(&self, class: usize, index: u32) {
        let class = &self.classes[class];
        let mut head = class.head.value.load(Ordering::Relaxed);
        loop {
            let (current, tag) = split(head);
            class.next[index as usize].store(current, Ordering::Relaxed);
            let new_head = pack(index, tag.wrapping_add(1));
            match class.head.value.compare_exchange_weak(
                head,
                new_head,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => {
                    head = actual;
                    std::hint::spin_loop();
                }
            }
        }
    }
}

fn pack(index: u32, tag: u32) -> u64 {
    u64::from(index) | (u64::from(tag) << 32)
}

fn split(packed: u64) -> (u32, u32) {
    (packed as u32, (packed >> 32) as u32)
}

thread_local! {
    static MAGAZINE: RefCell<Magazine<LockFreePool>> = RefCell::new(Magazine::new());
}

#[derive(Clone)]
pub struct LockFreeAlloc {
    pool: Arc<LockFreePool>,
    cache: bool,
}

impl LockFreeAlloc {
    pub fn new(cache: bool) -> Self {
        Self {
            pool: Arc::new(LockFreePool::new()),
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
            let alloc = LockFreeAlloc::new(cache);
            let mut seen = HashSet::new();
            let mut ptrs = Vec::new();
            while let Some(ptr) = alloc.allocate(16) {
                assert!(seen.insert(ptr.as_ptr() as usize), "duplicate slot");
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
        let alloc = LockFreeAlloc::new(true);
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
        let alloc = LockFreeAlloc::new(true);
        let producer = alloc.clone();
        let consumer = alloc.clone();
        let (tx, rx) = mpsc::channel();
        let make = thread::spawn(move || {
            for _ in 0..1_000 {
                let ptr = producer.allocate(128).unwrap();
                tx.send(ptr.as_ptr() as usize).unwrap();
            }
        });
        let drop_them = thread::spawn(move || {
            for _ in 0..1_000 {
                let addr = rx.recv().unwrap();
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                consumer.deallocate(ptr, 128);
            }
        });
        make.join().unwrap();
        drop_them.join().unwrap();
        assert_eq!(drain(&alloc), alloc.block_capacity());
    }

    #[test]
    fn contended_alloc_free() {
        let alloc = LockFreeAlloc::new(false);
        thread::scope(|scope| {
            for _ in 0..8 {
                let alloc = alloc.clone();
                scope.spawn(move || {
                    for _ in 0..20_000 {
                        let ptr = alloc.allocate(64).expect("pool");
                        unsafe {
                            ptr.as_ptr().write_volatile(0x5A);
                        }
                        alloc.deallocate(ptr, 64);
                    }
                });
            }
        });
        assert_eq!(drain(&alloc), alloc.block_capacity());
    }

    fn stress(alloc: &LockFreeAlloc, seed: u64) {
        let mut state = seed.wrapping_add(0x51ED);
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

    fn drain(alloc: &LockFreeAlloc) -> usize {
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
