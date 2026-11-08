---
created: 2026-10-30
updated: 2026-10-30
type: 修复
---

# 0010 — 修复热重载 watcher 的通道背压死锁

## 动机

热重载 watcher 的事件通道曾是 `mpsc::channel(16)`，notify 回调里用 `blocking_send` 投递。这个组合会在**消费端忙时（例如正处于一次 reload 之中）把 notify 的回调线程挂住**：第 17 个事件起 `blocking_send` 阻塞，而该回调运行在 FSEvents 的 run loop 线程上。

macOS 侧的后果是致命的：notify 的 `FsEventWatcher::stop()` 用

```rust
while !runloop.is_waiting() { thread::yield_now(); }
```

等待 run loop 回到空闲态。回调线程被 `blocking_send` 挂住时 `is_waiting()` 永远为假，于是这个自旋永不终止 —— `drop(watcher)` 卡死，`HotReloadHandle::shutdown()` 卡死，最终 `sag-gateway` 的 `shutdown()` 卡死，只能被 SIGKILL。消费端此刻正在 `drop(watcher)`，不会再取事件，通道永远满，构成真正的死锁而非慢。

在此之前观察到的是**测试偶发超时/挂起**。那只是症状：测试里 watcher 突发写入产生的文件事件偶尔跨过 16 条边界，踩中了同一条死锁路径；偶发性来自事件条数与调度时序，而不是测试本身不稳定。按「测试改写得更宽容」的思路去处理会把一个进程级挂死藏起来，所以本次修的是病根。

## 需求变更

行为的**对外语义**不变（仍只监听配置文件、仍只加载最终版本、仍不丢事件），故 `requirements.md` 无需修改；本次只修复挂死并校正实现描述。（后续文档校正已把 `requirements.md`「监听与防抖」一节的「目录 + 文件双监听」同步为「只监听父目录 + 按文件名过滤」，与本节一致。）以下是能力边界上的两点澄清：

- 热重载 watcher 的事件通道改为**无界通道 + 非阻塞 `send`**：notify 回调只做「组装消息并投递」，绝不阻塞，从而保证 run loop 线程总能返回空闲态、`stop()` 的自旋必然终止。
- notify 回调内**不再有任何日志**：回调和 run loop 同线程，任何额外工作（包括 `tracing` 的字段格式化与 writer 加锁）都可能推迟 run loop 回到等待态；投递失败只意味接收端已消失（watcher 正在关闭），无需上报。
- 文档校正：watcher **只监听父目录**（非递归），过滤发生在收到事件之后。

## 设计要点

**为什么无界是安全的**：① 尾部防抖（`DEBOUNCE`=300ms）已经把突发写入折叠成一次 reload，事件不会按写入次数线性堆积；② 即便真有积压，reload 的语义是「重新读取磁盘上的最终版本」，多读几次同样的内容是幂等的 no-op，丢帧也不失步 —— 队列里留下多少陈旧事件都不影响正确性，只是被消费后各自重置一次防抖窗口。

**风险**：无界队列只在「消费端无限期停滞」时才会无界增长。消费端是同一个 watcher 任务，其停滞上限由一次 reload 的耗时（读盘 + 校验 + 发布，加上有界重试与 settle 窗口）决定，不是无限期；若磁盘长期阻塞导致事件无限堆积，暴露的是存储故障而非 watcher 缺陷。相比「进程挂死不退出」，这是明确的取舍方向。

**为什么不是「有界 + `try_send` 丢弃」**：那会让关键的外部编辑事件在忙时被静默丢弃，进而破坏 `ConfigMutationCoordinator` 的一致性（外部编辑未被重载 → 随后的 UI 变更被误判为「外部已修改」而拒绝）。无界通道保住了「不丢事件」，代价仅是内存。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/loader/reload.rs`](../../../../sag-config/src/loader/reload.rs) | 事件通道 `mpsc::channel(16)` → `unbounded_channel()`，回调由 `blocking_send` 改为非阻塞 `send` 并删掉回调内的 `debug!`（回调只组装 + 投递）；`try_debounced_reload` 的接收端类型随之为 `UnboundedReceiver`；补充说明「回调绝不能阻塞」的注释。 |
| [`sag-state/tests/hot_reload_watcher.rs`](../../../../sag-state/tests/hot_reload_watcher.rs) | 新增死锁回归测试 `watcher_drop_with_event_burst_does_not_hang`。 |
| [`docs/spec/config/spec.md`](../spec.md) | 校正两处「目录 + 文件双监听」的过时描述为「只监听父目录 + 收包后按文件名过滤」。 |

## 测试

- `sag-state/tests/hot_reload_watcher.rs` 新增 `watcher_drop_with_event_burst_does_not_hang`：watcher 按生产路径构造（`spawn_hot_reload` + 一个会停滞 `800ms` 的 `ConfigReloadHandler`），先让消费端进入 reload（此时不再取事件），再向被监听目录写入 **40 个不同路径**的文件形成突发（FSEvents 会合并同一路径的重复变更，故必须用不同路径才能产生 >16 条事件），随后走 `shutdown()` → `drop(watcher)` 路径，并要求 10s 内完成。
  - 非真空性已验证：临时恢复 `mpsc::channel(16)` + `blocking_send` 后该测试在 10.00s 处失败并打印超时断言（不是挂住整个测试二进制），恢复修复后通过。
  - 测试用 `#[test]` 而非 `#[tokio::test]` 并在**独立线程 + 独立 runtime** 中运行 watcher：这样死锁只让该线程卡住，由 `recv_timeout` 判失败，不会拖住整个测试套件。
- 既有 4 个集成测试（外部编辑重载、原子替换突发只加载最终版、未重载外部编辑仍冲突、SAG 自身持久化不误报）行为不变。
