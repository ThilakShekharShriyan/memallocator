//! Throughput, contention, and tail latency.
//!
//! Rates are alloc+free pairs per second, except the bump allocators, which
//! cannot free. Latency samples are one alloc+free pair under contention,
//! with the cost of an empty `mach_absolute_time` pair subtracted.

use crate::alloc::{AtomicBump, Bump, LockFreeAlloc, MutexAlloc};
use crate::clock::{self, ticks, Clock};
use crate::gpu;
use crate::map::{LockFreeMap, MutexMap, ShardedMap};
use crate::pad::CachePadded;
use crate::queue::{MpmcQueue, MutexQueue};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Sample {
    ops: u64,
    secs: f64,
    threads: usize,
}

impl Sample {
    /// Total throughput across every thread.
    fn mops(&self) -> f64 {
        self.ops as f64 / self.secs / 1e6
    }

    /// Average time for one op on one thread.
    fn ns(&self) -> f64 {
        self.secs * self.threads as f64 / self.ops as f64 * 1e9
    }
}

pub fn run(sample: Duration, thorough: bool) {
    banner(&Clock::new());
    run_cpu(sample);
    if !run_gpu(thorough) {
        std::process::exit(1);
    }
    footer();
}

pub fn run_cpu(sample: Duration) {
    let clock = Clock::new();
    let threads = contention_threads();
    ordering_cost(sample, threads);
    false_sharing(sample, threads);
    allocators(sample, threads);
    bumps();
    latency(
        &clock,
        threads,
        if sample.as_millis() < 120 {
            1_500
        } else {
            6_000
        },
    );
    queues(sample);
    maps(sample, threads);
}

pub fn run_gpu(thorough: bool) -> bool {
    match gpu::run(thorough) {
        Ok(report) => {
            report.print();
            true
        }
        Err(err) => {
            eprintln!("\ngpu failed: {err}");
            false
        }
    }
}

fn banner(clock: &Clock) {
    let cpu =
        clock::read_sysctl("machdep.cpu.brand_string").unwrap_or_else(|| "unknown cpu".into());
    let line = clock::read_sysctl("hw.cachelinesize").unwrap_or_else(|| "128".into());
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("memallocator");
    println!("  {cpu}");
    println!("  {cores} hardware threads, cache line {line} bytes");
    println!(
        "  mach timer resolution {} ns, empty-read overhead {} ticks",
        clock.resolution_ns(),
        clock_overhead_ticks(clock)
    );
}

fn clock_overhead_ticks(clock: &Clock) -> u64 {
    // Recompute a display value. `Clock` keeps the subtracted overhead private.
    let _ = clock;
    let mut overhead = u64::MAX;
    for _ in 0..1000 {
        let start = ticks();
        std::hint::black_box(start);
        overhead = overhead.min(ticks().saturating_sub(start));
    }
    overhead
}

fn footer() {
    println!(
        "\nWhat the numbers are
  M/s is total throughput across threads. ns/op is the average on each thread.
  tcache is a thread-local freelist. Hits do not touch the shared list.
  A spin mutex can lose to a blocking mutex when every core hammers one line:
  the spinners delay the thread that holds it. A lock-free freelist has the
  same problem until the thread cache absorbs the hits.
  p50/p99/p99.9 are one alloc+free under contention. The mach timer is 41 ns,
  so a fast path below that shows up as 0 or 41.
  Ordering on one contended atomic is mostly the cache-line transfer, which
  is why relaxed and seqcst are close. False sharing is the line measurement.
  GPU bump is one device atomic add. GPU freelist is a device CAS, pop only.
  Delete, group commit, and cache-size runs: cargo run --release -- gaps"
    );
}

fn contention_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .clamp(2, 8)
}

