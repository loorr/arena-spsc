# arena-spsc

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)]()

A lock-free, single-producer single-consumer (SPSC) byte channel backed by a flat arena buffer. Designed for nanosecond-scale inter-thread communication where every cache miss counts.

## Features

- **Zero-copy reads** — consumer callback borrows directly from the arena; no intermediate allocation or copy
- **Single atomic publish** — one `Release` store on the write position completes a send
- **Cache-line isolation** — `write_pos` and `read_pos` live on separate 64-byte-aligned cache lines to eliminate false sharing
- **Inline variable-length messages** — `[len: u32][data][padding]` layout; no fixed-slot waste for mixed-size workloads
- **Scatter-gather send** — `send_parts(header, payload)` writes two slices into one arena slot with no temp buffer
- **Non-blocking try_recv** — poll-friendly `try_recv()` for multiplexing multiple channels
- **THP + pre-fault** — Linux allocations use `mmap` with `MADV_HUGEPAGE` and touch every page at init to eliminate runtime page-fault jitter
- **Cross-platform prefetch** — `PREFETCHT0` on x86_64, `PRFM PLDL1KEEP` on aarch64
- **CPU pinning** — built-in `pin_to_core()` for Linux (`sched_setaffinity`) and macOS (`thread_policy_set`)

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
arena-spsc = "0.1.0"
```

```rust
use arena_spsc::{channel, spin_pause};
use std::sync::{Arc, Barrier};

let (mut producer, mut consumer) = channel(2 * 1024 * 1024);
let start = Arc::new(Barrier::new(2));
let start_c = Arc::clone(&start);

let worker = std::thread::spawn(move || {
    let mut received = Vec::new();
    start_c.wait();
    while received.len() < 2 {
        consumer.recv_spin(|data| {
            received.push(data.to_vec());
        });
    }
    received
});

start.wait();
while !producer.send(b"hello") { spin_pause(); }
while !producer.send(b"world") { spin_pause(); }

let received = worker.join().unwrap();
assert_eq!(received, vec![b"hello".to_vec(), b"world".to_vec()]);
```

## Release Automation

GitHub Actions now runs `cargo test --all-targets --locked` on pull requests and pushes to `main`.

To publish to crates.io automatically:

1. Add a repository secret named `CARGO_REGISTRY_TOKEN` with your crates.io API token.
2. Bump `version` in `Cargo.toml`.
3. Push a matching Git tag such as `v0.1.0`.

```bash
git tag v0.1.0
git push origin main --follow-tags
```

## Design

### Arena Layout

```
 ┌──────────────────────────── arena (power-of-2, ≥ 2 MiB) ────────────────────────────┐
 │ [len=48][data··pad]  [len=120][data·····pad]  [len=0 wrap]  [len=64][data··pad] ...  │
 │ ◄── 64B aligned ──►  ◄──── 128B aligned ────►              ◄── 64B aligned ──►      │
 └──────────────────────────────────────────────────────────────────────────────────────-┘
         ▲ read_pos                                                    ▲ write_pos
