//! Lock-free chained hash map with delete.
//!
//! The map in `map` never reuses a slot, so it never frees one. This map
//! unlinks a node and hands that same allocation to the epoch collector. A
//! reader only follows a pointer while it holds a `Guard`.
//!
//! Each bucket is a Harris list sorted by key. The low bit of `next` means
//! "logically deleted." The thread that swings the predecessor over the node
//! is the one that retires it. Nodes are allocator-aligned, so the low bit
//! of the pointer is free.

use crate::epoch::{Collector, Guard, LocalHandle};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

struct Node {
    key: u64,
    value: AtomicU64,
    next: AtomicUsize,
}

enum Link {
    Bucket(usize),
    Node(*mut Node),
}

struct Found {
    prev: Link,
    /// Matching node, or the first live node with a greater key, or null.
    curr: *mut Node,
    matched: bool,
}

/// Shared table. Threads call `register` and then use the handle they got.
pub struct EpochMap {
    buckets: Box<[AtomicUsize]>,
    collector: Arc<Collector>,
    mask: usize,
}

/// Thread-local pin state for one map. Send it to a thread; do not share it.
pub struct MapHandle {
    map: Arc<EpochMap>,
    local: LocalHandle,
}

impl EpochMap {
    pub fn new(buckets: usize) -> Arc<Self> {
        let buckets = buckets.max(2).next_power_of_two();
        Arc::new(Self {
            buckets: (0..buckets)
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            collector: Collector::new(),
            mask: buckets - 1,
        })
    }

    pub fn register(self: &Arc<Self>) -> MapHandle {
        MapHandle {
            local: self.collector.register(),
            map: Arc::clone(self),
        }
    }

    pub fn reclaimed(&self) -> usize {
        self.collector.reclaimed()
    }
}

impl MapHandle {
    pub fn insert(&self, key: u64, value: u64) -> bool {
        let guard = self.local.pin();
        let node = Box::into_raw(Box::new(Node {
            key,
            value: AtomicU64::new(value),
            next: AtomicUsize::new(0),
        }));
        debug_assert_eq!(node as usize & 1, 0, "node pointer must be aligned");
        loop {
            let found = find(&self.map, key, &guard);
            if found.matched {
                unsafe { (*found.curr).value.store(value, Ordering::Release) };
                let marked = unsafe { (*found.curr).next.load(Ordering::Acquire) } & 1 == 1;
                if !marked {
                    // Never published, so it does not go through the epoch.
                    drop(unsafe { Box::from_raw(node) });
                    return false;
                }
                continue;
            }
            unsafe { (*node).next.store(found.curr as usize, Ordering::Release) };
            if cas_link(
                &self.map.buckets,
                &found.prev,
                found.curr as usize,
                node as usize,
            ) {
                return true;
            }
            std::hint::spin_loop();
        }
    }

    pub fn get(&self, key: u64) -> Option<u64> {
        let guard = self.local.pin();
        let found = find(&self.map, key, &guard);
        if !found.matched {
            return None;
        }
        let value = unsafe { (*found.curr).value.load(Ordering::Acquire) };
        let marked = unsafe { (*found.curr).next.load(Ordering::Acquire) } & 1 == 1;
        if marked {
            None
        } else {
            Some(value)
        }
    }

