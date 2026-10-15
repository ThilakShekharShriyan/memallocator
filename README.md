# memallocator

CPU slab allocators, a bounded lock-free queue, concurrent maps, an epoch-reclaimed map, a small write-ahead log, and a Metal GPU suballocator.

The result is the comparison. A thread cache beats a shared freelist. A spin lock can lose to a blocking mutex. Padding one counter removes false sharing. Group commit turns one `sync_data` per record into one `sync_data` per batch. This is a study of those effects on one machine. It is not a production allocator or a storage engine.

## Measured on this machine

Apple M5, 10 CPU cores, 24 GB RAM, 128-byte cache line. L1 data cache 64 KiB, L2 6 MiB, `hw.cachesize` about 3.2 GiB (the system-level cache in front of DRAM). Release build, one run of `cargo run --release -- cpu` and one run of `cargo run --release -- gaps`. Absolute rates move between runs. The ranking is the result.

**Thread cache, 64-byte alloc+free, 8 threads.** The lock-free freelist is the fastest design at 1 thread (86 M/s) and the slowest at 8 (1.3 M/s). The same freelist with a thread cache holds about 340 M/s. Taking the lock off the fast path matters more than which lock is on the slow path.

| design | 8 threads |
| --- | ---: |
| system malloc | 130 M/s, 61 ns/op |
| mutex, blocking | 17 M/s, 475 ns/op |
| mutex, spin | 2.0 M/s, 4.0 µs/op |
| mutex, spin + thread cache | 338 M/s, 24 ns/op |
| lock-free | 1.3 M/s, 6.0 µs/op |
| lock-free + thread cache | 343 M/s, 23 ns/op |

**False sharing, 8 threads, one counter each.** Packed counters share a line. Counters aligned to 128 bytes do not.

| layout | per thread |
| --- | ---: |
| packed | 49 ns/op |
| padded to 128 bytes | 2.5 ns/op |

**Group commit, 4 writers, 1 committer, 16-byte records.** Writers keep 64 records in flight. `rec/sync` is records per `sync_data`.

| mode | throughput | records per sync |
| --- | ---: | ---: |
| sync each record | 0.3 K/s | 1 |
| batch of 32 | 7.9 K/s | 32 |

## What this does not prove

- The maps do not resize. The open-addressing map does not delete. The chained map deletes and reclaims, and its buckets are fixed.
- There is no fragmentation curve against jemalloc or mimalloc. The allocator comparison is against the system allocator and the designs in this repo.
- The mach timer resolves 41 ns. A p50 of 41 ns means the fast path is under the clock.
- There are no hardware performance counters here. The working-set chase is the cache measurement. A flat step from 2 MiB to 8 MiB is the system-level cache, not proof that the load reached DRAM.
- The log has one committer thread. Durability is `sync_data` on the log, then replay. It is not a database.

## Run

```sh
cargo test
cargo run --release
```

`cargo run --release` runs CPU and GPU. Narrower runs:

```sh
cargo run --release -- cpu
cargo run --release -- gpu
cargo run --release -- quick
cargo run --release -- gaps
```

`gaps` is the delete, durability, and working-set run. The pieces also run on their own: `epoch`, `wal`, and `cache`. `all` stays the original suite.

Release is the number that matters. Debug builds keep double-free checks and do not inline the same way.

## Layout

| Piece | Where | Unsafe? |
| --- | --- | --- |
| Size-class slab | `src/alloc` | Backing `alloc` / `dealloc` only |
| Spin or blocking mutex freelist | `src/alloc/mutex_alloc.rs` | No |
| Tagged Treiber freelist | `src/alloc/lockfree.rs` | No. Indices, not pointers |
| Thread cache | `src/alloc/tcache.rs` | No |
| Bump and atomic bump | `src/alloc/bump.rs` | No, beyond the slab |
| Mutex queue | `src/queue.rs` | No |
| Vyukov MPMC queue | `src/queue.rs` | Slot write/read, invariant on the sequence word |
| Mutex map, 16 shards | `src/map.rs` | No |
| Fixed open-addressing map | `src/map.rs` | No. No delete, so no hazard pointers |
| Epoch collector | `src/epoch.rs` | Drop of a retired allocation, two epochs later |
| Chained map with delete | `src/epoch_map.rs` | Node links. The unlink winner retires the node |
| Page cache and WAL | `src/store.rs` | No. Group commit uses the MPMC queue |
| Pointer chase | `src/workingset.rs` | No |
| Release/acquire publish | `src/ordering.rs` | No |
| Metal bump and freelist | `shaders/gpu_alloc.metal`, `src/gpu.rs` | Timestamp message sends and buffer reads |

