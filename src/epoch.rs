//! Epoch-based reclamation.
//!
//! A pinned thread may hold raw pointers into a lock-free structure. Those
//! pointers must not be freed until every such pin has ended. This collector
//! is the small version of what crossbeam-epoch does.
//!
//! Each thread announces the global epoch it pinned in. The global epoch
//! advances only when every pinned thread has announced the current epoch.
//! An object retired in epoch `E` is freed once the global epoch reaches
//! `E + 2`. A pin that started at `E` blocks the second advance, so the
//! object cannot disappear under that pin.
//!
//! Three bags are enough: the bag for epoch `E` is not reused until `E + 3`,
//! and by then `E` is already two epochs in the past and has been emptied.
//!
//! `ADVANCE_EVERY` is the knob. Checking every thread on every unpin makes
//! the collector itself the bottleneck. Production collectors check in batches.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{fence, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Unpins between attempts to move the global epoch forward.
pub const ADVANCE_EVERY: usize = 64;

struct Retired {
    epoch: usize,
    ptr: *mut u8,
    drop_fn: unsafe fn(*mut u8),
}

// The collector frees an object on whichever thread notices the epoch has
// moved. `T: Send` is required at `defer` so that drop is allowed here.
unsafe impl Send for Retired {}

struct Participant {
    /// `0` means the thread is not pinned. Otherwise the low bit is set and
    /// the upper bits are the epoch observed at pin time.
    epoch: AtomicUsize,
}

struct CollectorInner {
    epoch: AtomicUsize,
    participants: Mutex<Vec<Arc<Participant>>>,
    orphans: Mutex<Vec<Retired>>,
    reclaimed: AtomicUsize,
}

/// Shared epoch counter and the list of threads participating in it.
pub struct Collector {
    inner: CollectorInner,
}

/// Per-thread retirement lists. Move this to the thread that allocates.
pub struct LocalHandle {
    collector: Arc<Collector>,
    participant: Arc<Participant>,
    bags: RefCell<[Vec<Retired>; 3]>,
    pins: Cell<usize>,
    ops: Cell<usize>,
}

/// Proof that this thread is inside a pin. Pointers loaded while this guard
/// is alive stay allocated until the guard drops and the epoch moves on.
pub struct Guard<'a> {
    handle: &'a LocalHandle,
}

impl Collector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: CollectorInner {
                epoch: AtomicUsize::new(0),
                participants: Mutex::new(Vec::new()),
                orphans: Mutex::new(Vec::new()),
                reclaimed: AtomicUsize::new(0),
            },
        })
    }

    pub fn register(self: &Arc<Self>) -> LocalHandle {
        let participant = Arc::new(Participant {
            epoch: AtomicUsize::new(0),
        });
        self.inner
            .participants
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(Arc::clone(&participant));
        LocalHandle {
            collector: Arc::clone(self),
            participant,
            bags: RefCell::new([Vec::new(), Vec::new(), Vec::new()]),
            pins: Cell::new(0),
            ops: Cell::new(0),
        }
    }

    pub fn global_epoch(&self) -> usize {
        self.inner.epoch.load(Ordering::Acquire)
    }

    pub fn reclaimed(&self) -> usize {
        self.inner.reclaimed.load(Ordering::Relaxed)
    }
}

impl Default for Collector {
    fn default() -> Self {
        Self {
            inner: CollectorInner {
                epoch: AtomicUsize::new(0),
                participants: Mutex::new(Vec::new()),
                orphans: Mutex::new(Vec::new()),
                reclaimed: AtomicUsize::new(0),
            },
        }
    }
}

