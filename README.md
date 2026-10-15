# memallocator

CPU slab allocators, a bounded lock-free queue, concurrent maps, and a Metal GPU suballocator. Built to run on an Apple Silicon Mac.

The point is the comparison: thread cache versus a shared freelist, mutex versus lock-free, packed atomics versus cache-line padding, and one `MTLBuffer` per allocation versus a device bump allocator.

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
```

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

## How to read a result

- **M/s** is total throughput. **ns/op** is the average on each thread.
- **tcache** should dominate once several threads share a size class. A hit is a `Vec` pop on the allocating thread.
- A **lock-free freelist with no cache** can lose to a mutex. Every miss is a CAS retry on one cache line. The cache is what scales; the lock-free list is the miss path.
- A **spin mutex** can lose to a **blocking mutex** at 8 threads. Spinners keep invalidating the line the owner needs. Parking them lets the owner finish.
- **packed** counters share a 128-byte line (M-series cache line). **padded** counters do not. The gap is false sharing.
- **p99** is the tail of alloc+free while several threads share one size class. The mach timer on this chip is coarser than a fast allocation, so a p50 near the timer resolution means "the fast path is under the clock," not "the allocation took that long."
- **GPU freelist** should be far below **GPU bump**. One contended CAS head does not scale across GPU threads. The bump counter is a single atomic add.
