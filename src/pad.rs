#![deny(unsafe_code)]

/// One Apple Silicon cache line.
///
/// `hw.cachelinesize` on this machine is 128. Padding an atomic to that
/// alignment keeps it off the same line as its neighbors, which is what
/// stops false sharing.
pub const CACHE_LINE: usize = 128;

#[repr(align(128))]
pub struct CachePadded<T> {
    pub value: T,
}

impl<T> CachePadded<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_stride_is_a_cache_line() {
        let values = vec![CachePadded::new(1u64), CachePadded::new(2u64)];
        let a = &values[0] as *const CachePadded<u64> as usize;
        let b = &values[1] as *const CachePadded<u64> as usize;
        assert!(b - a >= CACHE_LINE);
    }
}