fn section(title: impl std::fmt::Display) {
    println!("\n{title}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn ordering_cost(sample: Duration, threads: usize) {
    section(format!(
        "memory ordering on one atomic ({threads} threads, fetch_add)"
    ));
    println!("  {:<12} {:>10} {:>10}", "ordering", "M/s", "ns/op");
    for (name, order) in [
        ("relaxed", Ordering::Relaxed),
        ("acqrel", Ordering::AcqRel),
        ("seqcst", Ordering::SeqCst),
    ] {
        let counter = AtomicU64::new(0);
        let result = timed(threads, sample, |_| {
            counter.fetch_add(1, order);
            1
        });
        println!("  {name:<12} {:>10.2} {:>10.1}", result.mops(), result.ns());
    }
}

fn false_sharing(sample: Duration, threads: usize) {
    section(format!(
        "false sharing ({threads} threads, private counters)"
    ));
    let packed_counters: Vec<AtomicU64> = (0..threads).map(|_| AtomicU64::new(0)).collect();
    let packed = timed(threads, sample, |tid| {
        packed_counters[tid].fetch_add(1, Ordering::Relaxed);
        1
    });
    let padded_counters: Vec<CachePadded<AtomicU64>> = (0..threads)
        .map(|_| CachePadded::new(AtomicU64::new(0)))
        .collect();
    let padded = timed(threads, sample, |tid| {
        padded_counters[tid].value.fetch_add(1, Ordering::Relaxed);
        1
    });
    println!(
        "  {:<12} {:>8.2} M/s {:>8.1} ns/op",
        "packed",
        packed.mops(),
        packed.ns()
    );
    println!(
        "  {:<12} {:>8.2} M/s {:>8.1} ns/op",
        "padded 128B",
        padded.mops(),
        padded.ns()
    );
}

fn allocators(sample: Duration, threads: usize) {
    section(format!("allocation throughput, 64-byte alloc+free"));
    println!(
        "  {:<26} {:>8} {:>8}    {:>8} {:>8}",
        "design",
        "1 thr M/s",
        "ns/op",
        format!("{threads} thr M/s"),
        "ns/op"
    );
    let layout = std::alloc::Layout::from_size_align(64, 16).unwrap();
    let malloc = |n: usize| {
        timed(n, sample, move |_| {
            for _ in 0..32 {
                unsafe {
                    // SAFETY: layout is 64 bytes, alignment 16, non-zero.
                    let ptr = std::alloc::alloc(layout);
                    assert!(!ptr.is_null());
                    ptr.write_volatile(0xA5);
                    std::alloc::dealloc(ptr, layout);
                }
            }
            32
        })
    };
    print_pair("system malloc", malloc(1), malloc(threads));

    let blocking = MutexAlloc::blocking(false);
    print_pair(
        "mutex blocking",
        bench_slab(1, sample, &blocking),
        bench_slab(threads, sample, &blocking),
    );
    let spin = MutexAlloc::spin(false);
    print_pair(
        "mutex spin",
        bench_slab(1, sample, &spin),
        bench_slab(threads, sample, &spin),
    );
    let spin_cache = MutexAlloc::spin(true);
    print_pair(
        "mutex spin + tcache",
        bench_slab(1, sample, &spin_cache),
        bench_slab(threads, sample, &spin_cache),
    );
    let lockfree = LockFreeAlloc::new(false);
    print_pair(
        "lock-free",
        bench_slab(1, sample, &lockfree),
        bench_slab(threads, sample, &lockfree),
    );
    let lockfree_cache = LockFreeAlloc::new(true);
    print_pair(
        "lock-free + tcache",
        bench_slab(1, sample, &lockfree_cache),
        bench_slab(threads, sample, &lockfree_cache),
    );

    section(format!("mixed sizes 16/64/256/1024, {threads} threads"));
    println!("  {:<26} {:>8} {:>8}", "design", "M/s", "ns/op");
    let mixed_malloc = timed(threads, sample, move |_| {
        for &size in &[16usize, 64, 256, 1024] {
            let layout = std::alloc::Layout::from_size_align(size, 16).unwrap();
            unsafe {
                let ptr = std::alloc::alloc(layout);
                assert!(!ptr.is_null());
                ptr.write_volatile(0xA5);
                std::alloc::dealloc(ptr, layout);
            }
        }
        4
    });
    println!(
        "  {:<26} {:>8.2} {:>8.1}",
        "system malloc",
        mixed_malloc.mops(),
        mixed_malloc.ns()
    );
    print_one(
        "mutex spin + tcache",
        bench_mixed(threads, sample, &spin_cache),
    );
    print_one(
        "lock-free + tcache",
        bench_mixed(threads, sample, &lockfree_cache),
    );
}

fn bench_slab<A>(threads: usize, sample: Duration, alloc: &A) -> Sample
where
    A: Alloc + Sync,
{
    timed(threads, sample, move |_| {
        for _ in 0..32 {
            let ptr = alloc.allocate(64).expect("slab exhausted");
            touch(ptr.as_ptr());
            alloc.deallocate(ptr, 64);
        }
        32
    })
}

fn bench_mixed<A>(threads: usize, sample: Duration, alloc: &A) -> Sample
where
    A: Alloc + Sync,
{
    timed(threads, sample, move |_| {
        for &size in &[16usize, 64, 256, 1024] {
            let ptr = alloc.allocate(size).expect("slab exhausted");
            touch(ptr.as_ptr());
            alloc.deallocate(ptr, size);
        }
        4
    })
}

trait Alloc {
    fn allocate(&self, size: usize) -> Option<std::ptr::NonNull<u8>>;
    fn deallocate(&self, ptr: std::ptr::NonNull<u8>, size: usize);
}

impl Alloc for MutexAlloc {
    fn allocate(&self, size: usize) -> Option<std::ptr::NonNull<u8>> {
        MutexAlloc::allocate(self, size)
    }
    fn deallocate(&self, ptr: std::ptr::NonNull<u8>, size: usize) {
        MutexAlloc::deallocate(self, ptr, size);
    }
}

impl Alloc for LockFreeAlloc {
    fn allocate(&self, size: usize) -> Option<std::ptr::NonNull<u8>> {
        LockFreeAlloc::allocate(self, size)
    }
    fn deallocate(&self, ptr: std::ptr::NonNull<u8>, size: usize) {
        LockFreeAlloc::deallocate(self, ptr, size);
    }
}

fn bumps() {
    section("bump allocation, no free (ceiling and shared cursor)");
    let single = median_rate(7, |n| {
        let mut bump = Bump::new(n * 64);
        // Fault the pages before the timed pass.
        while bump.alloc(64).is_some() {}
        bump.reset();
        let started = Instant::now();
        for _ in 0..n {
            let ptr = bump.alloc(64).expect("bump");
            touch(ptr.as_ptr());
        }
        n as f64 / started.elapsed().as_secs_f64()
    });
    println!(
        "  {:<26} {:>8.2} M/s {:>8.1} ns/op",
        "bump 1 thread",
        single / 1e6,
        1e9 / single
    );

    let threads = contention_threads();
    let per_thread = 100_000usize;
    let bump = Arc::new(AtomicBump::new(threads * per_thread * 64));
    while bump.alloc(64).is_some() {}
    bump.reset();
    let rates = atomic_bump_rates(&bump, threads, per_thread, 7);
    let median = median(&rates);
    println!(
        "  {:<26} {:>8.2} M/s {:>8.1} ns/op",
        format!("atomic bump {threads} threads"),
        median / 1e6,
        1e9 * threads as f64 / median
    );
}

fn atomic_bump_rates(
    bump: &Arc<AtomicBump>,
    threads: usize,
    per_thread: usize,
    passes: usize,
) -> Vec<f64> {
    let go = CachePadded::new(AtomicU64::new(0));
    let done = AtomicU64::new(0);
    let rates = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let bump = Arc::clone(bump);
            let go = &go;
            let done = &done;
            scope.spawn(move || {
                for pass in 1..=passes as u64 {
                    while go.value.load(Ordering::Acquire) < pass {
                        std::hint::spin_loop();
                    }
                    for _ in 0..per_thread {
                        let ptr = bump.alloc(64).expect("atomic bump capacity");
                        touch(ptr.as_ptr());
                    }
                    done.fetch_add(1, Ordering::Release);
                }
            });
        }
        let rates = &rates;
        for pass in 1..=passes as u64 {
            bump.reset();
            let started = Instant::now();
            go.value.store(pass, Ordering::Release);
            while done.load(Ordering::Acquire) < pass * threads as u64 {
                std::hint::spin_loop();
            }
            let ops = (threads * per_thread) as f64;
            rates
                .lock()
                .unwrap()
                .push(ops / started.elapsed().as_secs_f64());
        }
    });
    rates.into_inner().unwrap()
}

