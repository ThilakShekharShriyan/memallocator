//! The three follow-on measurements: epoch delete, group commit, working set.
//!
//! These stay off the original `all` run. Each printout names the knob that
//! moves the number.

use crate::clock;
use crate::epoch::ADVANCE_EVERY;
use crate::epoch_map::EpochMap;
use crate::store::{GroupCommit, Store, WAL_BATCH};
use crate::workingset;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run(sample: Duration) {
    println!("memallocator — delete, durability, working set");
    run_epoch(sample);
    run_wal(sample);
    run_cache();
    println!(
        "\nWhat to change
  ADVANCE_EVERY ({ADVANCE_EVERY}) in src/epoch.rs: how often a thread tries to
  move the epoch. Smaller reclaims sooner and takes the participant lock more.
  WAL_BATCH ({WAL_BATCH}) in src/store.rs: records per sync_data while the queue
  stays full. The page cache frame count is the other durability knob.
  Working-set sizes are in src/gaps.rs. Past L2, a flat line is the
  system-level cache, not proof that the chase reached DRAM."
    );
}

pub fn run_epoch(sample: Duration) {
    let threads = threads();
    let keys = 32_768u64;
    section(format!(
        "delete-capable map ({threads} threads, 90% get / 10% remove+insert, {keys} keys)"
    ));
    println!(
        "  {:<16} {:>10} {:>10} {:>12}",
        "design", "M/s", "ns/op", "reclaimed"
    );

    let mutex = Arc::new(Mutex::new(HashMap::<u64, u64>::with_capacity(
        keys as usize,
    )));
    {
        let mut map = mutex.lock().unwrap_or_else(|err| err.into_inner());
        for key in 0..keys {
            map.insert(key, key);
        }
    }
    let started = Instant::now();
    let mutex_ops: u64 = std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(threads);
        for thread_id in 0..threads {
            let mutex = Arc::clone(&mutex);
            joins.push(scope.spawn(move || {
                let mut ops = 0u64;
                let mut cursor = thread_id as u64;
                while started.elapsed() < sample {
                    let key = cursor % keys;
                    let mut map = mutex.lock().unwrap_or_else(|err| err.into_inner());
                    if cursor % 10 == 0 {
                        map.remove(&key);
                        map.insert(key, key);
                    } else {
                        std::hint::black_box(map.get(&key));
                    }
                    drop(map);
                    cursor = cursor.wrapping_add(1);
                    ops += 1;
                }
                ops
            }));
        }
        joins
            .into_iter()
            .map(|join| join.join().expect("mutex worker"))
            .sum()
    });
    let mutex_sample = Sample {
        ops: mutex_ops,
        secs: started.elapsed().as_secs_f64().max(1e-9),
        threads,
    };
    println!(
        "  {:<16} {:>10.2} {:>10.1} {:>12}",
        "mutex hashmap",
        mutex_sample.mops(),
        mutex_sample.ns(),
        0
    );

    let epoch = EpochMap::new(4096);
    {
        let handle = epoch.register();
        for key in 0..keys {
            handle.insert(key, key);
        }
        handle.flush();
    }
    let started = Instant::now();
    let epoch_ops: u64 = std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(threads);
        for thread_id in 0..threads {
            let epoch = Arc::clone(&epoch);
            joins.push(scope.spawn(move || {
                let handle = epoch.register();
                let mut ops = 0u64;
                let mut cursor = thread_id as u64;
                while started.elapsed() < sample {
                    let key = cursor % keys;
                    if cursor % 10 == 0 {
                        handle.remove(key);
                        std::hint::black_box(handle.insert(key, key));
                    } else {
                        std::hint::black_box(handle.get(key));
                    }
                    cursor = cursor.wrapping_add(1);
                    ops += 1;
                }
                handle.flush();
                ops
            }));
        }
        joins
            .into_iter()
            .map(|join| join.join().expect("epoch worker"))
            .sum()
    });
    for _ in 0..4 {
        let handle = epoch.register();
        handle.flush();
    }
    let epoch_sample = Sample {
        ops: epoch_ops,
        secs: started.elapsed().as_secs_f64().max(1e-9),
        threads,
    };
    println!(
        "  {:<16} {:>10.2} {:>10.1} {:>12}",
        "epoch chain",
        epoch_sample.mops(),
        epoch_sample.ns(),
        epoch.reclaimed()
    );
    println!(
        "  A removed node stays allocated while any pin that might hold it is
  still in the epoch it was removed in. The collector frees it two epoch
  steps later. The chain is sorted, and only the unlink winner retires it."
    );
}

