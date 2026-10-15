//! A spin-mutex queue and Vyukov's bounded MPMC queue.
//!
//! The mutex queue is safe Rust. The MPMC queue uses `unsafe` for the slot
//! payload: the producer that claimed a ticket writes it, and the consumer
//! that observed the matching sequence reads it. The type system cannot see
//! that handshake, so the invariant is stated on the two unsafe blocks.

use crate::pad::CachePadded;
use crate::syncutil::spin_lock;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub struct MutexQueue<T> {
    inner: Mutex<VecDeque<T>>,
    cap: usize,
}

impl<T> MutexQueue<T> {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    pub fn try_push(&self, value: T) -> Result<(), T> {
        let mut guard = spin_lock(&self.inner);
        if guard.len() >= self.cap {
            return Err(value);
        }
        guard.push_back(value);
        Ok(())
    }

    pub fn try_pop(&self) -> Option<T> {
        spin_lock(&self.inner).pop_front()
    }
}

/// Bounded multi-producer, multi-consumer queue.
///
/// `enqueue` and `dequeue` live on separate cache lines. Each cell is aligned
/// to a cache line so adjacent sequence counters do not false-share.
///
/// `sequence` is the ticket allowed to touch the cell. The producer
/// Release-stores `pos + 1` after writing the value. The consumer
/// Acquire-loads that ticket, reads the value, then Release-stores
/// `pos + capacity` so a later producer may reuse the cell.
pub struct MpmcQueue<T> {
    cells: Box<[Cell<T>]>,
    mask: usize,
    capacity: usize,
    enqueue: CachePadded<AtomicUsize>,
    dequeue: CachePadded<AtomicUsize>,
}

#[repr(align(128))]
struct Cell<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: a cell value moves from one producer thread to one consumer thread.
// The sequence protocol in `try_push` / `try_pop` is the mutual exclusion.
unsafe impl<T: Send> Sync for MpmcQueue<T> {}

impl<T: Send> MpmcQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity >= 2 && capacity.is_power_of_two());
        let cells = (0..capacity)
            .map(|index| Cell {
                sequence: AtomicUsize::new(index),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            mask: capacity - 1,
            capacity,
            enqueue: CachePadded::new(AtomicUsize::new(0)),
            dequeue: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    pub fn try_push(&self, value: T) -> Result<(), T> {
        let mut pos = self.enqueue.value.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            let sequence = cell.sequence.load(Ordering::Acquire);
            let dif = (sequence as isize).wrapping_sub(pos as isize);
            if dif == 0 {
                match self.enqueue.value.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        unsafe {
                            // SAFETY: this thread won `pos`. No other thread
                            // writes the cell until we Release-store
                            // `sequence = pos + 1` and a consumer finishes it.
                            (*cell.value.get()).write(value);
                        }
                        cell.sequence.store(pos.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(actual) => pos = actual,
                }
            } else if dif < 0 {
                return Err(value);
            } else {
                pos = self.enqueue.value.load(Ordering::Relaxed);
            }
        }
    }

    pub fn try_pop(&self) -> Option<T> {
        let mut pos = self.dequeue.value.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            let sequence = cell.sequence.load(Ordering::Acquire);
            let dif = (sequence as isize).wrapping_sub(pos.wrapping_add(1) as isize);
            if dif == 0 {
                match self.dequeue.value.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let value = unsafe {
                            // SAFETY: the producer Release-stored `pos + 1`
                            // after initializing the value. This thread won
                            // the consumer ticket for `pos`, so the read is
                            // exclusive. `assume_init_read` takes the bits;
                            // the cell is not dropped in place.
                            (*cell.value.get()).assume_init_read()
                        };
                        cell.sequence
                            .store(pos.wrapping_add(self.capacity), Ordering::Release);
                        return Some(value);
                    }
                    Err(actual) => pos = actual,
                }
            } else if dif < 0 {
                return None;
            } else {
                pos = self.dequeue.value.load(Ordering::Relaxed);
            }
        }
    }
}

impl<T> Drop for MpmcQueue<T> {
    fn drop(&mut self) {
        // Exclusive access: the borrow checker holds `&mut self`.
        while let Some(value) = self.try_pop_exclusive() {
            drop(value);
        }
    }
}

impl<T> MpmcQueue<T> {
    fn try_pop_exclusive(&mut self) -> Option<T> {
        // Same protocol as `try_pop`, split out so `Drop` does not require `T: Send`.
        let pos = *self.dequeue.value.get_mut();
        let cell = &mut self.cells[pos & self.mask];
        let sequence = *cell.sequence.get_mut();
        if (sequence as isize).wrapping_sub(pos.wrapping_add(1) as isize) != 0 {
            return None;
        }
        let value = unsafe {
            // SAFETY: sequence == pos + 1 means a producer initialized this
            // cell and no consumer has taken it. Drop has exclusive access.
            (*cell.value.get()).assume_init_read()
        };
        *self.dequeue.value.get_mut() = pos.wrapping_add(1);
        *cell.sequence.get_mut() = pos.wrapping_add(self.capacity);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn mutex_queue_is_fifo() {
        let queue = MutexQueue::new(4);
        queue.try_push(1).unwrap();
        queue.try_push(2).unwrap();
        assert_eq!(queue.try_pop(), Some(1));
        assert_eq!(queue.try_pop(), Some(2));
        assert_eq!(queue.try_pop(), None);
    }

    #[test]
    fn mpmc_fifo_and_capacity() {
        let queue = MpmcQueue::new(2);
        assert!(queue.try_push(1).is_ok());
        assert!(queue.try_push(2).is_ok());
        assert!(queue.try_push(3).is_err());
        assert_eq!(queue.try_pop(), Some(1));
        assert!(queue.try_push(3).is_ok());
        assert_eq!(queue.try_pop(), Some(2));
        assert_eq!(queue.try_pop(), Some(3));
        assert_eq!(queue.try_pop(), None);
    }

    #[test]
    fn mpmc_delivers_each_value_once() {
        const PRODUCERS: usize = 4;
        const EACH: usize = 5_000;
        let queue = Arc::new(MpmcQueue::new(128));
        let seen = Arc::new(
            (0..PRODUCERS * EACH)
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>(),
        );
        let taken = Arc::new(AtomicUsize::new(0));
        let total = PRODUCERS * EACH;

        thread::scope(|scope| {
            for producer in 0..PRODUCERS {
                let queue = Arc::clone(&queue);
                scope.spawn(move || {
                    for index in 0..EACH {
                        let value = producer * EACH + index;
                        while queue.try_push(value).is_err() {
                            std::hint::spin_loop();
                        }
                    }
                });
            }
            for _ in 0..PRODUCERS {
                let queue = Arc::clone(&queue);
                let seen = Arc::clone(&seen);
                let taken = Arc::clone(&taken);
                scope.spawn(move || loop {
                    if let Some(value) = queue.try_pop() {
                        assert!(
                            !seen[value].swap(true, Ordering::Relaxed),
                            "duplicate {value}"
                        );
                        if taken.fetch_add(1, Ordering::Release) + 1 == total {
                            break;
                        }
                    } else if taken.load(Ordering::Acquire) == total {
                        break;
                    } else {
                        std::hint::spin_loop();
                    }
                });
            }
        });

        assert!(seen.iter().all(|flag| flag.load(Ordering::Relaxed)));
    }
}