```

- Each slot: `[len: u32][payload][zero-padding to 64B boundary]`
- `len = 0` is the **wrap sentinel** — consumer jumps back to offset 0
- Power-of-2 capacity enables `offset = pos & (capacity - 1)` (no modulo)

### Concurrency Model

| Thread | Writes | Reads |
|--------|--------|-------|
| Producer | `write_pos` (Release) | `read_pos` (Acquire, cached) |
| Consumer | `read_pos` (Release) | `write_pos` (Acquire) |

The producer caches `read_pos` locally and only refreshes it when the arena appears full, amortizing atomic loads on the hot path.

## Benchmarks

All benchmarks were run on the following hardware:

> **CPU:** Intel Xeon Platinum 8375C @ 2.90 GHz (8 cores / 16 threads)
> **RAM:** 123 GB &nbsp;|&nbsp; **OS:** Amazon Linux 2 (kernel 4.14)
> **Rust:** 1.90.0 &nbsp;|&nbsp; **Profile:** `lto = true`, `opt-level = 3`, `codegen-units = 1`

### Throughput

500K messages per iteration, 50 samples, producer and consumer pinned to separate cores.

**small60/60B** — fixed 60-byte messages (arena-spsc sweet spot: `4B header + 60B = 64B`, one cache line, zero padding):

| Implementation | Throughput (Melem/s) | Time / 500K msgs |
|---------------|--------------------:|------------------:|
| **arena-spsc** | **72.5** | **6.90 ms** |
| ringbuf | 29.7 | 16.8 ms |
| rtrb | 27.9 | 17.9 ms |
| crossbeam-arrayqueue | 12.0 | 41.5 ms |
| crossbeam-channel | 10.3 | 48.6 ms |
| disruptor | 7.8 | 64.0 ms |

**small/64B** — fixed 64-byte messages (worst case for arena-spsc: `4B header + 64B = 128B`, wastes 60B padding per message):

| Implementation | Throughput (Melem/s) | Time / 500K msgs |
|---------------|--------------------:|------------------:|
| ringbuf | 72.6 | 6.88 ms |
| rtrb | 67.2 | 7.45 ms |
| **arena-spsc** | **54.8** | **9.12 ms** |
| crossbeam-arrayqueue | 10.4 | 48.3 ms |
| disruptor | 7.8 | 64.1 ms |
| crossbeam-channel | 7.2 | 69.9 ms |

**mixed/128–512B** — variable-length messages (exposes fixed-slot padding waste):

| Implementation | Throughput (Melem/s) | Time / 500K msgs |
|---------------|--------------------:|------------------:|
| **arena-spsc** | **17.2** | **29.1 ms** |
| ringbuf | 11.2 | 44.8 ms |
| rtrb | 10.5 | 47.6 ms |
| crossbeam-channel | 8.4 | 59.4 ms |
| crossbeam-arrayqueue | 7.5 | 66.3 ms |
| disruptor | 5.2 | 95.8 ms |

> **Throughput insight:** Arena-spsc's performance depends heavily on how well message sizes align to 64B cache-line boundaries. At 60B (`4B header + 60B = exactly 64B`), it leads all competitors by **2.4x**. At 64B (`4B + 64B = 128B`, 47% wasted), fixed-slot queues like rtrb/ringbuf have the advantage since their slot size matches the message size exactly. In mixed/variable-length workloads — the real-world scenario — arena-spsc dominates because fixed-slot queues must allocate worst-case slots for every message.

### Latency

Per-message latency sampled every Nth message. Burst mode sends 100M messages at full speed; paced mode rate-limits to 1 msg/μs (1M msg/s).

**Burst mode** (100M messages, sample every 1000th):

| Implementation | avg | p50 | p90 | p99 | p999 | max |
|---------------|----:|----:|----:|----:|-----:|-----:|
| **arena-spsc** | **294** | **289** | **357** | **446** | 1,689 | 79,918 |
| rtrb | 317 | 296 | 430 | 626 | 850 | 459,418 |
| ringbuf | 338 | 312 | 470 | 686 | 933 | 71,415 |
| crossbeam-channel | 452 | 435 | 569 | 796 | 1,318 | 86,077 |
| crossbeam-arrayqueue | 457 | 431 | 563 | 795 | 1,348 | 491,328 |

**Paced mode** (5M messages at 1 msg/μs, sample every 100th):

| Implementation | avg | p50 | p90 | p99 | p999 | max |
|---------------|----:|----:|----:|----:|-----:|-----:|
| rtrb | 290 | 278 | 350 | 500 | 739 | 52,805 |
| **arena-spsc** | **309** | **304** | **344** | **405** | **663** | 69,943 |
| ringbuf | 321 | 300 | 374 | 569 | 4,752 | 75,526 |
| crossbeam-channel | 381 | 369 | 471 | 705 | 1,161 | 38,967 |
| crossbeam-arrayqueue | 405 | 399 | 474 | 633 | 1,185 | 14,067 |

All values in nanoseconds.

> **Key takeaway:** arena-spsc delivers the **tightest p99 tail latency** across both modes — its p99-to-p50 spread is only ~100ns in paced mode, compared to 200ns+ for alternatives. This is the payoff from single-atomic-store publishing and batch-drain consuming.

### send_parts

Compares zero-copy `send_parts(header, payload)` vs allocating a `Vec`, concatenating, then sending.

| Method | Latency | Throughput |
|--------|--------:|-----------:|
| `send_parts` (zero-copy) | **34 ns** | 29.2 Melem/s |
| concat then send | 148 ns | 6.8 Melem/s |

> `send_parts` is **4.3x faster** — the difference is one heap allocation + memcpy per message.

### FAQ: Why do rtrb/ringbuf drop 2.4x going from 64B to 60B messages?

Fixed-slot queues like rtrb and ringbuf store elements in a tightly-packed `[T; N]` array with no alignment padding. Performance depends on whether `sizeof(T)` happens to be a multiple of the cache line size (64 bytes):

```
[u8; 64] — slot = 64B = cache line size (lucky alignment)

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │    element 0       ││    element 1       ││    element 2       │
  │     64 bytes       ││     64 bytes       ││     64 bytes       │
  └────────────────────┘└────────────────────┘└────────────────────┘
  Every element fits in exactly one cache line ✓


[u8; 60] — slot = 60B ≠ multiple of 64 (cache line straddling)

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │  element 0  │elem 1▐▐ element 1│elem 2 ▐▐ element 2│elem 3 ▐
  │   60 bytes  │ 4B   ▐▐  56B     │ 8B    ▐▐  52B     │ 12B   ▐
  └─────────────┴──────┘└──────────┴───────┘└──────────┴───────┘
                ▲                   ▲                    ▲
            straddling!         straddling!           straddling!

  Only 1 in 16 elements is cache-line aligned (LCM(60,64)/60 = 16)
  → 93.75% of elements straddle two cache lines
  → every read/write touches 2 cache lines instead of 1
