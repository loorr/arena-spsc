//! # arena-spsc
//!
//! A lock-free, single-producer single-consumer (SPSC) byte channel backed by a
//! flat arena buffer.
//!
//! ## Design
//!
//! Messages are stored inline as `[len: u32][data][padding]`, where each slot is
//! aligned to 64 bytes (one cache line). A zero-length header serves as a wrap
//! sentinel, allowing the consumer to jump back to the start of the arena
//! without a secondary ring structure.
//!
//! The producer publishes data by storing the new write position with `Release`
//! ordering. The consumer observes published data with an `Acquire` load of the
//! write position and returns space by storing the read position with `Release`
//! ordering.
//!
//! ## Platform support
//!
//! - **Linux**: the arena is allocated via `mmap` with `MADV_HUGEPAGE` for THP
//!   (transparent huge pages) and pre-faulted to eliminate runtime page-fault
//!   jitter.
//! - **Other platforms**: falls back to aligned heap allocation with the same
//!   pre-fault strategy.
//!
//! ## Example
//!
//! ```rust
//! use arena_spsc::{channel, spin_pause};
//! use std::sync::{Arc, Barrier};
//!
//! let (mut producer, mut consumer) = channel(2 * 1024 * 1024);
//! let start = Arc::new(Barrier::new(2));
//! let start_c = Arc::clone(&start);
//!
//! let worker = std::thread::spawn(move || {
//!     let mut received = Vec::new();
//!     start_c.wait();
//!     // Receive exactly 2 messages
//!     while received.len() < 2 {
//!         consumer.recv_spin(|data| {
//!             received.push(data.to_vec());
//!         });
//!     }
//!     received
//! });
//!
//! start.wait();
//! while !producer.send(b"hello") { spin_pause(); }
//! while !producer.send(b"world") { spin_pause(); }
//!
//! let received = worker.join().unwrap();
//! assert_eq!(received, vec![b"hello".to_vec(), b"world".to_vec()]);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Issues a short spin-loop hint for busy-wait paths.
///
/// Uses the platform-optimal instruction: `PAUSE` on x86_64, `YIELD` on
/// aarch64, and [`std::hint::spin_loop`] elsewhere.
#[inline(always)]
pub fn spin_pause() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_pause();
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("yield", options(nomem, nostack));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        std::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Arena buffer
// ---------------------------------------------------------------------------

/// Owns the contiguous storage used by the channel.
///
/// On Linux the buffer is backed by `mmap` and released with `munmap`. Other
/// platforms fall back to a heap allocation with the same alignment.
struct ArenaBuffer {
    ptr: *mut u8,
    capacity: usize,
    #[cfg(target_os = "linux")]
    is_mmap: bool,
}

unsafe impl Send for ArenaBuffer {}
unsafe impl Sync for ArenaBuffer {}

impl ArenaBuffer {
    fn new(capacity: usize) -> Self {
        #[cfg(target_os = "linux")]
        {
            unsafe {
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    capacity,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
                libc::madvise(ptr, capacity, libc::MADV_HUGEPAGE);
                // Pre-fault: touch every page to eliminate runtime page-fault jitter.
                let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, capacity);
                for chunk in slice.chunks_mut(4096) {
                    chunk[0] = 0;
                }
                ArenaBuffer {
                    ptr: ptr as *mut u8,
                    capacity,
                    is_mmap: true,
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let layout = std::alloc::Layout::from_size_align(capacity, 64).expect("invalid layout");
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "allocation failed");
            // Pre-fault: touch every page.
            unsafe {
                let slice = std::slice::from_raw_parts_mut(ptr, capacity);
                for chunk in slice.chunks_mut(4096) {
                    chunk[0] = 0;
                }
            }
            ArenaBuffer { ptr, capacity }
        }
    }
}

