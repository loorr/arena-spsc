// ============================================================
// SPSC Throughput Benchmark (criterion)
//
// We split the benchmark into two regimes:
//   - small/64B: fixed-size messages to compare pure queue overhead
//   - mixed/128-512B: variable-length messages to expose arena-spsc's
//     storage-efficiency advantage over fixed-slot rings
//
// All producer / consumer pairs are pinned to separate cores and all
// implementations use the same "processed counter" completion signal so
// the benchmark does not accidentally measure per-message atomics.
//
// NOTE: On macOS, pin_to_core is only a hint — the kernel may still
// migrate threads, which can inflate variance.  Linux results with
// actual core affinity are the authoritative numbers.
// ============================================================

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use disruptor::Producer;
use ringbuf::{traits::Consumer as _, traits::Producer as _, traits::Split as _, HeapRb};
use rtrb::{PushError, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_MSG_SIZE: usize = 512;
const SMALL_MSG_SIZE: usize = 64;
const SMALL60_MSG_SIZE: usize = 60;
const QUEUE_CAPACITY: usize = 256 * 1024;
const ARENA_SIZE: usize = 64 * 1024 * 1024;
const BATCH_SIZE: usize = 500_000;

#[inline]
fn pin_core(_core_id: usize) {
    #[cfg(target_os = "linux")]
    arena_spsc::pin_to_core(_core_id);
}

type SmallMsg = [u8; SMALL_MSG_SIZE];
type Small60Msg = [u8; SMALL60_MSG_SIZE];

#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Msg {
    len: u16,
    data: [u8; MAX_MSG_SIZE],
}

impl Default for Msg {
    fn default() -> Self {
        Self {
            len: 0,
            data: [0u8; MAX_MSG_SIZE],
        }
    }
}

struct DisruptorMsg<const N: usize> {
    len: u16,
    data: [u8; N],
}

fn gen_messages(count: usize, min_len: usize, max_len: usize) -> Vec<Vec<u8>> {
    let range = max_len - min_len + 1;
    let mut seed: u64 = 0xDEAD_BEEF;
    (0..count)
        .map(|i| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = min_len + ((seed >> 33) as usize % range);
            let mut buf = vec![0u8; len];
            for (j, byte) in buf.iter_mut().enumerate() {
                *byte = ((i + j) & 0xFF) as u8;
            }
            buf
        })
        .collect()
}

fn gen_small_messages(count: usize) -> Vec<SmallMsg> {
    (0..count)
        .map(|i| {
            let mut buf = [0u8; SMALL_MSG_SIZE];
            for (j, byte) in buf.iter_mut().enumerate() {
                *byte = ((i + j) & 0xFF) as u8;
            }
            buf
        })
        .collect()
}

fn gen_small60_messages(count: usize) -> Vec<Small60Msg> {
    (0..count)
        .map(|i| {
            let mut buf = [0u8; SMALL60_MSG_SIZE];
            for (j, byte) in buf.iter_mut().enumerate() {
                *byte = ((i + j) & 0xFF) as u8;
            }
            buf
        })
        .collect()
}

/// Pre-construct Msg structs from variable-length payloads so that the
/// Msg construction cost is excluded from the timed region.  This keeps
/// the measurement fair: arena-spsc sends raw &[u8] slices (no Msg),
/// while fixed-slot queues must copy into a Msg, but we want to measure
/// pure queue throughput, not serialization overhead.
fn prepack_messages(raw: &[Vec<u8>]) -> Vec<Msg> {
    raw.iter()
        .map(|data| {
            let mut msg = Msg {
                len: data.len() as u16,
                ..Default::default()
            };
            msg.data[..data.len()].copy_from_slice(data);
            msg
        })
        .collect()
}

fn pin_producer() {
    pin_core(0);
}

