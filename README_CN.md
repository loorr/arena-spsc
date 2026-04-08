# arena-spsc

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)]()

无锁的单生产者单消费者（SPSC）字节通道，基于扁平 arena 缓冲区。专为纳秒级线程间通信设计，每一次 cache miss 都斤斤计较。

## 特性

- **零拷贝读取** — consumer 回调直接借用 arena 内存，无中间分配或拷贝
- **单次原子发布** — 一次 `Release` store 写入 write position 即完成发送
- **Cache line 隔离** — `write_pos` 与 `read_pos` 分别位于独立的 64 字节对齐 cache line，消除 false sharing
- **内联变长消息** — `[len: u32][data][padding]` 布局，无固定槽位的混合大小消息浪费
- **Scatter-gather 发送** — `send_parts(header, payload)` 将两段切片写入同一个 arena 槽位，无临时缓冲区
- **非阻塞 try_recv** — 支持轮询式 `try_recv()`，方便多通道复用
- **THP + 预缺页** — Linux 下使用 `mmap` + `MADV_HUGEPAGE` 分配，创建时触摸每一页，消除运行时 page fault 抖动
- **跨平台预取** — x86_64 使用 `PREFETCHT0`，aarch64 使用 `PRFM PLDL1KEEP`
- **CPU 绑核** — 内置 `pin_to_core()`，支持 Linux (`sched_setaffinity`) 和 macOS (`thread_policy_set`)

## 快速开始

在 `Cargo.toml` 中添加：

```toml
[dependencies]
arena-spsc = { git = "https://github.com/example/arena-spsc" }
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

## 设计

### Arena 内存布局

```
 ┌──────────────────────────── arena（2 的幂次，≥ 2 MiB）────────────────────────────┐
 │ [len=48][数据··填充]  [len=120][数据·····填充]  [len=0 回绕]  [len=64][数据··填充]  │
 │ ◄── 64B 对齐 ──►      ◄──── 128B 对齐 ────►                ◄── 64B 对齐 ──►      │
 └──────────────────────────────────────────────────────────────────────────────────-─┘
         ▲ read_pos                                                    ▲ write_pos
