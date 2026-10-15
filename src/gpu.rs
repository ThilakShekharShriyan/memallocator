//! Metal bump allocator and pop-only freelist on the Apple GPU.
//!
//! The shader source is `shaders/gpu_alloc.metal`. It is compiled at runtime
//! with `newLibraryWithSource`, which works with the system Metal compiler
//! even when the `metal` CLI is not installed.

use metal::objc::{msg_send, sel, sel_impl};
use metal::{
    Buffer, CommandQueue, CompileOptions, ComputePipelineState, Device, Library,
    MTLCommandBufferStatus, MTLResourceOptions, MTLSize,
};
use std::time::Instant;

const SHADER: &str = include_str!("../shaders/gpu_alloc.metal");
const EMPTY: u32 = u32::MAX;

#[repr(C)]
struct Params {
    repeats: u32,
    empty: u32,
    capacity: u32,
    pad: u32,
}

pub struct GpuReport {
    pub device: String,
    pub threads: u32,
    pub bump_repeats: u32,
    pub freelist_repeats: u32,
    pub bump_mops: f64,
    pub bump_us: f64,
    pub bump_used_gpu_time: bool,
    pub freelist_mops: f64,
    pub freelist_us: f64,
    pub freelist_used_gpu_time: bool,
    pub buffer_count: usize,
    pub buffer_bytes: u64,
    pub many_buffers_us: f64,
    pub one_buffer_us: f64,
    pub cpu_suballoc_ns: f64,
}

impl GpuReport {
    pub fn print(&self) {
        println!("\nGPU  {}", self.device);
        let bump_clock = if self.bump_used_gpu_time {
            "gpu"
        } else {
            "wall"
        };
        let free_clock = if self.freelist_used_gpu_time {
            "gpu"
        } else {
            "wall"
        };
        let bump_ops = u64::from(self.threads) * u64::from(self.bump_repeats);
        let free_ops = u64::from(self.threads) * u64::from(self.freelist_repeats);
        println!(
            "  bump fetch_add     {:>8.2} M/s   {}  ({bump_clock}, {bump_ops} ops, {} threads x {} repeats)",
            self.bump_mops,
            fmt_time(self.bump_us),
            self.threads,
            self.bump_repeats
        );
        println!(
            "  freelist CAS pop    {:>8.2} M/s   {}  ({free_clock}, {free_ops} ops, {} threads x {} repeats)",
            self.freelist_mops,
            fmt_time(self.freelist_us),
            self.threads,
            self.freelist_repeats
        );
        println!(
            "  {} x {}-byte MTLBuffer     {:>8.1} us total   {:>7.1} us each",
            self.buffer_count,
            self.buffer_bytes,
            self.many_buffers_us,
            self.many_buffers_us / self.buffer_count as f64
        );
        println!(
            "  one MTLBuffer of the same bytes {:>8.1} us",
            self.one_buffer_us
        );
        println!(
            "  CPU suballoc inside that buffer {:>8.1} ns each",
            self.cpu_suballoc_ns
        );
    }
}

struct Gpu {
    device: Device,
    queue: CommandQueue,
    #[allow(dead_code)]
    library: Library,
    bump: ComputePipelineState,
    freelist: ComputePipelineState,
}

impl Gpu {
    fn new() -> Result<Self, String> {
        let device = Device::system_default().ok_or("no Metal device")?;
        let library = device.new_library_with_source(SHADER, &CompileOptions::new())?;
        let bump_fn = library.get_function("bump_alloc", None)?;
        let free_fn = library.get_function("freelist_pop", None)?;
        let bump = device.new_compute_pipeline_state_with_function(&bump_fn)?;
        let freelist = device.new_compute_pipeline_state_with_function(&free_fn)?;
        let queue = device.new_command_queue();
        Ok(Self {
            device,
            queue,
            library,
            bump,
            freelist,
        })
    }

    fn exercise(&self, threads: u32, repeats: u32) -> Result<(), String> {
        self.run_bump(threads, repeats)?;
        self.run_freelist(threads, repeats)?;
        Ok(())
    }