pub fn throughput_benchmark(c: &mut Criterion) {
    pin_producer();

    {
        let mut group = c.benchmark_group("small");
        group.throughput(Throughput::Elements(BATCH_SIZE as u64));
        group.sample_size(50);
        group.warm_up_time(Duration::from_secs(2));
        group.measurement_time(Duration::from_secs(10));

        let msgs = gen_small_messages(BATCH_SIZE);
        bench_arena_small(&mut group, "arena_spsc", &msgs);
        bench_crossbeam_channel_small(&mut group, "crossbeam_ch", &msgs);
        bench_crossbeam_arrayqueue_small(&mut group, "crossbeam_aq", &msgs);
        bench_rtrb_small(&mut group, "rtrb", &msgs);
        bench_ringbuf_small(&mut group, "ringbuf", &msgs);
        bench_disruptor_small(&mut group, "disruptor", &msgs);
        group.finish();
    }

    {
        let mut group = c.benchmark_group("small60");
        group.throughput(Throughput::Elements(BATCH_SIZE as u64));
        group.sample_size(50);
        group.warm_up_time(Duration::from_secs(2));
        group.measurement_time(Duration::from_secs(10));

        let msgs = gen_small60_messages(BATCH_SIZE);
        bench_arena_small60(&mut group, "arena_spsc", &msgs);
        bench_crossbeam_channel_small60(&mut group, "crossbeam_ch", &msgs);
        bench_crossbeam_arrayqueue_small60(&mut group, "crossbeam_aq", &msgs);
        bench_rtrb_small60(&mut group, "rtrb", &msgs);
        bench_ringbuf_small60(&mut group, "ringbuf", &msgs);
        bench_disruptor_small60(&mut group, "disruptor", &msgs);
        group.finish();
    }

    {
        let mut group = c.benchmark_group("mixed");
        group.throughput(Throughput::Elements(BATCH_SIZE as u64));
        group.sample_size(50);
        group.warm_up_time(Duration::from_secs(2));
        group.measurement_time(Duration::from_secs(10));

        let raw_msgs = gen_messages(BATCH_SIZE, 128, 512);
        let packed_msgs = prepack_messages(&raw_msgs);
        bench_arena_mixed(&mut group, "arena_spsc", &raw_msgs);
        bench_crossbeam_channel_mixed(&mut group, "crossbeam_ch", &packed_msgs);
        bench_crossbeam_arrayqueue_mixed(&mut group, "crossbeam_aq", &packed_msgs);
        bench_rtrb_mixed(&mut group, "rtrb", &packed_msgs);
        bench_ringbuf_mixed(&mut group, "ringbuf", &packed_msgs);
        bench_disruptor_mixed(&mut group, "disruptor", &raw_msgs);
        group.finish();
    }
}

// ────────────────────────────────────────────────────────────
// arena-spsc
// ────────────────────────────────────────────────────────────

fn bench_arena_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            consumer.recv_spin(|_data| {
                local += 1;
            });
            processed_c.store(local, Ordering::Release);
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while !producer.send(msg) {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    while !producer.send(&[1]) {
        arena_spsc::spin_pause();
    }
    consumer.join().expect("consumer panicked");
}

fn bench_arena_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Vec<u8>],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            consumer.recv_spin(|_data| {
                local += 1;
            });
            processed_c.store(local, Ordering::Release);
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while !producer.send(msg) {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    while !producer.send(&[1]) {
        arena_spsc::spin_pause();
    }
    consumer.join().expect("consumer panicked");
}

// ────────────────────────────────────────────────────────────
// crossbeam bounded channel
// ────────────────────────────────────────────────────────────