fn latency(clock: &Clock, threads: usize, iters: usize) {
    section(format!(
        "tail latency, 64-byte alloc+free, {threads} threads"
    ));
    println!(
        "  {:<26} {:>8} {:>8} {:>8}",
        "design", "p50 ns", "p99 ns", "p99.9 ns"
    );
    report_latency(
        "mutex spin",
        clock,
        threads,
        iters,
        &MutexAlloc::spin(false),
    );
    report_latency(
        "mutex spin + tcache",
        clock,
        threads,
        iters,
        &MutexAlloc::spin(true),
    );
    report_latency(
        "lock-free",
        clock,
        threads,
        iters,
        &LockFreeAlloc::new(false),
    );
    report_latency(
        "lock-free + tcache",
        clock,
        threads,
        iters,
        &LockFreeAlloc::new(true),
    );
    println!(
        "  timer resolution {} ns; samples at or below it are the fast path",
        clock.resolution_ns()
    );
}

fn report_latency<A: Alloc + Sync>(
    name: &str,
    clock: &Clock,
    threads: usize,
    iters: usize,
    alloc: &A,
) {
    let samples = sample_latency(threads, iters, clock, || {
        let ptr = alloc.allocate(64).expect("slab exhausted");
        touch(ptr.as_ptr());
        alloc.deallocate(ptr, 64);
    });
    println!(
        "  {:<26} {:>8} {:>8} {:>8}",
        name,
        percentile(&samples, 0.50),
        percentile(&samples, 0.99),
        percentile(&samples, 0.999)
    );
}