    fn run_bump(&self, threads: u32, repeats: u32) -> Result<Timing, String> {
        let bump = self.buffer(4);
        let last = self.buffer(u64::from(threads) * 4);
        write_u32(&bump, 0, 0);
        let params = Params {
            repeats,
            empty: EMPTY,
            capacity: threads.saturating_mul(repeats),
            pad: 0,
        };
        let timing = self.dispatch(&self.bump, &[&bump, &last], &params, threads, 2)?;
        let counter = read_u32(&bump, 0);
        let expected = threads.saturating_mul(repeats);
        if counter != expected {
            return Err(format!("bump counter {counter} != {expected}"));
        }
        if repeats == 1 {
            let values = read_u32s(&last, threads as usize);
            if !is_permutation(&values) {
                return Err("bump outputs are not a permutation".to_string());
            }
        }
        Ok(timing)
    }

    fn run_freelist(&self, threads: u32, repeats: u32) -> Result<Timing, String> {
        let capacity = threads.saturating_mul(repeats);
        let head = self.buffer(4);
        let nexts = self.buffer(u64::from(capacity) * 4);
        let wins = self.buffer(u64::from(threads) * 4);
        write_u32(&head, 0, 0);
        seed_freelist(&nexts, capacity);
        let params = Params {
            repeats,
            empty: EMPTY,
            capacity,
            pad: 0,
        };
        let timing = self.dispatch(&self.freelist, &[&head, &nexts, &wins], &params, threads, 3)?;
        let left = read_u32(&head, 0);
        if left != EMPTY {
            return Err(format!("freelist head {left:#x} was not empty"));
        }
        let won: u64 = read_u32s(&wins, threads as usize)
            .into_iter()
            .map(u64::from)
            .sum();
        if won != u64::from(capacity) {
            return Err(format!("freelist pops {won} != {capacity}"));
        }
        Ok(timing)
    }

    fn dispatch(
        &self,
        pipeline: &ComputePipelineState,
        buffers: &[&Buffer],
        params: &Params,
        threads: u32,
        params_index: u64,
    ) -> Result<Timing, String> {
        let command = self.queue.new_command_buffer();
        let encoder = command.new_compute_command_encoder();
        encoder.set_compute_pipeline_state(pipeline);
        for (index, buffer) in buffers.iter().enumerate() {
            encoder.set_buffer(index as u64, Some(*buffer), 0);
        }
        encoder.set_bytes(
            params_index,
            std::mem::size_of::<Params>() as u64,
            params as *const Params as *const _,
        );
        let limit = pipeline.max_total_threads_per_threadgroup().clamp(1, 256);
        let group = limit.min(u64::from(threads)).max(1);
        encoder.dispatch_threads(
            MTLSize::new(u64::from(threads), 1, 1),
            MTLSize::new(group, 1, 1),
        );
        encoder.end_encoding();
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let wall_start = Instant::now();
        command.commit();
        command.wait_until_completed();
        let wall = wall_start.elapsed().as_secs_f64();
        if command.status() != MTLCommandBufferStatus::Completed {
            return Err(format!("command buffer status {:?}", command.status()));
        }
        let gpu = gpu_seconds(&command);
        Ok(Timing { gpu, wall })
    }

    fn buffer(&self, bytes: u64) -> Buffer {
        self.device
            .new_buffer(bytes.max(4), MTLResourceOptions::StorageModeShared)
    }
}

struct Timing {
    gpu: f64,
    wall: f64,
}

impl Timing {
    fn report(&self, ops: u64) -> (f64, f64, bool) {
        if self.gpu > 0.0 {
            (ops as f64 / self.gpu / 1e6, self.gpu * 1e6, true)
        } else {
            (ops as f64 / self.wall / 1e6, self.wall * 1e6, false)
        }
    }
}

