//! Per-thread magazines of free slot indices.
//!
//! A hit pops a `u32` from a `Vec` on this thread. No atomics, no locks.
//! Overflow and underflow move a batch to the shared pool. The magazine
//! holds an `Arc` so a flush during thread teardown cannot use a freed pool.

use std::cell::RefCell;
use std::sync::Arc;

pub const BATCH: usize = 32;
pub const LIMIT: usize = 64;

pub trait Pool: Send + Sync + 'static {
    fn class_count(&self) -> usize;
    fn pop_one(&self, class: usize) -> Option<u32>;
    fn push_one(&self, class: usize, index: u32);
    fn pop_batch(&self, class: usize, dst: &mut Vec<u32>, max: usize) -> usize {
        let mut got = 0;
        for _ in 0..max {
            match self.pop_one(class) {
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
        for &index in indices {
            self.push_one(class, index);
        }
    }
}

pub struct Magazine<P: Pool> {
    id: usize,
    pool: Option<Arc<P>>,
    bins: Vec<Vec<u32>>,
}

impl<P: Pool> Magazine<P> {
    pub fn new() -> Self {
        Self {
            id: 0,
            pool: None,
            bins: Vec::new(),
        }
    }

    pub fn ensure(&mut self, pool: &Arc<P>) {
        let id = Arc::as_ptr(pool) as usize;
        if self.id == id && self.pool.is_some() {
            return;
        }
        self.flush();
        self.id = id;
        self.bins.clear();
        self.bins.resize_with(pool.class_count(), Vec::new);
        self.pool = Some(Arc::clone(pool));
    }

    pub fn alloc_index(&mut self, class: usize) -> Option<u32> {
        if let Some(index) = self.bins[class].pop() {
            return Some(index);
        }
        let pool = self.pool.as_ref()?.clone();
        let got = pool.pop_batch(class, &mut self.bins[class], BATCH);
        if got == 0 {
            None
        } else {
            self.bins[class].pop()
        }
    }

    pub fn free_index(&mut self, class: usize, index: u32) {
        self.bins[class].push(index);
        if self.bins[class].len() >= LIMIT {
            let extra = self.bins[class].split_off(LIMIT / 2);
            let pool = self.pool.as_ref().expect("magazine pool").clone();
            pool.push_batch(class, &extra);
        }
    }

    pub fn flush(&mut self) {
        if let Some(pool) = self.pool.take() {
            for (class, bin) in self.bins.iter_mut().enumerate() {
                if !bin.is_empty() {
                    pool.push_batch(class, bin);
                    bin.clear();
                }
            }
            self.id = 0;
        }
    }
}

impl<P: Pool> Default for Magazine<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Pool> Drop for Magazine<P> {
    fn drop(&mut self) {
        self.flush();
    }
}

pub fn with_magazine<P, R>(
    slot: &RefCell<Magazine<P>>,
    pool: &Arc<P>,
    body: impl FnOnce(&mut Magazine<P>) -> R,
) -> R
where
    P: Pool,
{
    let mut magazine = slot.borrow_mut();
    magazine.ensure(pool);
    body(&mut magazine)
}
