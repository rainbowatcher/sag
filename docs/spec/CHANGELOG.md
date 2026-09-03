---
created: 2026-06-03
updated: 2026-09-03
---

# 变更时间线

按时间倒序列出所有模块的变更记录。

## 2026-09-03

- **配置**：[0003 修复 macOS 文件句柄耗尽与回退白名单遗漏](config/changes/0003-use-native-fsevent-and-backoff-whitelist.md) — 移除 `macos_kqueue` 切换回原生 FSEvents，杜绝全项目 WalkDir 导致的句柄爆炸，并将 `retry_on_empty_response` 纳入配置白名单消除虚警
- **配置**：[0002 热重载 watcher 加固：尾部防抖、目录监听与瞬时失败重试](config/changes/0002-hot-reload-trailing-debounce.md) — 热重载改为尾部防抖（只加载突发写入的最终版本），同时监听父目录以在原子 temp-file + rename 后不失效，并增加 settle 边界排空与瞬时失败有界重试

## 2026-08-26

- **调度**：[0006 流式请求超时边界修复](dispatch/changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 reqwest client 总 timeout 截断，非流式、TTFB 和 chunk idle timeout 分阶段生效

## 2026-08-25

- **供应商**：[0003 供应商模型下拉自动拉取与缺失标记](providers/changes/0003-combobox-auto-fetch-and-missing-indicator.md) — Combobox 点击展开自动请求 models 接口并在下拉内展示 loading；比对已启用模型并在已选列表和下拉项中标记上游不存在的模型
- **路由**：[0002 目标模型有效性校验与 Provider 变更联动感知](routing/changes/0002-target-model-validity-and-provider-sync.md) — 运行时 `filter_enabled_targets` 与 `/v1/models` 增加 `enabled_models` 白名单过滤；前端 Provider 变更联动清理模型缓存，Route 列表与 Target 编辑器增强失效目标诊断与高亮提示

## 2026-08-18

- **熔断器**：[0006 熔断器逻辑概念内聚与探测槽生命周期治理（RAII）](circuit-breaker/changes/0006-concept-consolidation-and-probe-guard-lifecycle.md) — `CircuitPermit`/`ProbeGuard` RAII 守卫收敛探测槽归还（`is_probe` 布尔与手工释放分支消失），`force_close_circuit` 下沉 `sag-state`，移除 shim 并统一 `DispatchRuntime.circuit_breaker` 命名；行为零回归

## 2026-08-17

- **调度**：[0005 聊天分发单文件复杂度解耦与模块化](dispatch/changes/0005-chat-dispatch-modularization.md) — 将 3900+ 行 `chat.rs` 解耦拆分为 `chat/` 模块目录（`mod.rs` 主调度、`attempt.rs` 事实构造、`empty.rs` 空响应校验、`tests.rs` 集成测试）
- **分析**：[0003 UUID v4/v7 版本分层决策](analytics/changes/0003-uuid-version-layering.md) — 记录 request_id(v4) 与身份 ID(v7) 的版本分工理据，明确共存是有意分层非技术债

## 2026-08-11

- **分析**：[0002 双层事实与 Rollup](analytics/changes/0002-two-level-facts.md) — 缓存命中率 Rollup 可合并计数（known/hit/prompt_known）、actual_model 语义、failure_reason 映射表、target 三值枚举（+cancelled）、flowchart 补 emit 边、FK RESTRICT 等设计修订

## 2026-08-04

- **Failover 4xx 策略**：[0001 4xx Failover 归因策略](failover-4xx-policy/changes/0001-4xx-attribution-policy.md) — 4xx 三分归因（客户端返客户端不 failover / provider 持续性 failover+挪 pin / 瞬时熔断）；403-quota 重分类为可重试+熔断
- **Sticky Targets**：[0002 端点重命名 /api/active-targets](sticky-targets-observability/changes/0002-rename-active-targets.md) — 端点路径与全链路标识符（handler/DTO/store/组件）从 sticky-targets 改名 active-targets，硬切换不留旧路径

- **Sticky Targets**：[0001 FillFirst 手动重置与可观测性](sticky-targets-observability/changes/0001-fill-first.md) — `CachedOrder` 下沉 `sag-state` 并注入 `AppState`；GET 对 FillFirst 路由报缓存顺序位置/age/TTL，DELETE 按 strategy 分发（FillFirst 清缓存重新洗牌）

## 2026-07-22

- **熔断器**：[0005 手动关闭熔断时清除 StickyFailover 粘性记忆](circuit-breaker/changes/0005-manual-close-clears-sticky.md) — `active_targets` 提升为 `AppState` 共享 Arc，`close` handler 无条件清除 sticky 记忆
- **调度**：[0004 上游 4xx 不再触发重试与熔断](dispatch/changes/0004-upstream-4xx-non-retryable.md) — `is_retryable_error` 对 `Upstream` 按 status 判定，4xx（除 408/429）不可重试、不熔断、failover
- **Body Override**：[0002 per-provider strip_fields 字段剥离](body-override/changes/0002-strip-fields.md) — 新增 provider 级字段剥离，在三层 override 之后硬性移除指定字段

## 2026-07-19

- **供应商**：[0002 Anthropic reasoning_effort 翻译为 extended thinking](providers/changes/0002-anthropic-reasoning-effort.md) — 7 档映射表 + 递归降级；`reasoning_effort` 入口归一化；`thinking_tokens` 捕获
- **调度**：[0003 失败路径补全 reasoning_effort 记录](dispatch/changes/0003-reasoning-effort-failure-paths.md) — `failure_log` 及三个 wrapper 签名补 `reasoning_effort` 参数，6 个调用点补传；4 个 round-trip 测试

## 2026-07-17

- **熔断器**：[0003 探测失败隐瞒冷却下界与流式取消探测槽泄漏修复](circuit-breaker/changes/0003-probe-lifetime-fix.md) — `cooldown_until` 提升至 `CircuitEntry` 跨状态保留（修复探测失败覆盖硬冷却）；`try_acquire_probe` 兜底回收僵死探测槽（修复流式取消导致永久跳过）；`prune_expired` 统一按 `cooldown_until` 清理 Open/HalfOpen

## 2026-07-16

- **熔断器**：[0002 熔断半开探测与冷却期周期探测](circuit-breaker/changes/0002-half-open-periodic-probe.md) — 三态机 + 冷却期内按 `probe_interval_secs` 周期探测，上游恢复最多一个 interval 内被发现（提前感知）；修复 `resolve_provider` 失败路径探测槽泄漏

## 2026-07-06

- **调度**：[0002 路由删除后缓存泄漏修复](dispatch/changes/0002-cache-leak-fix.md) — `resolve_route()` 开头惰性清理已删除路由的 `fill_first_orders`、`route_counters`、`active_targets` 缓存条目

## 2026-06-27

- **UI**：[0002 表格固定表头与玻璃拟态效果](ui/changes/0002-sticky-header.md) — 四个表格页面 sticky 表头、`[&>div]:h-full` 高度传递模式、玻璃拟态表头

## 2026-06-26

- **UI**：[0001 初始化与大屏幕响应式优化](ui/changes/0001-initial.md) — 管理面板规格文档、`xl:`/`2xl:` 断点响应式布局
- **日志**：[0002 日志轮转与清理](logging/changes/0002-rotation-and-cleanup.md) — TTY 自适应输出、每日轮转、启动时清理 7 天前日志

## 2026-06-14

- **熔断器**：[0001 初始化](circuit-breaker/changes/0001-initial.md) — 按错误类型分类冷却

## 2026-06-03

- **架构**：[0001 初始化](architecture/changes/0001-initial.md) — 8 子包 workspace、启动编排
- **配置**：[0001 初始化](config/changes/0001-initial.md) — TOML 加载、热重载、原位保存
- **路由**：[0001 初始化](routing/changes/0001-initial.md) — 路由匹配、目标选择、shuffle 缓存
- **调度**：[0001 初始化](dispatch/changes/0001-initial.md) — 分发、重试退避、流式处理
- **供应商**：[0001 初始化](providers/changes/0001-initial.md) — OpenAI/Anthropic 适配器
- **认证**：[0001 初始化](auth/changes/0001-initial.md) — Bearer token 可选认证
- **分析**：[0001 初始化](analytics/changes/0001-initial.md) — SQLite 异步调用日志
- **日志**：[0001 初始化](logging/changes/0001-initial.md) — tracing、HTTP 日志、trace ID
- **管理 API**：[0001 初始化](admin/changes/0001-initial.md) — 供应商/路由 CRUD
