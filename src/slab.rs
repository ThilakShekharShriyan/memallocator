//! Owned, 16-byte-aligned storage for size-class slots.
//!
//! The freelist never moves this allocation. Slot ownership is the freelist's
//! job; this type only hands out addresses inside the block it allocated.

use std::alloc::Layout;
use std::ptr::NonNull;

pub struct Slab {
    base: NonNull<u8>,
    layout: Layout,
    pub block_size: usize,
    pub count: u32,
}

impl Slab {
    pub fn new(block_size: usize, count: u32) -> Self {
        assert!(block_size >= 16 && block_size.is_multiple_of(16));
        assert!(count > 0);
        let bytes = block_size
            .checked_mul(count as usize)
            .expect("slab byte count overflows");
        let layout = Layout::from_size_align(bytes, 16).expect("slab layout");
        let base = unsafe {
            // SAFETY: `layout` has a non-zero size and alignment of 16.
            // alloc_zeroed returns null only on OOM, which we reject below.
            std::alloc::alloc_zeroed(layout)
        };
        let base = NonNull::new(base).expect("out of memory allocating a slab");
        Self {
            base,
            layout,
            block_size,
            count,
        }
    }

    pub fn ptr(&self, index: u32) -> NonNull<u8> {
        debug_assert!(index < self.count);
        let offset = self.block_size * index as usize;
        unsafe {
            // SAFETY: `index` is a freelist index, so `offset` points at the
            // first byte of a slot inside this allocation.
            NonNull::new_unchecked(self.base.as_ptr().add(offset))
        }
    }

    pub fn index_of(&self, ptr: NonNull<u8>) -> u32 {
        let addr = ptr.as_ptr() as usize;
        let base = self.base.as_ptr() as usize;
        let bytes = self.block_size * self.count as usize;
        debug_assert!(addr >= base && addr < base + bytes);
        debug_assert!((addr - base).is_multiple_of(self.block_size));
        ((addr - base) / self.block_size) as u32
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `base` came from alloc_zeroed with `layout`, and every
            // outstanding slot pointer must already have been returned. The
            // allocator drops the slab only after its Arc count hits zero.
            std::alloc::dealloc(self.base.as_ptr(), self.layout);
        }
    }
}

// SAFETY: the allocation is owned here and never reallocated. Threads share
// the address only to compute slot pointers; the freelist decides which
// thread may write a given slot. `Drop` runs when the owner is exclusive.
unsafe impl Send for Slab {}
unsafe impl Sync for Slab {}