```

- 每个槽位：`[len: u32][载荷][零填充至 64B 边界]`
- `len = 0` 为**回绕哨兵** — consumer 跳回 offset 0
- 容量为 2 的幂次，使得 `offset = pos & (capacity - 1)`（无需取模）

### 并发模型

| 线程 | 写入 | 读取 |
|------|------|------|
| Producer | `write_pos`（Release） | `read_pos`（Acquire，带本地缓存） |
| Consumer | `read_pos`（Release） | `write_pos`（Acquire） |

Producer 在本地缓存 `read_pos`，仅在 arena 看起来已满时才刷新，摊销热路径上的原子加载开销。

## 基准测试

所有测试均在以下硬件上运行：

> **CPU:** Intel Xeon Platinum 8375C @ 2.90 GHz（8 核 / 16 线程）
> **内存:** 123 GB &nbsp;|&nbsp; **系统:** Amazon Linux 2（内核 4.14）
> **Rust:** 1.90.0 &nbsp;|&nbsp; **编译参数:** `lto = true`, `opt-level = 3`, `codegen-units = 1`

### 吞吐量

每轮 50 万条消息，50 次采样，producer 和 consumer 分别绑定到不同核心。

**small60/60B** — 固定 60 字节消息（arena-spsc 最优点：`4B 头部 + 60B = 64B`，恰好一条 cache line，零填充）：

| 实现 | 吞吐量 (Melem/s) | 耗时 / 50 万条 |
|------|------------------:|---------------:|
| **arena-spsc** | **72.5** | **6.90 ms** |
| ringbuf | 29.7 | 16.8 ms |
| rtrb | 27.9 | 17.9 ms |
| crossbeam-arrayqueue | 12.0 | 41.5 ms |
| crossbeam-channel | 10.3 | 48.6 ms |
| disruptor | 7.8 | 64.0 ms |

**small/64B** — 固定 64 字节消息（arena-spsc 最差点：`4B 头部 + 64B = 128B`，每条消息浪费 60B 填充）：

| 实现 | 吞吐量 (Melem/s) | 耗时 / 50 万条 |
|------|------------------:|---------------:|
| ringbuf | 72.6 | 6.88 ms |
| rtrb | 67.2 | 7.45 ms |
| **arena-spsc** | **54.8** | **9.12 ms** |
| crossbeam-arrayqueue | 10.4 | 48.3 ms |
| disruptor | 7.8 | 64.1 ms |
| crossbeam-channel | 7.2 | 69.9 ms |

**mixed/128–512B** — 变长消息（暴露固定槽位的填充浪费）：

| 实现 | 吞吐量 (Melem/s) | 耗时 / 50 万条 |
|------|------------------:|---------------:|
| **arena-spsc** | **17.2** | **29.1 ms** |
| ringbuf | 11.2 | 44.8 ms |
| rtrb | 10.5 | 47.6 ms |
| crossbeam-channel | 8.4 | 59.4 ms |
| crossbeam-arrayqueue | 7.5 | 66.3 ms |
| disruptor | 5.2 | 95.8 ms |

> **吞吐量解读：** arena-spsc 的性能高度依赖消息大小与 64B cache line 边界的对齐程度。在 60B 时（`4B 头部 + 60B = 恰好 64B`），arena-spsc 以 **2.4 倍**领先所有竞品。在 64B 时（`4B + 64B = 128B`，47% 浪费），固定槽位队列 rtrb/ringbuf 占优，因为其槽位大小恰好等于消息大小。在 mixed 变长消息场景——即真实业务场景——arena-spsc 全面领先，因为固定槽位队列必须为每条消息分配最大槽位。

### 延迟

逐消息延迟采样，每 N 条消息采样一次。Burst 模式全速发送 1 亿条消息；Paced 模式限速为 1 msg/μs（100 万条/秒）。

**Burst 模式**（1 亿条消息，每 1000 条采样一次）：

| 实现 | avg | p50 | p90 | p99 | p999 | max |
|------|----:|----:|----:|----:|-----:|-----:|
| **arena-spsc** | **294** | **289** | **357** | **446** | 1,689 | 79,918 |
| rtrb | 317 | 296 | 430 | 626 | 850 | 459,418 |
| ringbuf | 338 | 312 | 470 | 686 | 933 | 71,415 |
| crossbeam-channel | 452 | 435 | 569 | 796 | 1,318 | 86,077 |
| crossbeam-arrayqueue | 457 | 431 | 563 | 795 | 1,348 | 491,328 |

**Paced 模式**（500 万条消息，1 msg/μs，每 100 条采样一次）：

| 实现 | avg | p50 | p90 | p99 | p999 | max |
|------|----:|----:|----:|----:|-----:|-----:|
| rtrb | 290 | 278 | 350 | 500 | 739 | 52,805 |
| **arena-spsc** | **309** | **304** | **344** | **405** | **663** | 69,943 |
| ringbuf | 321 | 300 | 374 | 569 | 4,752 | 75,526 |
| crossbeam-channel | 381 | 369 | 471 | 705 | 1,161 | 38,967 |
| crossbeam-arrayqueue | 405 | 399 | 474 | 633 | 1,185 | 14,067 |

所有数值单位为纳秒。

> **关键结论：** arena-spsc 在两种模式下均拥有**最紧凑的 p99 尾部延迟** — 在 paced 模式下 p99 与 p50 的差距仅约 100ns，而其他实现在 200ns 以上。这是单次原子 store 发布 + 批量 drain 消费的回报。

### send_parts

对比零拷贝 `send_parts(header, payload)` 与先分配 `Vec` 拼接再发送。

| 方法 | 延迟 | 吞吐量 |
|------|-----:|---------:|
| `send_parts`（零拷贝） | **34 ns** | 29.2 Melem/s |
| 拼接后发送 | 148 ns | 6.8 Melem/s |

> `send_parts` 快 **4.3 倍** — 差距来自每条消息省掉了一次堆分配 + memcpy。

### FAQ：为什么 rtrb/ringbuf 从 64B 到 60B 消息性能暴跌 2.4 倍？

rtrb 和 ringbuf 等固定槽位队列内部使用紧密排列的 `[T; N]` 数组，没有额外对齐填充。性能取决于 `sizeof(T)` 是否恰好是 cache line 大小（64 字节）的倍数：

```
[u8; 64] — 槽位 = 64B = cache line 大小（巧合对齐）

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │    element 0       ││    element 1       ││    element 2       │
  │     64 bytes       ││     64 bytes       ││     64 bytes       │
  └────────────────────┘└────────────────────┘└────────────────────┘
  每个元素恰好占满一条 cache line ✓


[u8; 60] — 槽位 = 60B ≠ 64 的倍数（cache line 跨越）

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │  element 0  │elem 1▐▐ element 1│elem 2 ▐▐ element 2│elem 3 ▐
  │   60 bytes  │ 4B   ▐▐  56B     │ 8B    ▐▐  52B     │ 12B   ▐
  └─────────────┴──────┘└──────────┴───────┘└──────────┴───────┘
                ▲                   ▲                    ▲
            跨越边界！           跨越边界！            跨越边界！

  每 16 个元素才有 1 个对齐（LCM(60,64)/60 = 16）
  → 93.75% 的元素跨越两条 cache line
  → 每次读写触及 2 条 cache line 而非 1 条
```

arena-spsc 完全不受此影响，因为每条消息都**按设计**填充到 64B 边界，而非靠巧合：

```
arena-spsc — 60B 消息 → align64(4 + 60) = 64B 槽位

  cache line 0          cache line 1          cache line 2
  ├───────────────────┤├───────────────────┤├───────────────────┤
  │ len│ 60B 载荷       ││ len│ 60B 载荷       ││ len│ 60B 载荷       │
  │ 4B │               ││ 4B │               ││ 4B │               │
  └────┴───────────────┘└────┴───────────────┘└────┴───────────────┘
  无论消息大小，每条消息始终占据一条 cache line ✓
