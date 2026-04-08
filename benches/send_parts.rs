// Benchmark: send_parts (two-slice zero-copy) vs concat-then-send
//
// Compares the zero-copy `send_parts(&header, &payload)` path against the
// naive approach of concatenating into a temporary Vec before sending.
//
// Run:
//   cargo bench --bench send_parts

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ARENA_SIZE: usize = 64 * 1024 * 1024; // 64 MiB
const HEADER_SIZE: usize = 16;

#[inline]
fn pin_core(_core_id: usize) {
    #[cfg(target_os = "linux")]
    arena_spsc::pin_to_core(_core_id);
}

/// Simulate typical payload sizes (128-200 bytes).
fn gen_payloads(count: usize) -> Vec<Vec<u8>> {
    let mut seed: u64 = 0xCAFE_BABE;
    (0..count)
        .map(|i| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = 128 + ((seed >> 33) as usize % 73); // 128..200
            let mut buf = vec![0u8; len];
            for (j, byte) in buf.iter_mut().enumerate().take(len) {
                *byte = ((i + j) & 0xFF) as u8;
            }
            buf
        })
        .collect()
}

fn gen_headers(count: usize) -> Vec<[u8; HEADER_SIZE]> {
    (0..count)
        .map(|i| {
            let mut h = [0u8; HEADER_SIZE];
            h[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            h[8..16].copy_from_slice(&(1_000_000u64 + i as u64).to_le_bytes());
            h
        })
        .collect()
}

pub fn spsc_parts_benchmark(c: &mut Criterion) {
    pin_core(0);
    let mut group = c.benchmark_group("spsc_parts");
    group.throughput(Throughput::Elements(1));

    // ---------- send_parts (zero-copy two-slice) ----------
    {
        let processed = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

        let processed_c = Arc::clone(&processed);
        let shutdown_c = Arc::clone(&shutdown);
        let consumer = std::thread::spawn(move || {
            pin_core(1);
            let mut local: u64 = 0;
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

        group.bench_function("send_parts_zero_copy", |b| {
            b.iter_custom(|iters| {
                let n = iters as usize;
                let headers = gen_headers(n);
                let payloads = gen_payloads(n);
                let target = processed.load(Ordering::Acquire) + n as u64;

                let start = Instant::now();
                for i in 0..n {
                    while !producer.send_parts(&headers[i], &payloads[i]) {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                start.elapsed()
            });
        });

        shutdown.store(true, Ordering::Release);
        while !producer.send(&[1]) {
            arena_spsc::spin_pause();
        }
        consumer.join().expect("consumer panicked");
    }

    // ---------- concat-then-send (naive approach) ----------
    {
        let processed = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (mut producer, mut consumer) = arena_spsc::channel(ARENA_SIZE);

        let processed_c = Arc::clone(&processed);
        let shutdown_c = Arc::clone(&shutdown);
        let consumer = std::thread::spawn(move || {
            pin_core(1);
            let mut local: u64 = 0;
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

        group.bench_function("concat_then_send", |b| {
            b.iter_custom(|iters| {
                let n = iters as usize;
                let headers = gen_headers(n);
                let payloads = gen_payloads(n);
                let target = processed.load(Ordering::Acquire) + n as u64;

                let start = Instant::now();
                for i in 0..n {
                    let mut combined = Vec::with_capacity(HEADER_SIZE + payloads[i].len());
                    combined.extend_from_slice(&headers[i]);
                    combined.extend_from_slice(&payloads[i]);
                    while !producer.send(&combined) {
                        arena_spsc::spin_pause();
                    }
                }
                while processed.load(Ordering::Acquire) < target {
                    arena_spsc::spin_pause();
                }
                start.elapsed()
            });
        });

        shutdown.store(true, Ordering::Release);
        while !producer.send(&[1]) {
            arena_spsc::spin_pause();
        }
        consumer.join().expect("consumer panicked");
    }

    group.finish();
}

criterion_group! {
    name = spsc_parts;
    config = Criterion::default()
        .sample_size(200)
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(10));
    targets = spsc_parts_benchmark
}
criterion_main!(spsc_parts);