impl Drop for ArenaBuffer {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if self.is_mmap {
                unsafe {
                    libc::munmap(self.ptr as *mut libc::c_void, self.capacity);
                }
                return;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let layout =
                std::alloc::Layout::from_size_align(self.capacity, 64).expect("invalid layout");
            unsafe {
                std::alloc::dealloc(self.ptr, layout);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Keeps frequently updated atomics on separate cache lines to prevent false
/// sharing between the producer and consumer.
#[repr(C, align(64))]
struct CachePadded<T>(T);

/// Size of the inline length header written before each message.
const HEADER_SIZE: usize = 4;

/// Shared state visible to both ends of the channel.
struct SharedState {
    buffer: ArenaBuffer,
    capacity: usize,
    /// Producer stores, consumer loads.
    write_pos: CachePadded<AtomicU64>,
    /// Consumer stores, producer loads.
    read_pos: CachePadded<AtomicU64>,
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// The sending half of an arena-backed SPSC channel.
///
/// Created by [`channel`]. Sends byte messages into the shared arena with
/// zero-copy semantics.
pub struct Producer {
    /// Local shadow of `write_pos` (avoids atomic load on the hot path).
    write_pos: u64,
    /// Cached snapshot of the consumer's `read_pos` (amortized atomic load).
    read_pos_cache: u64,
    state: Arc<SharedState>,
}

impl Producer {
    /// Attempts to write a single message into the arena.
    ///
    /// Returns `true` on success, `false` if the consumer has not released
    /// enough space yet (back-pressure). The caller should spin or yield and
    /// retry.
    ///
    /// # Panics (debug only)
    ///
    /// Panics if `data` is empty (would collide with the wrap sentinel) or
    /// larger than half the arena capacity.
    #[inline(always)]
    pub fn send(&mut self, data: &[u8]) -> bool {
        let state = &*self.state;
        let capacity = state.capacity;
        let buf_ptr = state.buffer.ptr;

        let raw_len = data.len();
        debug_assert!(
            raw_len > 0,
            "empty message would collide with wrap sentinel (len=0)"
        );
        let total = align64(HEADER_SIZE + raw_len);

        debug_assert!(
            total < capacity / 2,
            "message too large: {} >= capacity/2 ({})",
            total,
            capacity / 2
        );

        let wp = self.write_pos;
        let offset = (wp as usize) & (capacity - 1);

        // Check if the message fits before the arena end; otherwise wrap.
        let (write_offset, wp) = if offset + total <= capacity {
            (offset, wp)
        } else {
            // Cold path: write wrap sentinel (len=0), jump to arena start.
            let result = write_wrap_sentinel(buf_ptr, capacity, offset, wp);
            // Save immediately so the next send() won't repeat the wrap.
            self.write_pos = result.1;
            result
        };

        // Refresh the cached read position only when the arena appears full.
        let used = wp - self.read_pos_cache;
        if used as usize + total > capacity {
            self.read_pos_cache = state.read_pos.0.load(Ordering::Acquire);
            if (wp - self.read_pos_cache) as usize + total > capacity {
                return false;
            }
        }

        // Write the inline header and payload directly into the arena slot.
        unsafe {
            let base = buf_ptr.add(write_offset);
            (base as *mut u32).write(raw_len as u32);
            std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(HEADER_SIZE), raw_len);
        }

        let new_wp = wp + total as u64;
        self.write_pos = new_wp;

        // Publish the new write position after the payload is visible.
        state.write_pos.0.store(new_wp, Ordering::Release);

        true
    }

    /// Attempts to write a message assembled from two borrowed slices.
    ///
    /// Both slices are copied into a single arena slot with no intermediate
    /// allocation, making this ideal for prepending a header to a payload.
    /// Returns `false` if the consumer has not released enough space yet.
    #[inline(always)]
    pub fn send_parts(&mut self, part1: &[u8], part2: &[u8]) -> bool {
        let state = &*self.state;
        let capacity = state.capacity;
        let buf_ptr = state.buffer.ptr;

        let raw_len = part1.len() + part2.len();
        debug_assert!(
            raw_len > 0,
            "empty message would collide with wrap sentinel (len=0)"
        );
        let total = align64(HEADER_SIZE + raw_len);

        debug_assert!(
            total < capacity / 2,
            "message too large: {} >= capacity/2 ({})",
            total,
            capacity / 2
        );

        let wp = self.write_pos;
        let offset = (wp as usize) & (capacity - 1);

        let (write_offset, wp) = if offset + total <= capacity {
            (offset, wp)
        } else {
            let result = write_wrap_sentinel(buf_ptr, capacity, offset, wp);
            self.write_pos = result.1;
            result
        };

        let used = wp - self.read_pos_cache;
        if used as usize + total > capacity {
            self.read_pos_cache = state.read_pos.0.load(Ordering::Acquire);
            if (wp - self.read_pos_cache) as usize + total > capacity {
                return false;
            }
        }

        unsafe {
            let base = buf_ptr.add(write_offset);
            (base as *mut u32).write(raw_len as u32);
            std::ptr::copy_nonoverlapping(part1.as_ptr(), base.add(HEADER_SIZE), part1.len());
            std::ptr::copy_nonoverlapping(
                part2.as_ptr(),
                base.add(HEADER_SIZE + part1.len()),
                part2.len(),
            );
        }

        let new_wp = wp + total as u64;
        self.write_pos = new_wp;
        state.write_pos.0.store(new_wp, Ordering::Release);

        true
    }
}

/// Writes the wrap sentinel and advances the logical write position.
#[cold]
fn write_wrap_sentinel(buf_ptr: *mut u8, capacity: usize, offset: usize, wp: u64) -> (usize, u64) {
    debug_assert!(
        offset + HEADER_SIZE <= capacity,
        "offset({offset}) + HEADER_SIZE({HEADER_SIZE}) > capacity({capacity})"
    );
    unsafe {
        (buf_ptr.add(offset) as *mut u32).write(0u32);
    }
    let skip = (capacity - offset) as u64;
    (0, wp + skip)
}

// ---------------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------------

/// The receiving half of an arena-backed SPSC channel.
///
/// Created by [`channel`]. Reads byte messages directly from the shared arena
/// with zero-copy access.
pub struct Consumer {
    /// Local shadow of `read_pos`.
    read_pos: u64,
    state: Arc<SharedState>,
}

impl Consumer {
    /// Non-blocking receive. Drains all currently published messages and
    /// passes each one to `f`.
    ///
    /// Returns `true` if at least one message was processed, `false` if the
    /// queue was empty. The byte slice passed to `f` borrows directly from
    /// the shared arena and **must not** be retained after the callback
    /// returns.
    #[inline(always)]
    pub fn try_recv<F>(&mut self, mut f: F) -> bool
    where
        F: FnMut(&[u8]),
    {
        let state = &*self.state;
        let capacity = state.capacity;
        let buf_ptr = state.buffer.ptr;
        let rp = self.read_pos;

        let wp = state.write_pos.0.load(Ordering::Acquire);
        if wp <= rp {
            return false;
        }

        let mut pos = rp;
        while pos < wp {
            let offset = (pos as usize) & (capacity - 1);

            let len = unsafe { (buf_ptr.add(offset) as *const u32).read() };

            if len == 0 {
                let skip = (capacity - offset) as u64;
                pos += skip;
                continue;
            }

            let total = align64(HEADER_SIZE + len as usize);

            let data = unsafe {
                std::slice::from_raw_parts(buf_ptr.add(offset + HEADER_SIZE), len as usize)
            };

            // Prefetch the next message's cache line while processing the current one.
            prefetch_read(unsafe { buf_ptr.add((pos as usize + total) & (capacity - 1)) });

            f(data);

            pos += total as u64;
        }

        self.read_pos = pos;
        state.read_pos.0.store(pos, Ordering::Release);

        true
    }

    /// Busy-waits for published messages, then processes the current readable
    /// batch.
    ///
    /// The byte slice passed to `f` borrows directly from the shared arena and
    /// **must not** be retained after the callback returns.
    #[inline(always)]
    pub fn recv_spin<F>(&mut self, mut f: F)
    where
        F: FnMut(&[u8]),
    {
        while !self.try_recv(&mut f) {
            spin_pause();
            spin_pause();
        }
    }

    /// Repeatedly receives and processes messages in an infinite loop.
    pub fn run<F>(&mut self, mut f: F) -> !
    where
        F: FnMut(&[u8]),
    {
        loop {
            self.recv_spin(&mut f);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rounds a slot size up to cache-line alignment (64 bytes).
#[inline(always)]
const fn align64(n: usize) -> usize {
    (n + 63) & !63
}

/// Prefetches the cache line at `ptr` into L1 for reading.
///
/// Uses the platform-optimal prefetch instruction: `PREFETCHT0` on x86_64,
/// `PRFM PLDL1KEEP` on aarch64, and is a no-op elsewhere.
#[inline(always)]
fn prefetch_read(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "prfm pldl1keep, [{addr}]",
            addr = in(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}

/// Pins the current thread to the specified CPU core.
///
/// This can reduce scheduling jitter and cache-miss overhead for latency-
/// sensitive workloads.
///
/// # Platform behavior
///
/// - **Linux**: uses `sched_setaffinity`.
/// - **macOS**: uses `thread_policy_set` with `THREAD_AFFINITY_POLICY` (hint
///   only; the kernel may still migrate the thread).
/// - **Other**: prints a warning to stderr and returns.
///
/// # Panics
///
/// Panics if the underlying system call fails.
#[cfg(target_os = "linux")]
pub fn pin_to_core(core_id: usize) {
    unsafe {
        let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(core_id, &mut cpuset);
        let ret = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset);
        assert_eq!(ret, 0, "sched_setaffinity failed");
    }
}

/// See [`pin_to_core`] (Linux variant) for documentation.
#[cfg(target_os = "macos")]
pub fn pin_to_core(core_id: usize) {
    unsafe {
        let mut policy_info = libc::thread_affinity_policy_data_t {
            affinity_tag: core_id as i32,
        };
        let ret = libc::thread_policy_set(
            libc::pthread_mach_thread_np(libc::pthread_self()),
            libc::THREAD_AFFINITY_POLICY as u32,
            &mut policy_info as *mut _ as libc::thread_policy_t,
            libc::THREAD_AFFINITY_POLICY_COUNT,
        );
        assert_eq!(ret, 0, "thread_policy_set failed");
    }
}

/// See [`pin_to_core`] (Linux variant) for documentation.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pin_to_core(core_id: usize) {
    eprintln!("pin_to_core({}) not supported on this platform", core_id);
}

// ---------------------------------------------------------------------------
// Channel constructor
// ---------------------------------------------------------------------------

/// Creates a bounded SPSC channel backed by a contiguous arena.
///
/// # Arguments
///
/// * `arena_capacity` - Total size of the arena in bytes. Must be a power of
///   two and at least 2 MiB (to benefit from transparent huge pages on Linux).
///
/// # Returns
///
/// A `(Producer, Consumer)` pair. Each half is `Send` but **not** `Clone` —
/// this is a single-producer, single-consumer channel by design.
///
/// # Panics
///
/// Panics if `arena_capacity` is not a power of two or is less than 2 MiB.
pub fn channel(arena_capacity: usize) -> (Producer, Consumer) {
    assert!(arena_capacity.is_power_of_two());
    assert!(
        arena_capacity >= 2 * 1024 * 1024,
        "arena must be >= 2 MiB for THP"
    );

    let state = Arc::new(SharedState {
        buffer: ArenaBuffer::new(arena_capacity),
        capacity: arena_capacity,
        write_pos: CachePadded(AtomicU64::new(0)),
        read_pos: CachePadded(AtomicU64::new(0)),
    });

    let producer = Producer {
        write_pos: 0,
        read_pos_cache: 0,
        state: Arc::clone(&state),
    };

    let consumer = Consumer { read_pos: 0, state };

    (producer, consumer)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn basic_send_recv() {
        let messages = vec![
            b"first-message".to_vec(),
            b"second-message".to_vec(),
            b"header-payload".to_vec(),
        ];

        let (mut producer, mut consumer) = channel(2 * 1024 * 1024);
        let start = Arc::new(Barrier::new(2));
        let start_consumer = Arc::clone(&start);
        let expected = messages.len();

        let worker = std::thread::spawn(move || {
            let mut received = Vec::with_capacity(expected);
            start_consumer.wait();

            while received.len() < expected {
                consumer.recv_spin(|data| {
                    received.push(data.to_vec());
                });
            }

            received
        });

        start.wait();

        while !producer.send(&messages[0]) {
            spin_pause();
        }
        while !producer.send(&messages[1]) {
            spin_pause();
        }
        while !producer.send_parts(b"header-", b"payload") {
            spin_pause();
        }

        let received = worker.join().unwrap();
        assert_eq!(received, messages);
    }

    #[test]
    fn send_parts_equivalence() {
        let (mut p1, mut c1) = channel(2 * 1024 * 1024);
        let (mut p2, mut c2) = channel(2 * 1024 * 1024);

        let barrier = Arc::new(Barrier::new(3));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let h1 = std::thread::spawn(move || {
            let mut out = Vec::new();
            b1.wait();
            while out.is_empty() {
                c1.recv_spin(|data| out.push(data.to_vec()));
            }
            out
        });

        let h2 = std::thread::spawn(move || {
            let mut out = Vec::new();
            b2.wait();
            while out.is_empty() {
                c2.recv_spin(|data| out.push(data.to_vec()));
            }
            out
        });

        barrier.wait();

        // send as one slice
        let combined: Vec<u8> = b"HEADER".iter().chain(b"BODY").copied().collect();
        while !p1.send(&combined) {
            spin_pause();
        }
        // send as two parts
        while !p2.send_parts(b"HEADER", b"BODY") {
            spin_pause();
        }

        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert_eq!(r1, r2);
    }
}