```

rtrb/ringbuf 在 64B 时的高性能是一个**巧合** — 恰好 `sizeof([u8; 64]) == cache line size`。arena-spsc 的对齐是**设计保证**，对任意消息大小都成立。

### 自行运行基准测试

```bash
# 吞吐量（50 万条消息 × 50 次采样）
cargo bench --bench throughput

# 逐消息延迟 — burst（全速）与 paced（限速）模式
cargo bench --bench latency

# send_parts（零拷贝双切片）vs 先拼接再发送
cargo bench --bench send_parts
```

## 适用场景

本通道专为**绑核、自旋等待、纳秒级预算**的管线设计：

| 场景 | 为什么适合 |
|------|-----------|
| 交易系统热路径（收包 → 解析 → 决策 → 下单） | 零分配，最紧凑的 p99 尾部延迟，绑核后性能稳定 |
| 网卡收包线程 → 处理线程 | 零拷贝 arena 读取，无逐包 `Bytes` 分配 |
| 实时音视频流水线 | 确定性延迟，数据路径无系统调用 |
| 高频日志 / 指标采集 | 变长消息，无固定槽位浪费 |

### 不适用的场景

| 场景 | 更好的替代方案 |
|------|--------------|
| 多生产者或多消费者 | `crossbeam-channel`、`flume` |
| Consumer 可能阻塞或 `await` | `tokio::sync::mpsc`、`async-channel` |
| 跨进程通信 | 共享内存环形缓冲区（如 `io_uring`、自定义 `mmap`） |
| 消息需要在回调之外继续存活 | `Bytes` + channel（见下文） |
| 消息大小 > arena / 2 | 分块协议或流式传输 |

### arena-spsc vs Bytes + Channel

核心决策因素是**消费端拿到数据后的生命周期需求**：

```
网卡收到数据包
  ├─ 在回调内同步处理完、不保留引用 → arena-spsc ✓
  └─ 需要保留 / 转发 / 扇出 / 异步处理 → Bytes + channel ✓
```

**使用 arena-spsc**：consumer 在回调内完成全部工作：

```rust
consumer.recv_spin(|pkt| {
    let msg = parse(pkt);
    order_book.apply(msg);  // 处理完毕 — pkt 不会被保留
});
```

**使用 Bytes + channel**：数据需要在接收调用之后继续存活：

```rust
let bytes = Bytes::copy_from_slice(&raw_pkt);
tx.send(bytes.clone()).unwrap();  // 引用计数 +1，零拷贝扇出
```

| 维度 | arena-spsc | Bytes + channel |
|------|-----------|-----------------|
| 每条消息分配 | 零 | 一次（或池化摊销） |
| 数据生命周期 | 仅回调作用域内 | 任意长 |
| 下游灵活性 | 单线程同步 | 多阶段、async、扇出 |
| 背压机制 | arena 满则自旋 | `await` / `try_send` |

## 平台支持

| 平台 | 内存分配 | CPU 绑核 | 预取指令 |
|------|---------|---------|---------|
| Linux x86_64 | `mmap` + `MADV_HUGEPAGE` | `sched_setaffinity`（硬绑定） | `PREFETCHT0` |
| macOS x86_64 | 对齐堆分配 | `thread_policy_set`（仅为提示） | `PREFETCHT0` |
| Linux aarch64 | `mmap` + `MADV_HUGEPAGE` | `sched_setaffinity`（硬绑定） | `PRFM PLDL1KEEP` |
| 其他 | 对齐堆分配 | 打印警告，无操作 | 无操作 |

所有平台在创建时预触摸 arena 的每一页，消除运行时 page fault。

## API

```rust
// 创建通道（arena_capacity 必须是 2 的幂次，≥ 2 MiB）
let (mut producer, mut consumer) = arena_spsc::channel(64 * 1024 * 1024);

// 发送单条消息（arena 满时返回 false）
producer.send(b"payload");

// 发送 header + payload 合并为一条消息，无中间分配
producer.send_parts(b"header", b"payload");

// 非阻塞接收 — 有数据返回 true，无数据立即返回 false
consumer.try_recv(|data: &[u8]| { /* 处理数据 */ });

// 自旋等待接收 — 阻塞直到有数据，然后批量消费
consumer.recv_spin(|data: &[u8]| { /* 处理数据 */ });

// 无限接收循环
consumer.run(|data: &[u8]| { /* 处理数据 */ });

// 将当前线程绑定到指定 CPU 核心
arena_spsc::pin_to_core(0);

// 自旋循环提示（x86_64 上为 PAUSE，aarch64 上为 YIELD）
arena_spsc::spin_pause();
```

## 许可证

本项目采用 [MIT 许可证](LICENSE-MIT)。