pub fn run(thorough: bool) -> Result<GpuReport, String> {
    let gpu = Gpu::new()?;
    gpu.exercise(2048, 1)?;
    let threads: u32 = if thorough { 65_536 } else { 16_384 };
    // The freelist is one contended CAS head. Fewer repeats than the bump
    // keeps the run short; the rate is what to compare.
    let bump_repeats: u32 = if thorough { 32 } else { 8 };
    let freelist_repeats: u32 = 4;
    let bump = gpu.run_bump(threads, bump_repeats)?;
    let freelist = gpu.run_freelist(threads, freelist_repeats)?;
    let (bump_mops, bump_us, bump_used_gpu_time) =
        bump.report(u64::from(threads) * u64::from(bump_repeats));
    let (freelist_mops, freelist_us, freelist_used_gpu_time) =
        freelist.report(u64::from(threads) * u64::from(freelist_repeats));

    let buffer_count = if thorough { 4096 } else { 1024 };
    let buffer_bytes = 4096u64;
    let many_buffers_us = median_us(
        || time_many_buffers(&gpu.device, buffer_count, buffer_bytes),
        3,
    );
    let one_buffer_us = median_us(
        || {
            let started = Instant::now();
            let buffer = gpu.device.new_buffer(
                buffer_bytes * buffer_count as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let elapsed = started.elapsed().as_secs_f64() * 1e6;
            std::hint::black_box(&buffer);
            elapsed
        },
        3,
    );
    let heap = gpu.device.new_buffer(
        buffer_bytes * buffer_count as u64,
        MTLResourceOptions::StorageModeShared,
    );
    let cpu_suballoc_ns = time_cpu_suballoc(heap.contents(), buffer_count, buffer_bytes as usize);

    Ok(GpuReport {
        device: gpu.device.name().to_string(),
        threads,
        bump_repeats,
        freelist_repeats,
        bump_mops,
        bump_us,
        bump_used_gpu_time,
        freelist_mops,
        freelist_us,
        freelist_used_gpu_time,
        buffer_count,
        buffer_bytes,
        many_buffers_us,
        one_buffer_us,
        cpu_suballoc_ns,
    })
}

fn fmt_time(us: f64) -> String {
    if us >= 1_000.0 {
        format!("{:>8.2} ms", us / 1_000.0)
    } else {
        format!("{:>8.1} us", us)
    }
}

fn time_many_buffers(device: &Device, count: usize, bytes: u64) -> f64 {
    let started = Instant::now();
    let mut buffers = Vec::with_capacity(count);
    for _ in 0..count {
        buffers.push(device.new_buffer(bytes, MTLResourceOptions::StorageModeShared));
    }
    let elapsed = started.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(&buffers);
    elapsed
}

fn time_cpu_suballoc(base: *mut std::ffi::c_void, count: usize, bytes: usize) -> f64 {
    let started = Instant::now();
    let mut offset = 0usize;
    for _ in 0..count {
        unsafe {
            // SAFETY: `base` is a shared MTLBuffer of `count * bytes` bytes,
            // and `offset` steps through non-overlapping 1-byte writes.
            (base as *mut u8).add(offset).write_volatile(1);
        }
        offset += bytes;
    }
    std::hint::black_box(offset);
    started.elapsed().as_secs_f64() * 1e9 / count as f64
}

fn median_us(mut body: impl FnMut() -> f64, samples: usize) -> f64 {
    let mut values: Vec<f64> = (0..samples).map(|_| body()).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn seed_freelist(buffer: &Buffer, capacity: u32) {
    unsafe {
        // SAFETY: the buffer holds `capacity` u32 links and the GPU is idle.
        let nexts = buffer.contents() as *mut u32;
        for index in 0..capacity - 1 {
            nexts.add(index as usize).write((index + 1) as u32);
        }
        nexts.add((capacity - 1) as usize).write(EMPTY);
    }
}

fn write_u32(buffer: &Buffer, index: usize, value: u32) {
    unsafe {
        // SAFETY: shared buffer, in range, GPU idle.
        (buffer.contents() as *mut u32).add(index).write(value);
    }
}

fn read_u32(buffer: &Buffer, index: usize) -> u32 {
    unsafe {
        // SAFETY: shared buffer, in range, command buffer has completed.
        (buffer.contents() as *const u32).add(index).read()
    }
}

fn read_u32s(buffer: &Buffer, len: usize) -> Vec<u32> {
    unsafe {
        // SAFETY: shared buffer of at least `len` u32s, GPU idle.
        let ptr = buffer.contents() as *const u32;
        std::slice::from_raw_parts(ptr, len).to_vec()
    }
}

fn is_permutation(values: &[u32]) -> bool {
    let mut seen = vec![false; values.len()];
    for &value in values {
        let index = value as usize;
        if index >= seen.len() || seen[index] {
            return false;
        }
        seen[index] = true;
    }
    true
}

#[allow(unexpected_cfgs)]
fn gpu_seconds(command: &metal::CommandBufferRef) -> f64 {
    let start: f64 = unsafe {
        // SAFETY: GPUStartTime is an MTLCommandBuffer method. It is valid
        // after waitUntilCompleted and returns seconds as CFTimeInterval.
        msg_send![command, GPUStartTime]
    };
    let end: f64 = unsafe { msg_send![command, GPUEndTime] };
    (end - start).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_dispatch_matches_the_contract() {
        let gpu = Gpu::new().expect("metal device");
        gpu.exercise(1024, 1).expect("gpu allocators");
    }
}