fn queues(sample: Duration) {
    section("bounded queues, successful pushes and pops");
    println!("  {:<26} {:>10} {:>10}", "design", "M/s", "ns/op");
    for (producers, consumers) in [(1, 1), (4, 4)] {
        let mutex = Arc::new(MutexQueue::new(1024));
        let produced = Arc::clone(&mutex);
        let consumed = Arc::clone(&mutex);
        let result = timed_roles(
            sample,
            producers,
            consumers,
            move || produced.try_push(1u64).is_ok(),
            move || consumed.try_pop().is_some(),
        );
        println!(
            "  {:<26} {:>10.2} {:>10.1}",
            format!("mutex {producers}p/{consumers}c"),
            result.mops(),
            result.ns()
        );

        let mpmc = Arc::new(MpmcQueue::new(1024));
        let produced = Arc::clone(&mpmc);
        let consumed = Arc::clone(&mpmc);
        let result = timed_roles(
            sample,
            producers,
            consumers,
            move || produced.try_push(1u64).is_ok(),
            move || consumed.try_pop().is_some(),
        );
        println!(
            "  {:<26} {:>10.2} {:>10.1}",
            format!("lock-free {producers}p/{consumers}c"),
            result.mops(),
            result.ns()
        );
    }
}

fn maps(sample: Duration, threads: usize) {
    section(format!(
        "maps, 90% get / 10% update, {threads} threads, 32768 keys"
    ));
    println!("  {:<26} {:>10} {:>10}", "design", "M/s", "ns/op");
    const KEYS: u64 = 32_768;
    let mutex = MutexMap::new();
    let sharded = ShardedMap::new(16);
    let lockfree = LockFreeMap::new(KEYS as usize * 2);
    for key in 1..=KEYS {
        mutex.insert(key, key);
        sharded.insert(key, key);
        lockfree.insert(key, key).unwrap();
    }
    let mutex_rate = timed(threads, sample, move |tid| {
        map_work(tid, |key, value| {
            if let Some(value) = value {
                mutex.insert(key, value);
            } else {
                std::hint::black_box(mutex.get(key));
            }
        })
    });
    print_one("one mutex", mutex_rate);
    let sharded_rate = timed(threads, sample, move |tid| {
        map_work(tid, |key, value| {
            if let Some(value) = value {
                sharded.insert(key, value);
            } else {
                std::hint::black_box(sharded.get(key));
            }
        })
    });
    print_one("16 shards", sharded_rate);
    let lockfree_rate = timed(threads, sample, move |tid| {
        map_work(tid, |key, value| {
            if let Some(value) = value {
                lockfree.insert(key, value).expect("map full");
            } else {
                std::hint::black_box(lockfree.get(key));
            }
        })
    });
    print_one("lock-free open address", lockfree_rate);

    section(format!("one hot key, {threads} threads"));
    println!("  {:<26} {:>8} {:>8}", "design", "M/s", "ns/op");
    let hot_mutex = MutexMap::new();
    let hot_shard = ShardedMap::new(16);
    let hot_free = LockFreeMap::new(1024);
    hot_mutex.insert(1, 1);
    hot_shard.insert(1, 1);
    hot_free.insert(1, 1).unwrap();
    print_one(
        "one mutex",
        timed(threads, sample, |_| {
            hot_mutex.insert(1, 2);
            std::hint::black_box(hot_mutex.get(1));
            2
        }),
    );
    print_one(
        "16 shards (same shard)",
        timed(threads, sample, |_| {
            hot_shard.insert(1, 2);
            std::hint::black_box(hot_shard.get(1));
            2
        }),
    );
    print_one(
        "lock-free open address",
        timed(threads, sample, |_| {
            hot_free.insert(1, 2).unwrap();
            std::hint::black_box(hot_free.get(1));
            2
        }),
    );
}