## Memory ordering

Apple Silicon is weakly ordered. A release store pairs with an acquire load. That is enough for "payload first, then publish."

- Freelist push stores `next`, then Release-CASes the head. Pop Acquire-loads the head, then reads `next`.
- The MPMC producer writes the value, then Release-stores `sequence`. The consumer Acquire-loads `sequence`, then reads the value.
- The open-addressing map stores key and value, then Release-stores `ready`.
- `Relaxed` is used for counters that do not publish other data.
- `SeqCst` is not used in the structures. The ordering bench shows what it costs on one contended atomic.

The freelist head is `(generation << 32) | index`. The generation is the ABA tag: a stalled pop must not succeed after that index was freed and reused.

## GPU

The GPU path compiles the Metal shader at runtime, so it does not need the `metal` command-line tool. Two kernels:

- `bump_alloc` does `atomic_fetch_add` on one device counter. This is the usual GPU heap.
- `freelist_pop` is a CAS pop. The kernel never pushes, so ABA cannot happen. The CPU freelist does both, which is why it carries a tag. Device atomics on this Metal compiler are `relaxed`; the CAS is still atomic, and the CPU publishes the links before the dispatch.

A separate measurement creates thousands of `MTLBuffer`s versus one buffer plus pointer bumping. Device allocations are kernel calls. Suballocation is arithmetic.

## Delete, durability, working set

```sh
cargo run --release -- gaps
```

Three knobs, each with a place to change it:

- **Epoch delete.** `src/epoch.rs` keeps a removed map node alive until every pin that could hold it has ended. `ADVANCE_EVERY` is how often a thread tries to move the epoch. The map in `src/epoch_map.rs` is a sorted Harris list: the low bit of `next` marks a delete, and only the thread that unlinks the node retires it.
- **Group commit.** Writers enqueue log records on the MPMC queue and do not wait for each one. One committer syncs a contiguous LSN prefix, then publishes the flushed LSN. `WAL_BATCH` in `src/store.rs` is the batch size while the queue stays full. The page cache is a clock. A dirty frame is written back only after its log records are synced, so the page file does not get ahead of the log. Replay is by absolute page offset, and a torn tail record is dropped.
- **Working set.** `src/workingset.rs` chases one cache line at a time through a single cycle. On this machine the chase was about 2.7 ns at 32 KiB, 13 ns at 2 MiB, 16 ns at 8 MiB, and 157 ns at 64 MiB. 8 MiB is just past the 6 MiB L2 and stayed near the 2 MiB cost. 64 MiB left L2 and can still hit the system-level cache.

## How to read a result

- **M/s** is total throughput. **ns/op** is the average on each thread.
- **tcache** should dominate once several threads share a size class. A hit is a `Vec` pop on the allocating thread.
- A **lock-free freelist with no cache** can lose to a mutex. Every miss is a CAS retry on one cache line. The cache is what scales; the lock-free list is the miss path.
- A **spin mutex** can lose to a **blocking mutex** at 8 threads. Spinners keep invalidating the line the owner needs. Parking them lets the owner finish.
- **packed** counters share a 128-byte line (M-series cache line). **padded** counters do not. The gap is false sharing.
- **p99** is the tail of alloc+free while several threads share one size class. The mach timer on this chip is coarser than a fast allocation, so a p50 near the timer resolution means "the fast path is under the clock," not "the allocation took that long."
- **GPU freelist** should be far below **GPU bump**. One contended CAS head does not scale across GPU threads. The bump counter is a single atomic add.
