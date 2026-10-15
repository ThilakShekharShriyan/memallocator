//! CPU slab allocation, concurrent structures, and a Metal GPU suballocator.
//!
//! The algorithms are safe Rust. `unsafe` is limited to the slab's backing
//! allocation, raw pointers handed to the caller, the lock-free queue's slot
//! storage, epoch retirement of map nodes, and the Metal timestamp calls.
//! Each site states the invariant.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod alloc;
pub mod bench;
pub mod clock;
pub mod epoch;
pub mod epoch_map;
pub mod gaps;
pub mod gpu;
pub mod map;
pub mod ordering;
pub mod pad;
pub mod queue;
pub mod slab;
pub mod store;
pub mod syncutil;
pub mod workingset;

pub use alloc::{AtomicBump, Bump, LockFreeAlloc, MutexAlloc};
pub use epoch_map::EpochMap;
pub use map::{LockFreeMap, MapFull, MutexMap, ShardedMap};
pub use queue::{MpmcQueue, MutexQueue};
pub use store::{GroupCommit, Store};
