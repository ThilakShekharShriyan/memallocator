#include <metal_stdlib>
using namespace metal;

// Device-side suballocators.
//
// bump_alloc is a shared atomic counter. It is the usual GPU heap: one
// relaxed fetch-add, no reuse.
//
// freelist_pop is a Treiber pop. This kernel only pops, so the list only
// shrinks and the ABA problem cannot occur. The CPU freelist is different:
// alloc and free run together, so the CPU head carries a generation tag.
//
// This Metal compiler only accepts memory_order_relaxed on device atomics.
// That is enough here: the CPU builds the freelist before the dispatch, the
// kernel never pushes, and the CAS itself is still atomic. The CPU freelist
// uses release/acquire because alloc and free publish links to other cores.

struct Params {
    uint repeats;
    uint empty;
    uint capacity;
    uint pad;
};

kernel void bump_alloc(
    device atomic_uint *bump [[buffer(0)]],
    device uint *last [[buffer(1)]],
    constant Params &params [[buffer(2)]],
    uint tid [[thread_position_in_grid]]
) {
    uint value = 0;
    for (uint r = 0; r < params.repeats; ++r) {
        value = atomic_fetch_add_explicit(bump, 1u, memory_order_relaxed);
    }
    last[tid] = value;
}

kernel void freelist_pop(
    device atomic_uint *head [[buffer(0)]],
    device atomic_uint *nexts [[buffer(1)]],
    device uint *wins [[buffer(2)]],
    constant Params &params [[buffer(3)]],
    uint tid [[thread_position_in_grid]]
) {
    uint won = 0;
    for (uint r = 0; r < params.repeats; ++r) {
        uint h = atomic_load_explicit(head, memory_order_relaxed);
        bool took = false;
        while (h != params.empty && h < params.capacity) {
            uint nxt = atomic_load_explicit(&nexts[h], memory_order_relaxed);
            uint expected = h;
            if (atomic_compare_exchange_weak_explicit(
                    head,
                    &expected,
                    nxt,
                    memory_order_relaxed,
                    memory_order_relaxed)) {
                took = true;
                break;
            }
            h = expected;
        }
        if (!took) {
            break;
        }
        won += 1;
    }
    wins[tid] = won;
}
