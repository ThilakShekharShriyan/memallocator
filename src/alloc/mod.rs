//! Size-class slab allocators.
//!
//! Each size class is a fixed array of slots plus a freelist. A thread cache
//! (magazine) satisfies alloc and free without touching that shared list.
//! The shared list is either a spin/blocking mutex or a tagged Treiber stack.

mod bump;
mod lockfree;
mod mutex_alloc;
mod tcache;

pub use bump::{AtomicBump, Bump};
pub use lockfree::LockFreeAlloc;
pub use mutex_alloc::MutexAlloc;

/// Power-of-two classes from 16 to 2048. Anything larger is out of scope:
/// production allocators mmap those directly.
pub const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

pub const EMPTY: u32 = u32::MAX;

pub fn class_for(size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    CLASSES.iter().position(|&class| class >= size)
}

/// About 8 MiB of slots per class, so a missed thread cache still has room
/// to refill without the benchmark emptying the pool.
pub fn blocks_for(block_size: usize) -> u32 {
    let count = (8 * 1024 * 1024) / block_size;
    count.max(1024) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_up_into_a_class() {
        assert_eq!(class_for(0), None);
        assert_eq!(class_for(1), Some(0));
        assert_eq!(class_for(16), Some(0));
        assert_eq!(class_for(17), Some(1));
        assert_eq!(class_for(2048), Some(7));
        assert_eq!(class_for(2049), None);
    }
}