pub fn run_wal(sample: Duration) {
    let writers = 4usize;
    section(format!(
        "group commit ({writers} writers, 1 committer, 16-byte records, 8 hot pages)"
    ));
    println!(
        "  {:<16} {:>10} {:>10} {:>10} {:>10}",
        "mode", "K/s", "ns/op", "syncs", "rec/sync"
    );
    for (name, batch) in [("fsync each", 1usize), ("batch 32", WAL_BATCH)] {
        let result = wal_once(writers, sample, batch);
        let per_sync = result.ops as f64 / result.syncs.max(1) as f64;
        println!(
            "  {:<16} {:>10.1} {:>10.0} {:>10} {:>10.1}",
            name,
            result.k_per_s(),
            result.ns(),
            result.syncs,
            per_sync
        );
    }
    println!(
        "  K/s is total records. ns/op is the average on each writer, including
  the wait until its prefix is durable. Writers keep 64 records in flight, so
  the committer can fill WAL_BATCH instead of syncing one queue's worth at the
  end. rec/sync should move toward that batch. A hole in the LSN prefix waits.
  Page eviction syncs the log before it writes a dirty frame back."
    );
}

pub fn run_cache() {
    section("working set (dependent load per cache line, then a sequential scan)");
    let l1 = clock::read_sysctl("hw.l1dcachesize").unwrap_or_else(|| "unknown".into());
    let l2 = clock::read_sysctl("hw.l2cachesize").unwrap_or_else(|| "unknown".into());
    let slc = clock::read_sysctl("hw.cachesize").unwrap_or_else(|| "unknown".into());
    println!("  hw.l1dcachesize {l1}");
    println!("  hw.l2cachesize  {l2}");
    println!("  hw.cachesize    {slc}");
    println!(
        "  32 KiB sits in L1. 256 KiB and 2 MiB fit in a 6 MiB L2. 8 MiB is just
  past that L2; if it stays near the 2 MiB number, the system cache absorbed it.
  64 MiB is large enough to leave L2 and can still hit hw.cachesize. A chase
  has to be larger than that cache before the load is a DRAM miss."
    );
    println!("  {:<12} {:>10} {:>12}", "bytes", "ns/load", "scan GB/s");
    let sizes = [
        32 * 1024,
        256 * 1024,
        2 * 1024 * 1024,
        8 * 1024 * 1024,
        64 * 1024 * 1024,
    ];
    for point in workingset::measure(&sizes) {
        println!(
            "  {:<12} {:>10.1} {:>12.2}",
            format_bytes(point.bytes),
            point.ns_per_load,
            point.scan_gbs
        );
    }
}

struct Sample {
    ops: u64,
    secs: f64,
    threads: usize,
}

impl Sample {
    fn mops(&self) -> f64 {
        self.ops as f64 / self.secs / 1e6
    }

    fn ns(&self) -> f64 {
        self.secs * self.threads as f64 / self.ops as f64 * 1e9
    }
}

struct WalSample {
    ops: u64,
    secs: f64,
    threads: usize,
    syncs: u64,
}

impl WalSample {
    fn k_per_s(&self) -> f64 {
        self.ops as f64 / self.secs / 1e3
    }

    fn ns(&self) -> f64 {
        self.secs * self.threads as f64 / self.ops.max(1) as f64 * 1e9
    }
}

const PIPELINE: u64 = 64;

fn wal_once(writers: usize, sample: Duration, batch: usize) -> WalSample {
    let dir =
        std::env::temp_dir().join(format!("memallocator-gap-{}-{}", std::process::id(), batch));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::create(&dir, 32).expect("create store"));
    let mut group = GroupCommit::start(Arc::clone(&store), batch).expect("start committer");
    let writer = group.writer();
    let start = Instant::now();
    let ops = std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(writers);
        for thread_id in 0..writers {
            let writer = writer.clone();
            joins.push(scope.spawn(move || {
                let mut ops = 0u64;
                let mut n = thread_id as u64;
                let mut payload = [0u8; 16];
                while start.elapsed() < sample {
                    let mut last = 0u64;
                    for _ in 0..PIPELINE {
                        payload[..8].copy_from_slice(&n.to_le_bytes());
                        let page = (n % 8) as u32;
                        last = writer
                            .submit(page, 0, &payload)
                            .expect("enqueue wal record");
                        n = n.wrapping_add(1);
                        ops += 1;
                    }
                    writer.wait(last).expect("durable prefix");
                }
                ops
            }));
        }
        joins
            .into_iter()
            .map(|join| join.join().expect("writer"))
            .sum()
    });
    let secs = start.elapsed().as_secs_f64().max(1e-9);
    let syncs = group.syncs();
    group.shutdown();
    drop(group);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
    WalSample {
        ops,
        secs,
        threads: writers,
        syncs,
    }
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .clamp(2, 8)
}

fn section(title: impl std::fmt::Display) {
    println!("\n{title}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{} KiB", bytes / 1024)
    }
}