fn map_work(tid: usize, mut op: impl FnMut(u64, Option<u64>)) -> u64 {
    let mut state = 0x1234_5678_u64.wrapping_add(tid as u64 * 0x9E37);
    for _ in 0..32 {
        state = xorshift(state);
        let key = (state % 32_768) + 1;
        if state & 15 == 0 {
            op(key, Some(state));
        } else {
            op(key, None);
        }
    }
    32
}

fn print_pair(name: &str, one: Sample, many: Sample) {
    println!(
        "  {:<26} {:>8.2} {:>8.1}    {:>8.2} {:>8.1}",
        name,
        one.mops(),
        one.ns(),
        many.mops(),
        many.ns()
    );
}

fn print_one(name: &str, sample: Sample) {
    println!("  {:<26} {:>8.2} {:>8.1}", name, sample.mops(), sample.ns());
}

fn timed(threads: usize, sample: Duration, work: impl Fn(usize) -> u64 + Sync) -> Sample {
    let go = CachePadded::new(AtomicBool::new(false));
    let stop = CachePadded::new(AtomicBool::new(false));
    let started = AtomicU64::new(0);
    let ops = AtomicU64::new(0);
    let mut started_at = None;

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let go = &go;
            let stop = &stop;
            let started = &started;
            let ops = &ops;
            let work = &work;
            scope.spawn(move || {
                for _ in 0..32 {
                    work(tid);
                }
                started.fetch_add(1, Ordering::Release);
                while !go.value.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                let mut local = 0u64;
                let mut checks = 0u64;
                loop {
                    local += work(tid);
                    checks += 1;
                    if checks == 64 {
                        if stop.value.load(Ordering::Relaxed) {
                            break;
                        }
                        checks = 0;
                    }
                }
                ops.fetch_add(local, Ordering::Relaxed);
            });
        }
        while started.load(Ordering::Acquire) < threads as u64 {
            std::hint::spin_loop();
        }
        started_at = Some(Instant::now());
        go.value.store(true, Ordering::Release);
        std::thread::sleep(sample);
        stop.value.store(true, Ordering::Release);
    });

    Sample {
        ops: ops.load(Ordering::Relaxed),
        secs: started_at.unwrap().elapsed().as_secs_f64(),
        threads,
    }
}