fn bench_crossbeam_channel_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded::<SmallMsg>(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while rx.try_recv().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while tx.try_send(*msg).is_err() {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    // Send a final message to wake the consumer out of spin_pause.
    let _ = tx.try_send([0u8; SMALL_MSG_SIZE]);
    consumer.join().expect("consumer panicked");
}

fn bench_crossbeam_channel_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded::<Msg>(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while rx.try_recv().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while tx.try_send(*msg).is_err() {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    let _ = tx.try_send(Msg::default());
    consumer.join().expect("consumer panicked");
}

// ────────────────────────────────────────────────────────────
// crossbeam ArrayQueue
// ────────────────────────────────────────────────────────────

fn bench_crossbeam_arrayqueue_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(crossbeam_queue::ArrayQueue::<SmallMsg>::new(QUEUE_CAPACITY));

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let queue_c = Arc::clone(&queue);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while queue_c.pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match queue.push(*msg) {
                            Ok(()) => break,
                            Err(_) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer.join().expect("consumer panicked");
}

fn bench_crossbeam_arrayqueue_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(crossbeam_queue::ArrayQueue::<Msg>::new(QUEUE_CAPACITY));

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let queue_c = Arc::clone(&queue);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while queue_c.pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match queue.push(*msg) {
                            Ok(()) => break,
                            Err(_) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer.join().expect("consumer panicked");
}

// ────────────────────────────────────────────────────────────
// rtrb
// ────────────────────────────────────────────────────────────

fn bench_rtrb_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = RingBuffer::<SmallMsg>::new(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.pop().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match producer.push(*msg) {
                            Ok(()) => break,
                            Err(PushError::Full(_)) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

fn bench_rtrb_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = RingBuffer::<Msg>::new(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.pop().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match producer.push(*msg) {
                            Ok(()) => break,
                            Err(PushError::Full(_)) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

// ────────────────────────────────────────────────────────────
// ringbuf
// ────────────────────────────────────────────────────────────

fn bench_ringbuf_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let rb = HeapRb::<SmallMsg>::new(QUEUE_CAPACITY);
    let (mut producer, mut consumer) = rb.split();

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.try_pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        if producer.try_push(*msg).is_ok() {
                            break;
                        }
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

fn bench_ringbuf_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let rb = HeapRb::<Msg>::new(QUEUE_CAPACITY);
    let (mut producer, mut consumer) = rb.split();

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.try_pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        if producer.try_push(*msg).is_ok() {
                            break;
                        }
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

// ────────────────────────────────────────────────────────────
// disruptor
// ────────────────────────────────────────────────────────────

fn bench_disruptor_small(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[SmallMsg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let factory = || DisruptorMsg::<SMALL_MSG_SIZE> {
        len: SMALL_MSG_SIZE as u16,
        data: [0u8; SMALL_MSG_SIZE],
    };

    let processed_c = Arc::clone(&processed);
    let mut local = 0u64;
    let processor =
        move |_event: &DisruptorMsg<SMALL_MSG_SIZE>, _sequence: i64, end_of_batch: bool| {
            local += 1;
            if end_of_batch {
                processed_c.store(local, Ordering::Release);
            }
        };

    let mut producer =
        disruptor::build_single_producer(QUEUE_CAPACITY, factory, disruptor::BusySpin)
            .handle_events_with(processor)
            .build();

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    producer.publish(|event| {
                        event.len = SMALL_MSG_SIZE as u16;
                        event.data.copy_from_slice(msg);
                    });
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_disruptor_mixed(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Vec<u8>],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let factory = || DisruptorMsg::<MAX_MSG_SIZE> {
        len: 0,
        data: [0u8; MAX_MSG_SIZE],
    };

    let processed_c = Arc::clone(&processed);
    let mut local = 0u64;
    let processor =
        move |_event: &DisruptorMsg<MAX_MSG_SIZE>, _sequence: i64, end_of_batch: bool| {
            local += 1;
            if end_of_batch {
                processed_c.store(local, Ordering::Release);
            }
        };

    let mut producer =
        disruptor::build_single_producer(QUEUE_CAPACITY, factory, disruptor::BusySpin)
            .handle_events_with(processor)
            .build();

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg_data in msgs {
                    let len = msg_data.len();
                    producer.publish(|event| {
                        event.len = len as u16;
                        event.data[..len].copy_from_slice(msg_data);
                    });
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });
}

// ────────────────────────────────────────────────────────────
// small60 (60B) benchmarks
// ────────────────────────────────────────────────────────────

fn bench_arena_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            consumer.recv_spin(|_data| {
                local += 1;
            });
            processed_c.store(local, Ordering::Release);
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while !producer.send(msg) {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    while !producer.send(&[1]) {
        arena_spsc::spin_pause();
    }
    consumer.join().expect("consumer panicked");
}

fn bench_crossbeam_channel_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded::<Small60Msg>(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while rx.try_recv().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    while tx.try_send(*msg).is_err() {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    let _ = tx.try_send([0u8; SMALL60_MSG_SIZE]);
    consumer.join().expect("consumer panicked");
}

fn bench_crossbeam_arrayqueue_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(crossbeam_queue::ArrayQueue::<Small60Msg>::new(
        QUEUE_CAPACITY,
    ));

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let queue_c = Arc::clone(&queue);
    let consumer = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while queue_c.pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match queue.push(*msg) {
                            Ok(()) => break,
                            Err(_) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer.join().expect("consumer panicked");
}

fn bench_rtrb_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = RingBuffer::<Small60Msg>::new(QUEUE_CAPACITY);

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.pop().is_ok() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        match producer.push(*msg) {
                            Ok(()) => break,
                            Err(PushError::Full(_)) => arena_spsc::spin_pause(),
                        }
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

fn bench_ringbuf_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let rb = HeapRb::<Small60Msg>::new(QUEUE_CAPACITY);
    let (mut producer, mut consumer) = rb.split();

    let processed_c = Arc::clone(&processed);
    let shutdown_c = Arc::clone(&shutdown);
    let consumer_thread = std::thread::spawn(move || {
        pin_core(1);
        let mut local = 0u64;
        loop {
            let mut progressed = false;
            while consumer.try_pop().is_some() {
                local += 1;
                progressed = true;
            }
            if progressed {
                processed_c.store(local, Ordering::Release);
                continue;
            }
            if shutdown_c.load(Ordering::Acquire) {
                break;
            }
            arena_spsc::spin_pause();
        }
    });

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    loop {
                        if producer.try_push(*msg).is_ok() {
                            break;
                        }
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });

    shutdown.store(true, Ordering::Release);
    consumer_thread.join().expect("consumer panicked");
}

fn bench_disruptor_small60(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    name: &str,
    msgs: &[Small60Msg],
) {
    let processed = Arc::new(AtomicU64::new(0));
    let factory = || DisruptorMsg::<SMALL60_MSG_SIZE> {
        len: SMALL60_MSG_SIZE as u16,
        data: [0u8; SMALL60_MSG_SIZE],
    };

    let processed_c = Arc::clone(&processed);
    let mut local = 0u64;
    let processor =
        move |_event: &DisruptorMsg<SMALL60_MSG_SIZE>, _sequence: i64, end_of_batch: bool| {
            local += 1;
            if end_of_batch {
                processed_c.store(local, Ordering::Release);
            }
        };

    let mut producer =
        disruptor::build_single_producer(QUEUE_CAPACITY, factory, disruptor::BusySpin)
            .handle_events_with(processor)
            .build();

    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let target = processed.load(Ordering::Acquire) + msgs.len() as u64;
                let start = Instant::now();
                for msg in msgs {
                    producer.publish(|event| {
                        event.len = SMALL60_MSG_SIZE as u16;
                        event.data.copy_from_slice(msg);
                    });
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                total += start.elapsed();
            }
            total
        });
    });
}

criterion_group! {
    name = throughput;
    config = Criterion::default();
    targets = throughput_benchmark
}
criterion_main!(throughput);
