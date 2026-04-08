// ============================================================
// SPSC Per-Message Latency Benchmark (Sampled)
//
// Two modes:
//   1. **Burst** — producer fires as fast as possible (100M messages).
//      Measures queuing delay under full backpressure.
//   2. **Paced** — producer rate-limited to 1 msg/μs (5M messages).
//      Measures true single-hop transit latency with shallow queue.
//
// Methodology:
//   - A single shared Instant (epoch) is created before both threads start.
//   - Every SAMPLE_INTERVAL-th message: producer records `epoch.elapsed()`
//     as a u64 (nanos) and embeds it as the first 8 bytes of the payload.
//   - Consumer reads the embedded timestamp only on sampled messages and
//     stores raw (send_ts, recv_ts) pairs for deferred computation.
//   - Non-sampled messages incur zero clock reads on either side.
//
// Counter alignment:
//   Producer uses 0-based index `i`, consumer uses 1-based `count` (increments
//   before the check). Producer embeds timestamp when `(i+1) % interval == 0`,
//   so producer i=999 → consumer count=1000 both agree on the same message.
//
// Run:
//   cargo bench --bench latency
// ============================================================

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ringbuf::{traits::Consumer as _, traits::Producer as _, traits::Split as _, HeapRb};
use rtrb::{PopError, PushError, RingBuffer};

const ARENA_SIZE: usize = 64 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 256 * 1024;
const MAX_MSG_SIZE: usize = 512;

// 8 bytes for embedded timestamp + variable payload
const TS_SIZE: usize = 8;

// Only sample every Nth message to minimize clock-read overhead.
// msg_count and warmup_count must be divisible by sample_interval.
const SAMPLE_INTERVAL: usize = 1_000;

struct BenchParams {
    msg_count: usize,
    warmup_count: usize,
    sample_interval: usize,
    /// Nanoseconds between consecutive sends. 0 = full speed (burst).
    send_interval_ns: u64,
}

impl BenchParams {
    fn total(&self) -> usize {
        self.msg_count + self.warmup_count
    }

    fn expected_samples(&self) -> usize {
        self.msg_count / self.sample_interval
    }

    fn label(&self) -> &'static str {
        if self.send_interval_ns == 0 {
            "burst"
        } else {
            "paced"
        }
    }
}

const BURST: BenchParams = BenchParams {
    msg_count: 100_000_000,
    warmup_count: 1_000_000,
    sample_interval: SAMPLE_INTERVAL,
    send_interval_ns: 0,
};

const PACED: BenchParams = BenchParams {
    msg_count: 5_000_000,
    warmup_count: 500_000,
    sample_interval: 100,    // sample every 100 msgs → 50K samples
    send_interval_ns: 1_000, // 1 μs → 1M msg/s
};

struct PayloadRng {
    seed: u64,
    min: usize,
    range: usize,
}

impl PayloadRng {
    fn new(min: usize, max: usize) -> Self {
        Self {
            seed: 0xDEAD_BEEF,
            min,
            range: max - min + 1,
        }
    }

    #[inline(always)]
    fn next(&mut self) -> usize {
        self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.min + ((self.seed >> 33) as usize % self.range)
    }
}

#[inline]
fn pin_core(_core_id: usize) {
    #[cfg(target_os = "linux")]
    arena_spsc::pin_to_core(_core_id);
}

/// Spin-wait until `epoch.elapsed()` reaches `target_ns`.
#[inline(always)]
fn spin_until(epoch: Instant, target_ns: u64) {
    while (epoch.elapsed().as_nanos() as u64) < target_ns {
        std::hint::spin_loop();
    }
}

/// Check if producer message at 0-based index `i` should carry a timestamp.
/// Consumer uses 1-based `count` (increments before check), so producer must
/// embed at `i` where `(i + 1) % interval == 0` to align with consumer's
/// `count.is_multiple_of(interval)`.
#[inline(always)]
fn is_sampled(i: usize, interval: usize) -> bool {
    (i + 1).is_multiple_of(interval)
}

