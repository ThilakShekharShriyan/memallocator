//! Pointer-chase latency across working sets, plus a sequential scan.
//!
//! Each node is one cache line and the chase is a single cycle, so every
//! step depends on the previous load. A sequential scan shows what the
//! memory path can do when the next line is predictable.

use std::time::Instant;

#[derive(Clone, Copy)]
pub struct ChasePoint {
    pub bytes: usize,
    pub ns_per_load: f64,
    pub scan_gbs: f64,
}

#[repr(C, align(128))]
struct Line {
    next: u32,
}

pub fn measure(sizes: &[usize]) -> Vec<ChasePoint> {
    sizes.iter().copied().map(measure_one).collect()
}

fn measure_one(bytes: usize) -> ChasePoint {
    let lines = (bytes / std::mem::size_of::<Line>()).max(2);
    let nodes = cycle(lines);
    let steps = 2_000_000usize.max(lines.saturating_mul(8));
    let mut index = 0u32;
    let started = Instant::now();
    for _ in 0..steps {
        index = nodes[index as usize].next;
    }
    std::hint::black_box(index);
    let elapsed = started.elapsed().as_secs_f64().max(1e-9);
    let ns_per_load = elapsed * 1e9 / steps as f64;

    let passes = (64 * 1024 * 1024 / (lines * std::mem::size_of::<Line>())).max(1);
    let mut acc = 0u64;
    let started = Instant::now();
    for _ in 0..passes {
        for node in &nodes {
            acc = acc.wrapping_add(u64::from(node.next));
        }
    }
    std::hint::black_box(acc);
    let scan_elapsed = started.elapsed().as_secs_f64().max(1e-9);
    let scanned = lines * std::mem::size_of::<Line>() * passes;
    ChasePoint {
        bytes: lines * std::mem::size_of::<Line>(),
        ns_per_load,
        scan_gbs: scanned as f64 / scan_elapsed / 1e9,
    }
}

/// One cycle through every line. A raw shuffle of `next` is many short cycles.
fn cycle(lines: usize) -> Vec<Line> {
    let mut order: Vec<u32> = (0..lines as u32).collect();
    let mut state = 0x1234_5678_9abc_def0u64;
    for index in (1..order.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let swap_with = (state >> 33) as usize % (index + 1);
        order.swap(index, swap_with);
    }
    let mut nodes = Vec::with_capacity(lines);
    nodes.resize_with(lines, || Line { next: 0 });
    for window in order.windows(2) {
        nodes[window[0] as usize].next = window[1];
    }
    nodes[order[lines - 1] as usize].next = order[0];
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chase_is_one_cycle() {
        let nodes = cycle(128);
        let mut seen = vec![false; nodes.len()];
        let mut index = 0u32;
        for _ in 0..nodes.len() {
            assert!(!seen[index as usize]);
            seen[index as usize] = true;
            index = nodes[index as usize].next;
        }
        assert!(seen.iter().all(|bit| *bit));
        assert_eq!(index, 0);
    }
}
