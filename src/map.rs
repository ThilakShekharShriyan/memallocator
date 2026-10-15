//! Three maps over `u64` keys: one mutex, sharded mutexes, and a fixed
//! open-addressing table that only uses atomics.
//!
//! The lock-free map does not delete. A slot moves `empty -> claimed -> ready`
//! and then stays ready. That avoids hazard pointers. Key `0` is reserved so
//! an empty key cell cannot be confused with a stored key; the state word is
//! still what readers synchronize on.
//!
//! A reader Acquire-loads `ready`. That synchronizes with the writer's Release
//! store of `ready`, which happens after the key and value stores.

#![deny(unsafe_code)]

use crate::pad::CachePadded;
use crate::syncutil::spin_lock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const EMPTY: u64 = 0;
const CLAIMED: u64 = 1;
const READY: u64 = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct MapFull;

pub struct MutexMap {
    inner: Mutex<HashMap<u64, u64>>,
}

impl MutexMap {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, key: u64, value: u64) {
        spin_lock(&self.inner).insert(key, value);
    }

    pub fn get(&self, key: u64) -> Option<u64> {
        spin_lock(&self.inner).get(&key).copied()
    }
}

impl Default for MutexMap {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShardedMap {
    shards: Vec<CachePadded<Mutex<HashMap<u64, u64>>>>,
    mask: usize,
}

impl ShardedMap {
    pub fn new(shards: usize) -> Self {
        let shards = shards.max(1).next_power_of_two();
        Self {
            shards: (0..shards)
                .map(|_| CachePadded::new(Mutex::new(HashMap::new())))
                .collect(),
            mask: shards - 1,
        }
    }

    pub fn insert(&self, key: u64, value: u64) {
        spin_lock(&self.shards[self.shard(key)].value).insert(key, value);
    }

    pub fn get(&self, key: u64) -> Option<u64> {
        spin_lock(&self.shards[self.shard(key)].value)
            .get(&key)
            .copied()
    }

    fn shard(&self, key: u64) -> usize {
        mix(key) & self.mask
    }
}

struct Slot {
    state: AtomicU64,
    key: AtomicU64,
    value: AtomicU64,
}

impl Slot {
    fn empty() -> Self {
        Self {
            state: AtomicU64::new(EMPTY),
            key: AtomicU64::new(0),
            value: AtomicU64::new(0),
        }
    }
}

pub struct LockFreeMap {
    slots: Box<[Slot]>,
    mask: usize,
}

impl LockFreeMap {
    pub fn new(min_slots: usize) -> Self {
        let slots = min_slots.max(2).next_power_of_two();
        Self {
            slots: (0..slots)
                .map(|_| Slot::empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: slots - 1,
        }
    }

    pub fn insert(&self, key: u64, value: u64) -> Result<(), MapFull> {
        assert!(key != 0, "key 0 is reserved");
        let start = mix(key) & self.mask;
        let mut index = start;
        loop {
            let slot = &self.slots[index];
            loop {
                match slot.state.load(Ordering::Acquire) {
                    READY => {
                        if slot.key.load(Ordering::Relaxed) == key {
                            slot.value.store(value, Ordering::Release);
                            return Ok(());
                        }
                        break;
                    }
                    EMPTY => {
                        match slot.state.compare_exchange(
                            EMPTY,
                            CLAIMED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                slot.key.store(key, Ordering::Relaxed);
                                slot.value.store(value, Ordering::Relaxed);
                                slot.state.store(READY, Ordering::Release);
                                return Ok(());
                            }
                            Err(_) => continue,
                        }
                    }
                    CLAIMED => {
                        std::hint::spin_loop();
                        continue;
                    }
                    _ => unreachable!("slot state"),
                }
            }
            index = (index + 1) & self.mask;
            if index == start {
                return Err(MapFull);
            }
        }
    }

    pub fn get(&self, key: u64) -> Option<u64> {
        if key == 0 {
            return None;
        }
        let start = mix(key) & self.mask;
        let mut index = start;
        loop {
            let slot = &self.slots[index];
            match slot.state.load(Ordering::Acquire) {
                CLAIMED => {
                    std::hint::spin_loop();
                    continue;
                }
                EMPTY => return None,
                READY => {
                    if slot.key.load(Ordering::Relaxed) == key {
                        return Some(slot.value.load(Ordering::Acquire));
                    }
                }
                _ => unreachable!("slot state"),
            }
            index = (index + 1) & self.mask;
            if index == start {
                return None;
            }
        }
    }
}

fn mix(key: u64) -> usize {
    let mut z = key.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    (z ^ (z >> 31)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn lockfree_insert_get_and_update() {
        let map = LockFreeMap::new(64);
        map.insert(7, 1).unwrap();
        assert_eq!(map.get(7), Some(1));
        map.insert(7, 2).unwrap();
        assert_eq!(map.get(7), Some(2));
        assert_eq!(map.get(8), None);
    }

    #[test]
    fn concurrent_disjoint_inserts() {
        let map = Arc::new(LockFreeMap::new(4096));
        thread::scope(|scope| {
            for thread_id in 0..4u64 {
                let map = Arc::clone(&map);
                scope.spawn(move || {
                    for index in 1..500u64 {
                        let key = thread_id * 1_000 + index;
                        map.insert(key, key ^ 0x55).unwrap();
                        assert_eq!(map.get(key), Some(key ^ 0x55));
                    }
                });
            }
        });
        for thread_id in 0..4u64 {
            for index in 1..500u64 {
                let key = thread_id * 1_000 + index;
                assert_eq!(map.get(key), Some(key ^ 0x55));
            }
        }
    }

    #[test]
    fn sharded_and_mutex_roundtrip() {
        let mutex = MutexMap::new();
        let sharded = ShardedMap::new(8);
        mutex.insert(3, 9);
        sharded.insert(3, 9);
        assert_eq!(mutex.get(3), Some(9));
        assert_eq!(sharded.get(3), Some(9));
    }
}