fn main() {
    pin_core(0);

    for params in [&BURST, &PACED] {
        println!(
            "=== {} mode: {}M msgs, {}K warmup, sample 1/{}, {} samples, interval {}ns ===",
            params.label(),
            params.msg_count / 1_000_000,
            params.warmup_count / 1_000,
            params.sample_interval,
            params.expected_samples(),
            params.send_interval_ns,
        );
        println!();

        bench_arena_spsc(params);
        bench_crossbeam_channel(params);
        bench_crossbeam_arrayqueue(params);
        bench_rtrb(params);
        bench_ringbuf(params);

        println!();
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_stats(name: &str, latencies: &mut [u64]) {
    latencies.sort_unstable();
    let len = latencies.len();
    let sum: u64 = latencies.iter().sum();
    let avg = sum / len as u64;

    println!("  {name}:");
    println!("    samples: {len}");
    println!("    avg:  {:>8} ns", avg);
    println!("    p50:  {:>8} ns", percentile(latencies, 50.0));
    println!("    p90:  {:>8} ns", percentile(latencies, 90.0));
    println!("    p99:  {:>8} ns", percentile(latencies, 99.0));
    println!("    p999: {:>8} ns", percentile(latencies, 99.9));
    println!("    max:  {:>8} ns", latencies[len - 1]);
    println!();
}

fn compute_and_print_stats(name: &str, raw_samples: &[(u64, u64)]) {
    let mut latencies: Vec<u64> = raw_samples
        .iter()
        .map(|&(send_ts, recv_ts)| recv_ts.saturating_sub(send_ts))
        .collect();
    print_stats(name, &mut latencies);
}

// ────────────────────────────────────────────────────────────
// arena-spsc
// ────────────────────────────────────────────────────────────

fn bench_arena_spsc(p: &BenchParams) {
    let total = p.total();
    let epoch = Instant::now();
    let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

    let epoch_c = epoch;
    let warmup = p.warmup_count;
    let interval = p.sample_interval;
    let expected = p.expected_samples();
    let handle = std::thread::spawn(move || {
        pin_core(1);
        let mut raw_samples: Vec<(u64, u64)> = Vec::with_capacity(expected);
        let mut count: usize = 0;

        while count < total {
            consumer.recv_spin(|data| {
                count += 1;
                if count.is_multiple_of(interval) {
                    let recv_ts = epoch_c.elapsed().as_nanos() as u64;
                    let send_ts = u64::from_le_bytes(data[..TS_SIZE].try_into().unwrap());
                    if count > warmup {
                        raw_samples.push((send_ts, recv_ts));
                    }
                }
            });
        }
        raw_samples
    });

    let mut rng = PayloadRng::new(128, 256);
    let mut buf = vec![0u8; MAX_MSG_SIZE + TS_SIZE];
    for i in 0..total {
        if p.send_interval_ns > 0 {
            spin_until(epoch, i as u64 * p.send_interval_ns);
        }

        let plen = rng.next();
        let msg_len = TS_SIZE + plen;

        for (j, byte) in buf.iter_mut().enumerate().take(msg_len).skip(TS_SIZE) {
            *byte = ((i + j) & 0xFF) as u8;
        }

        if is_sampled(i, p.sample_interval) {
            let send_ts = epoch.elapsed().as_nanos() as u64;
            buf[..TS_SIZE].copy_from_slice(&send_ts.to_le_bytes());
        }

        while !producer.send(&buf[..msg_len]) {
            arena_spsc::spin_pause();
        }
    }

    let raw_samples = handle.join().unwrap();
    compute_and_print_stats("arena-spsc", &raw_samples);
}

// ────────────────────────────────────────────────────────────
// crossbeam bounded channel
// ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Msg {
    len: u16,
    data: [u8; MAX_MSG_SIZE + TS_SIZE],
}

impl Default for Msg {
    fn default() -> Self {
        Self {
            len: 0,
            data: [0u8; MAX_MSG_SIZE + TS_SIZE],
        }
    }
}

fn bench_crossbeam_channel(p: &BenchParams) {
    let total = p.total();
    let epoch = Instant::now();
    let (tx, rx) = crossbeam_channel::bounded::<Msg>(QUEUE_CAPACITY);

    let epoch_c = epoch;
    let warmup = p.warmup_count;
    let interval = p.sample_interval;
    let expected = p.expected_samples();
    let handle = std::thread::spawn(move || {
        pin_core(1);
        let mut raw_samples: Vec<(u64, u64)> = Vec::with_capacity(expected);
        let mut count: usize = 0;

        while count < total {
            match rx.try_recv() {
                Ok(msg) => {
                    count += 1;
                    if count.is_multiple_of(interval) {
                        let recv_ts = epoch_c.elapsed().as_nanos() as u64;
                        let send_ts = u64::from_le_bytes(msg.data[..TS_SIZE].try_into().unwrap());
                        if count > warmup {
                            raw_samples.push((send_ts, recv_ts));
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    arena_spsc::spin_pause();
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
        raw_samples
    });

    let mut rng = PayloadRng::new(128, 256);
    for i in 0..total {
        if p.send_interval_ns > 0 {
            spin_until(epoch, i as u64 * p.send_interval_ns);
        }

        let plen = rng.next();
        let msg_len = TS_SIZE + plen;
        let mut msg = Msg {
            len: msg_len as u16,
            ..Default::default()
        };
        for j in TS_SIZE..msg_len {
            msg.data[j] = ((i + j) & 0xFF) as u8;
        }
        if is_sampled(i, p.sample_interval) {
            let send_ts = epoch.elapsed().as_nanos() as u64;
            msg.data[..TS_SIZE].copy_from_slice(&send_ts.to_le_bytes());
        }

        while tx.try_send(msg).is_err() {
            arena_spsc::spin_pause();
        }
    }

    drop(tx);
    let raw_samples = handle.join().unwrap();
    compute_and_print_stats("crossbeam-channel", &raw_samples);
}

// ────────────────────────────────────────────────────────────
// crossbeam ArrayQueue
// ────────────────────────────────────────────────────────────

fn bench_crossbeam_arrayqueue(p: &BenchParams) {
    let total = p.total();
    let epoch = Instant::now();
    let shutdown = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(crossbeam_queue::ArrayQueue::<Msg>::new(QUEUE_CAPACITY));

    let epoch_c = epoch;
    let warmup = p.warmup_count;
    let interval = p.sample_interval;
    let expected = p.expected_samples();
    let shutdown_c = Arc::clone(&shutdown);
    let queue_c = Arc::clone(&queue);
    let handle = std::thread::spawn(move || {
        pin_core(1);
        let mut raw_samples: Vec<(u64, u64)> = Vec::with_capacity(expected);
        let mut count: usize = 0;

        while count < total {
            match queue_c.pop() {
                Some(msg) => {
                    count += 1;
                    if count.is_multiple_of(interval) {
                        let recv_ts = epoch_c.elapsed().as_nanos() as u64;
                        let send_ts = u64::from_le_bytes(msg.data[..TS_SIZE].try_into().unwrap());
                        if count > warmup {
                            raw_samples.push((send_ts, recv_ts));
                        }
                    }
                }
                None => {
                    if shutdown_c.load(Ordering::Relaxed) {
                        break;
                    }
                    arena_spsc::spin_pause();
                }
            }
        }
        raw_samples
    });

    let mut rng = PayloadRng::new(128, 256);
    for i in 0..total {
        if p.send_interval_ns > 0 {
            spin_until(epoch, i as u64 * p.send_interval_ns);
        }

        let plen = rng.next();
        let msg_len = TS_SIZE + plen;
        let mut msg = Msg {
            len: msg_len as u16,
            ..Default::default()
        };
        for j in TS_SIZE..msg_len {
            msg.data[j] = ((i + j) & 0xFF) as u8;
        }
        if is_sampled(i, p.sample_interval) {
            let send_ts = epoch.elapsed().as_nanos() as u64;
            msg.data[..TS_SIZE].copy_from_slice(&send_ts.to_le_bytes());
        }

        loop {
            match queue.push(msg) {
                Ok(()) => break,
                Err(_) => arena_spsc::spin_pause(),
            }
        }
    }

    shutdown.store(true, Ordering::Release);
    let raw_samples = handle.join().unwrap();
    compute_and_print_stats("crossbeam-arrayqueue", &raw_samples);
}

// ────────────────────────────────────────────────────────────
// rtrb
// ────────────────────────────────────────────────────────────

fn bench_rtrb(p: &BenchParams) {
    let total = p.total();
    let epoch = Instant::now();
    let (mut producer, mut consumer) = RingBuffer::<Msg>::new(QUEUE_CAPACITY);

    let epoch_c = epoch;
    let warmup = p.warmup_count;
    let interval = p.sample_interval;
    let expected = p.expected_samples();
    let handle = std::thread::spawn(move || {
        pin_core(1);
        let mut raw_samples: Vec<(u64, u64)> = Vec::with_capacity(expected);
        let mut count: usize = 0;

        while count < total {
            match consumer.pop() {
                Ok(msg) => {
                    count += 1;
                    if count.is_multiple_of(interval) {
                        let recv_ts = epoch_c.elapsed().as_nanos() as u64;
                        let send_ts = u64::from_le_bytes(msg.data[..TS_SIZE].try_into().unwrap());
                        if count > warmup {
                            raw_samples.push((send_ts, recv_ts));
                        }
                    }
                }
                Err(PopError::Empty) => arena_spsc::spin_pause(),
            }
        }
        raw_samples
    });

    let mut rng = PayloadRng::new(128, 256);
    for i in 0..total {
        if p.send_interval_ns > 0 {
            spin_until(epoch, i as u64 * p.send_interval_ns);
        }

        let plen = rng.next();
        let msg_len = TS_SIZE + plen;
        let mut msg = Msg {
            len: msg_len as u16,
            ..Default::default()
        };
        for j in TS_SIZE..msg_len {
            msg.data[j] = ((i + j) & 0xFF) as u8;
        }
        if is_sampled(i, p.sample_interval) {
            let send_ts = epoch.elapsed().as_nanos() as u64;
            msg.data[..TS_SIZE].copy_from_slice(&send_ts.to_le_bytes());
        }

        loop {
            match producer.push(msg) {
                Ok(()) => break,
                Err(PushError::Full(_)) => arena_spsc::spin_pause(),
            }
        }
    }

    let raw_samples = handle.join().unwrap();
    compute_and_print_stats("rtrb", &raw_samples);
}

// ────────────────────────────────────────────────────────────
// ringbuf
// ────────────────────────────────────────────────────────────

fn bench_ringbuf(p: &BenchParams) {
    let total = p.total();
    let epoch = Instant::now();
    let rb = HeapRb::<Msg>::new(QUEUE_CAPACITY);
    let (mut producer, mut consumer) = rb.split();

    let epoch_c = epoch;
    let warmup = p.warmup_count;
    let interval = p.sample_interval;
    let expected = p.expected_samples();
    let handle = std::thread::spawn(move || {
        pin_core(1);
        let mut raw_samples: Vec<(u64, u64)> = Vec::with_capacity(expected);
        let mut count: usize = 0;

        while count < total {
            match consumer.try_pop() {
                Some(msg) => {
                    count += 1;
                    if count.is_multiple_of(interval) {
                        let recv_ts = epoch_c.elapsed().as_nanos() as u64;
                        let send_ts = u64::from_le_bytes(msg.data[..TS_SIZE].try_into().unwrap());
                        if count > warmup {
                            raw_samples.push((send_ts, recv_ts));
                        }
                    }
                }
                None => arena_spsc::spin_pause(),
            }
        }
        raw_samples
    });

    let mut rng = PayloadRng::new(128, 256);
    for i in 0..total {
        if p.send_interval_ns > 0 {
            spin_until(epoch, i as u64 * p.send_interval_ns);
        }

        let plen = rng.next();
        let msg_len = TS_SIZE + plen;
        let mut msg = Msg {
            len: msg_len as u16,
            ..Default::default()
        };
        for j in TS_SIZE..msg_len {
            msg.data[j] = ((i + j) & 0xFF) as u8;
        }
        if is_sampled(i, p.sample_interval) {
            let send_ts = epoch.elapsed().as_nanos() as u64;
            msg.data[..TS_SIZE].copy_from_slice(&send_ts.to_le_bytes());
        }

        loop {
            if producer.try_push(msg).is_ok() {
                break;
            }
            arena_spsc::spin_pause();
        }
    }

    let raw_samples = handle.join().unwrap();
    compute_and_print_stats("ringbuf", &raw_samples);
}