impl LocalHandle {
    pub fn pin(&self) -> Guard<'_> {
        if self.pins.get() == 0 {
            // Announce, then a SeqCst fence, then re-read the global epoch.
            // `try_advance` has the matching fence. A pin that loaded a
            // pointer is ordered against the epoch bump that would free it.
            let mut global = self.collector.inner.epoch.load(Ordering::Relaxed);
            loop {
                self.participant
                    .epoch
                    .store(global.wrapping_shl(1) | 1, Ordering::Relaxed);
                fence(Ordering::SeqCst);
                let current = self.collector.inner.epoch.load(Ordering::Relaxed);
                if current == global {
                    break;
                }
                global = current;
            }
        }
        self.pins.set(self.pins.get() + 1);
        Guard { handle: self }
    }

    /// Advance and reclaim now. Tests use this to show a held pin blocks freeing.
    pub fn flush(&self) {
        self.try_advance();
        self.collect();
    }

    fn exit_pin(&self) {
        self.participant.epoch.store(0, Ordering::Release);
        let ops = self.ops.get() + 1;
        self.ops.set(ops);
        if ops % ADVANCE_EVERY == 0 {
            self.flush();
        }
    }

    fn retire(&self, value: Retired) {
        debug_assert!(self.pins.get() > 0, "retire requires a pin");
        // Empty a bag before that epoch's slot is reused. Slot `E % 3` is
        // reused at `E + 3`, and those objects became free at `E + 2`.
        self.reclaim_bags();
        let epoch = self.collector.inner.epoch.load(Ordering::Acquire);
        self.bags.borrow_mut()[epoch % 3].push(Retired { epoch, ..value });
    }

    fn try_advance(&self) {
        let global = self.collector.inner.epoch.load(Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let participants = self
            .collector
            .inner
            .participants
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        for participant in participants.iter() {
            let announced = participant.epoch.load(Ordering::Acquire);
            let pinned = announced & 1 == 1;
            // A pin at `global` has already observed this epoch, so it does
            // not block the step to `global + 1`. It blocks the one after.
            if pinned && (announced >> 1) != global {
                return;
            }
        }
        let _ = self.collector.inner.epoch.compare_exchange(
            global,
            global.wrapping_add(1),
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    fn collect(&self) {
        self.reclaim_bags();
        self.collect_orphans(self.collector.global_epoch());
    }

    fn reclaim_bags(&self) {
        let global = self.collector.global_epoch();
        let mut ready = Vec::new();
        {
            let mut bags = self.bags.borrow_mut();
            for bag in bags.iter_mut() {
                let mut kept = Vec::new();
                for item in bag.drain(..) {
                    if global.wrapping_sub(item.epoch) >= 2 {
                        ready.push(item);
                    } else {
                        kept.push(item);
                    }
                }
                *bag = kept;
            }
        }
        for item in ready {
            self.destroy_one(item);
        }
    }

    fn collect_orphans(&self, global: usize) {
        let mut orphans = self
            .collector
            .inner
            .orphans
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut kept = Vec::new();
        let ready = std::mem::take(&mut *orphans);
        drop(orphans);
        for item in ready {
            if global.wrapping_sub(item.epoch) >= 2 {
                self.destroy_one(item);
            } else {
                kept.push(item);
            }
        }
        if !kept.is_empty() {
            self.collector
                .inner
                .orphans
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend(kept);
        }
    }

    fn destroy_one(&self, item: Retired) {
        unsafe {
            // SAFETY: this object was retired at `item.epoch`, and the global
            // epoch is at least two past that. Every pin that could have
            // loaded the pointer has ended. `drop_fn` reconstructs the Box.
            (item.drop_fn)(item.ptr);
        }
        self.collector
            .inner
            .reclaimed
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl Guard<'_> {
    /// Free `value` after every current pin has ended.
    pub fn defer<T: Send>(&self, value: T) {
        self.defer_destroy(Box::into_raw(Box::new(value)));
    }

    /// Free an existing allocation. The pointer itself stays valid until the
    /// epoch moves on; this does not move the value to a new box.
    pub fn defer_destroy<T: Send>(&self, ptr: *mut T) {
        self.handle.retire(Retired {
            epoch: 0,
            ptr: ptr.cast(),
            drop_fn: drop_box::<T>,
        });
    }
}

unsafe fn drop_box<T>(ptr: *mut u8) {
    // SAFETY: `ptr` came from `Box::<T>::into_raw` and is dropped once.
    drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let pins = self.handle.pins.get();
        self.handle.pins.set(pins - 1);
        if pins == 1 {
            self.handle.exit_pin();
        }
    }
}

impl Drop for LocalHandle {
    fn drop(&mut self) {
        if self.pins.get() > 0 {
            self.participant.epoch.store(0, Ordering::Release);
            self.pins.set(0);
        }
        let mut leftover = Vec::new();
        for bag in self.bags.borrow_mut().iter_mut() {
            leftover.append(bag);
        }
        if !leftover.is_empty() {
            self.collector
                .inner
                .orphans
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend(leftover);
        }
        self.collector
            .inner
            .participants
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .retain(|participant| !Arc::ptr_eq(participant, &self.participant));
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    struct Flag(Arc<AtomicUsize>);

    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_held_pin_keeps_retired_memory_alive() {
        let collector = Collector::new();
        let holder = collector.register();
        let other = collector.register();
        let dropped = Arc::new(AtomicUsize::new(0));

        let pin = holder.pin();
        {
            let guard = other.pin();
            guard.defer(Flag(Arc::clone(&dropped)));
        }
        for _ in 0..8 {
            other.flush();
            assert_eq!(dropped.load(Ordering::Relaxed), 0);
            assert!(collector.global_epoch() < 2);
        }
        drop(pin);
        holder.flush();
        other.flush();
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            1,
            "epoch {}, reclaimed {}",
            collector.global_epoch(),
            collector.reclaimed()
        );
    }

    #[test]
    fn unpin_then_two_advances_frees() {
        let collector = Collector::new();
        let handle = collector.register();
        let dropped = Arc::new(AtomicUsize::new(0));
        {
            let guard = handle.pin();
            guard.defer(Flag(Arc::clone(&dropped)));
            handle.flush();
            assert_eq!(dropped.load(Ordering::Relaxed), 0);
        }
        handle.flush();
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn threads_can_retire_while_another_reads() {
        let collector = Collector::new();
        let pointer = Arc::new(AtomicUsize::new(0));
        let initial = Box::into_raw(Box::new(7u64));
        pointer.store(initial as usize, Ordering::Release);

        thread::scope(|scope| {
            for _ in 0..4 {
                let collector = Arc::clone(&collector);
                let pointer = Arc::clone(&pointer);
                scope.spawn(move || {
                    let handle = collector.register();
                    for _ in 0..200 {
                        let guard = handle.pin();
                        let raw = pointer.load(Ordering::Acquire) as *const u64;
                        if !raw.is_null() {
                            let value = unsafe { *raw };
                            assert!(value == 7 || value == 9);
                        }
                        drop(guard);
                    }
                });
            }
            let collector = Arc::clone(&collector);
            let pointer = Arc::clone(&pointer);
            scope.spawn(move || {
                let handle = collector.register();
                for _ in 0..100 {
                    let next = Box::into_raw(Box::new(9u64));
                    let guard = handle.pin();
                    let old = pointer.swap(next as usize, Ordering::AcqRel) as *mut u64;
                    if !old.is_null() {
                        guard.defer_destroy(old);
                    }
                    drop(guard);
                }
                handle.flush();
            });
        });
        let last = pointer.load(Ordering::Acquire) as *mut u64;
        if !last.is_null() {
            drop(unsafe { Box::from_raw(last) });
        }
    }
}