fn timed_roles(
    sample: Duration,
    producers: usize,
    consumers: usize,
    push: impl Fn() -> bool + Sync,
    pop: impl Fn() -> bool + Sync,
) -> Sample {
    let go = CachePadded::new(AtomicBool::new(false));
    let stop = CachePadded::new(AtomicBool::new(false));
    let started = AtomicU64::new(0);
    let ops = AtomicU64::new(0);
    let mut started_at = None;
    let threads = producers + consumers;

    std::thread::scope(|scope| {
        for role in 0..threads {
            let go = &go;
            let stop = &stop;
            let started = &started;
            let ops = &ops;
            let push = &push;
            let pop = &pop;
            scope.spawn(move || {
                let produce = role < producers;
                let step = || if produce { push() } else { pop() };
                for _ in 0..128 {
                    step();
                }
                started.fetch_add(1, Ordering::Release);
                while !go.value.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                let mut local = 0u64;
                let mut checks = 0u64;
                loop {
                    if step() {
                        local += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                    checks += 1;
                    if checks == 256 {
                        if stop.value.load(Ordering::Relaxed) {
                            break;
                        }
                        checks = 0;
                    }
                }
                ops.fetch_add(local, Ordering::Relaxed);
            });
        }
        while started.load(Ordering::Acquire) < threads as u64 {
            std::hint::spin_loop();
        }
        started_at = Some(Instant::now());
        go.value.store(true, Ordering::Release);
        std::thread::sleep(sample);
        stop.value.store(true, Ordering::Release);
    });

    Sample {
        ops: ops.load(Ordering::Relaxed),
        secs: started_at.unwrap().elapsed().as_secs_f64(),
        threads,
    }
}

fn sample_latency(threads: usize, iters: usize, clock: &Clock, op: impl Fn() + Sync) -> Vec<u64> {
    let go = AtomicBool::new(false);
    let collected = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let go = &go;
            let collected = &collected;
            let op = &op;
            scope.spawn(move || {
                for _ in 0..1000 {
                    op();
                }
                while !go.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                let mut local = Vec::with_capacity(iters);
                for _ in 0..iters {
                    let start = ticks();
                    op();
                    let end = ticks();
                    local.push(clock.elapsed_ns(start, end));
                }
                collected.lock().unwrap().extend(local);
            });
        }
        go.store(true, Ordering::Release);
    });
    let mut samples = collected.into_inner().unwrap();
    samples.sort_unstable();
    samples
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn median_rate(passes: usize, mut body: impl FnMut(usize) -> f64) -> f64 {
    let n = 200_000usize;
    let mut rates = Vec::with_capacity(passes);
    for pass in 0..passes {
        let rate = body(n);
        if pass > 0 {
            rates.push(rate);
        }
    }
    median(&rates)
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

#[inline(never)]
fn touch(ptr: *mut u8) {
    unsafe {
        // SAFETY: caller owns this byte for the duration of the write.
        ptr.write_volatile(0xA5);
    }
}

fn xorshift(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}