```

Arena-spsc is immune to this because every message is padded to a 64B boundary **by design**, not by coincidence:

```
arena-spsc — 60B message → align64(4 + 60) = 64B slot

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │ len│ 60B payload    ││ len│ 60B payload    ││ len│ 60B payload    │
  │ 4B │               ││ 4B │               ││ 4B │               │
  └────┴───────────────┘└────┴───────────────┘└────┴───────────────┘
  Always one cache line per message, regardless of payload size ✓
```

The 64B throughput numbers for rtrb/ringbuf are a **coincidence** — they happen to work because `sizeof([u8; 64]) == cache line size`. Arena-spsc's alignment is a **design guarantee** that holds for any message size.

### Run Benchmarks Yourself

```bash
# Throughput (500K messages × 50 samples)
cargo bench --bench throughput

# Per-message latency — burst & paced modes
cargo bench --bench latency

# send_parts vs concat-then-send
cargo bench --bench send_parts
```

## When to Use

This channel is purpose-built for **pinned-core, spin-wait, nanosecond-budget** pipelines:

| Use Case | Why It Fits |
|----------|-------------|
| Trading system hot path (receive → parse → decide → send) | Zero-alloc, tightest p99 tail latency with dedicated cores |
| NIC rx thread → processing thread | Zero-copy arena read; no per-packet `Bytes` allocation |
| Real-time audio/video pipeline | Deterministic latency; no syscalls on the data path |
| High-frequency log/metrics ingestion | Variable-length messages without fixed-slot waste |

### When NOT to Use

| Scenario | Better Alternative |
|----------|-------------------|
| Multiple producers or consumers | `crossbeam-channel`, `flume` |
| Consumer may block or `await` | `tokio::sync::mpsc`, `async-channel` |
| Cross-process communication | Shared-memory ring (e.g., `io_uring`, custom `mmap`) |
| Messages need to outlive the callback | `Bytes` + channel (see below) |
| Message size > arena / 2 | Chunked protocol or streaming |

### arena-spsc vs Bytes + Channel

The key decision factor is **data lifetime after receipt**:

```
Packet arrives from NIC
  ├─ Process synchronously in callback, discard → arena-spsc ✓
  └─ Retain / forward / fan-out / async process  → Bytes + channel ✓
```

**Use arena-spsc** when the consumer completes all work inside the callback:

```rust
consumer.recv_spin(|pkt| {
    let msg = parse(pkt);
    order_book.apply(msg);  // done — pkt is not retained
});
```

**Use Bytes + channel** when data must outlive the receive call:

```rust
let bytes = Bytes::copy_from_slice(&raw_pkt);
tx.send(bytes.clone()).unwrap();  // refcount bump, zero-copy fan-out
```

| Dimension | arena-spsc | Bytes + channel |
|-----------|-----------|-----------------|
| Per-message allocation | Zero | One (or pooled) |
| Data lifetime | Callback scope only | Arbitrary |
| Downstream flexibility | Single-threaded, synchronous | Multi-stage, async, fan-out |
| Back-pressure | Spin on arena full | `await` / `try_send` |

## Platform Support

| Platform | Allocation | Core Pinning | Prefetch |
|----------|-----------|--------------|----------|
| Linux | `mmap` + `MADV_HUGEPAGE` | `sched_setaffinity` (hard) | `PREFETCHT0` |
| macOS | Aligned heap allocation | `thread_policy_set` (hint only) | `PREFETCHT0` |
| Linux/aarch64 | `mmap` + `MADV_HUGEPAGE` | `sched_setaffinity` (hard) | `PRFM PLDL1KEEP` |
| Others | Aligned heap allocation | Warning printed, no-op | no-op |

All platforms pre-fault the arena at creation time to eliminate runtime page faults.

## API

```rust
// Create a channel (arena_capacity must be power-of-2, ≥ 2 MiB)
let (mut producer, mut consumer) = arena_spsc::channel(64 * 1024 * 1024);

// Send a single message (returns false if arena is full)
producer.send(b"payload");

// Send header + payload as one message, zero intermediate allocation
producer.send_parts(b"header", b"payload");

// Non-blocking receive — returns true if at least one message was processed
consumer.try_recv(|data: &[u8]| { /* process data */ });

// Busy-wait receive — spins until data is available, then drains the batch
consumer.recv_spin(|data: &[u8]| { /* process data */ });

// Infinite receive loop
consumer.run(|data: &[u8]| { /* process data */ });

// Pin current thread to a CPU core
arena_spsc::pin_to_core(0);

// Spin-loop hint (PAUSE on x86_64, YIELD on aarch64)
arena_spsc::spin_pause();
```

## License

Licensed under the [MIT License](LICENSE-MIT).