    pub fn remove(&self, key: u64) -> Option<u64> {
        let guard = self.local.pin();
        loop {
            let found = find(&self.map, key, &guard);
            if !found.matched {
                return None;
            }
            let next = unsafe { (*found.curr).next.load(Ordering::Acquire) };
            if next & 1 == 1 {
                continue;
            }
            if unsafe {
                (*found.curr)
                    .next
                    .compare_exchange(next, next | 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            } {
                continue;
            }
            let value = unsafe { (*found.curr).value.load(Ordering::Acquire) };
            if cas_link(
                &self.map.buckets,
                &found.prev,
                found.curr as usize,
                next & !1,
            ) {
                retire(&guard, found.curr);
            } else {
                // The winner of the unlink retires the node. Help, then stop.
                let _ = find(&self.map, key, &guard);
            }
            return Some(value);
        }
    }

    pub fn flush(&self) {
        self.local.flush();
    }

    /// Live keys, bucket by bucket. Chains are sorted, so a duplicate is visible.
    pub fn live_keys(&self) -> Vec<u64> {
        let _guard = self.local.pin();
        let mut keys = Vec::new();
        for bucket in self.map.buckets.iter() {
            let mut curr = bucket.load(Ordering::Acquire);
            let mut previous: Option<u64> = None;
            let mut steps = 0usize;
            while curr != 0 {
                steps += 1;
                assert!(
                    steps <= self.map.buckets.len() * 64 + 1024,
                    "cycle in bucket"
                );
                debug_assert_eq!(curr & 1, 0);
                let node = curr as *const Node;
                let next = unsafe { (*node).next.load(Ordering::Acquire) };
                if next & 1 == 0 {
                    let key = unsafe { (*node).key };
                    if let Some(previous) = previous {
                        debug_assert!(key > previous, "bucket chain is not sorted");
                    }
                    previous = Some(key);
                    keys.push(key);
                }
                curr = next & !1;
            }
        }
        keys
    }
}

fn find(map: &EpochMap, key: u64, guard: &Guard<'_>) -> Found {
    let bucket = mix(key) & map.mask;
    loop {
        let mut prev = Link::Bucket(bucket);
        let mut curr = load_link(&map.buckets, &prev);
        loop {
            if curr == 0 {
                return Found {
                    prev,
                    curr: std::ptr::null_mut(),
                    matched: false,
                };
            }
            let node = curr as *mut Node;
            let next = unsafe { (*node).next.load(Ordering::Acquire) };
            if load_link(&map.buckets, &prev) != curr {
                break;
            }
            if next & 1 == 1 {
                if cas_link(&map.buckets, &prev, curr, next & !1) {
                    retire(guard, node);
                } else {
                    break;
                }
                curr = next & !1;
                continue;
            }
            let node_key = unsafe { (*node).key };
            if node_key >= key {
                return Found {
                    prev,
                    curr: node,
                    matched: node_key == key,
                };
            }
            prev = Link::Node(node);
            curr = next;
        }
    }
}

fn load_link(buckets: &[AtomicUsize], link: &Link) -> usize {
    match *link {
        Link::Bucket(index) => buckets[index].load(Ordering::Acquire),
        Link::Node(node) => unsafe { (*node).next.load(Ordering::Acquire) & !1 },
    }
}

fn cas_link(buckets: &[AtomicUsize], link: &Link, current: usize, new: usize) -> bool {
    let atomic = match *link {
        Link::Bucket(index) => &buckets[index],
        Link::Node(node) => unsafe { &(*node).next },
    };
    atomic
        .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn retire(guard: &Guard<'_>, node: *mut Node) {
    // This thread won the unlink. `defer_destroy` frees that allocation later.
    guard.defer_destroy(node);
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
    use std::thread;

    #[test]
    fn insert_get_remove_roundtrip() {
        let map = EpochMap::new(16);
        let handle = map.register();
        assert!(handle.insert(1, 10));
        assert!(!handle.insert(1, 11));
        assert_eq!(handle.get(1), Some(11));
        assert_eq!(handle.remove(1), Some(11));
        assert_eq!(handle.get(1), None);
        assert_eq!(handle.remove(1), None);
        assert!(handle.insert(1, 12));
        assert_eq!(handle.get(1), Some(12));
        assert_eq!(handle.live_keys(), vec![1]);
    }

    #[test]
    fn chains_stay_sorted_across_threads() {
        let map = EpochMap::new(64);
        thread::scope(|scope| {
            for thread_id in 0..4u64 {
                let map = Arc::clone(&map);
                scope.spawn(move || {
                    let handle = map.register();
                    for index in 1..400u64 {
                        let key = thread_id * 1_000 + index;
                        assert!(handle.insert(key, key));
                        assert_eq!(handle.get(key), Some(key));
                        if index % 2 == 0 {
                            assert_eq!(handle.remove(key), Some(key));
                            assert_eq!(handle.get(key), None);
                        }
                    }
                    handle.flush();
                });
            }
        });
        let handle = map.register();
        for thread_id in 0..4u64 {
            for index in 1..400u64 {
                let key = thread_id * 1_000 + index;
                if index % 2 == 0 {
                    assert_eq!(handle.get(key), None);
                } else {
                    assert_eq!(handle.get(key), Some(key));
                }
            }
        }
        let mut keys = handle.live_keys();
        keys.sort_unstable();
        assert_eq!(keys.len(), 800);
        handle.flush();
        assert!(map.reclaimed() > 0, "deleted nodes were not reclaimed");
    }

    #[test]
    fn same_key_hammer_stays_consistent() {
        let map = EpochMap::new(8);
        thread::scope(|scope| {
            for thread_id in 0..4u64 {
                let map = Arc::clone(&map);
                scope.spawn(move || {
                    let handle = map.register();
                    for step in 0..1_000u64 {
                        let key = (step % 32) + 1;
                        if thread_id % 2 == 0 {
                            handle.insert(key, key);
                        } else if step % 3 == 0 {
                            handle.remove(key);
                        } else {
                            let value = handle.get(key);
                            assert!(value.is_none() || value == Some(key));
                        }
                    }
                    handle.flush();
                });
            }
        });
        let handle = map.register();
        let keys = handle.live_keys();
        for key in &keys {
            assert_eq!(handle.get(*key), Some(*key));
        }
    }
}
