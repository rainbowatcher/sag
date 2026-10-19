---
created: 2026-06-03
updated: 2026-10-19
---

# 变更时间线

按时间倒序列出所有模块的变更记录。

## 2026-10-19

- **日志**：[0004 响应体超限不再毁坏响应](logging/changes/0004-response-body-truncation-fix.md) — `log_http` 不再用 `to_bytes(..).unwrap_or_default()` 把 > 10 MB 响应换成空 body：改为帧级有界收集 + 帧链重组（未超限字节级不变，超限时已收集帧与剩余流 `chain` 后全量转发、headers 不动），日志超限只打 512 B 预览 + `truncated=true` / `collected=<n>`；body 流错误不再被吞，记 `body_error=true` WARN 并传播给客户端；请求侧孪生缺陷（谎报 content-length 导致 handler 收到空 body）同步修复

## 2026-10-18

- **架构**：[0003 HTTP 响应压缩：SSE 防护头、UI 构建期预压缩、动态压缩层](architecture/changes/0003-http-response-compression.md) — SSE 响应补 `Cache-Control: no-cache, no-transform` + `X-Accel-Buffering: no` 防中间层缓冲；UI 构建新增 `postbuild-compress.mjs` 生成 `.br`/`.gz` 副本，`ui_routes` 按 `Accept-Encoding` 直接下发预压缩件（带 `Vary`，运行期零压缩）；`sag-router` 插入 `CompressionLayer`（`Fastest`，`SizeAbove(1024)` ∧ ¬SSE/GRPC/IMAGES，默认开启无配置）；`just clippy` / `just test-rust` 补 `--features embed-ui` 消除门控代码盲区

## 2026-10-17

- **analytics**：[0008 退役旧分析端点，固化两套口径契约分层](analytics/changes/0008-retire-legacy-analytics-endpoints.md) — 删除 `/v1/analytics/{cache,trend,distribution}`（零消费方 + 承诺无法兼现），保留 `/v1/analytics/stats` 与 `/api/logs` 作为热窗口诊断面（`/api/logs` 不迁移到事实表，dashboard 不展示统计），并将「诊断口径 vs 分析口径数字不可对比」写进契约
- **分析**：[0007 call_logs 保留窗口 + 表轮转（热窗口视图）](analytics/changes/0007-call-log-retention-rotation.md) — 整表轮转为冷表 `call_logs_arch_*`，`call_logs_hot`/`call_logs_all` 视图承接热窗口与全史查询。

## 2026-10-15

- **UI**：[0004 UI 多语言支持（i18n，第一版 en/zh）](ui/changes/0004-ui-i18n-language-switching.md) — 新增无依赖的 DIY i18n 层（`t()` 命名插值 + `active → en → key` 三段回退、`useI18n()`、`installI18n()` 同步 `<html lang>`）；locale 经 `localStorage["sag-ui-lang"]` 持久化，默认 `zh` 且白名单校验脏值自愈；Settings 页新增 Language tab；539 个 key 全量迁移（`en.ts` canonical `as const`，`zh.ts: Record<keyof typeof en, string>` 编译期齐平），TanStack 表头改 `() => t(...)`、标签映射改 `computed`、表单 `validators` 改 getter 以免切换语言时冻结；新增 `check-i18n.mjs` 防回潮门禁（键/占位符齐平、花括号合法、CJK 字面量扫描、dead-key 报告）以 `ui-i18n-check` 接入 `verify-ui`

## 2026-10-13

- **配置**：[0006 XDG Base Directory 标准路径布局](config/changes/0006-xdg-base-directories.md) — 配置/数据库/日志按 XDG 标准落位（`$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_STATE_HOME` 下的 `sag/` 子目录）；配置查找顺序 `--config` > `./gateway.toml`（存在时）> XDG 默认；相对 `sqlite://` DSN 锚定数据目录并修复 from_str 不建库的 `SQLITE_CANTOPEN`；test-env 以项目内 XDG 覆盖保持两轨物理隔离

## 2026-10-11

- **配置**：[0007 顶级配置 Admin API 与 Settings 页可视化](config/changes/0007-gateway-config-visualization.md) — 新增 GET/PUT `/api/config` 暴露 server / logging / analytics / backoff / circuit_breaker（含 `probe_interval_secs`）与两个调度 TTL 标量；可热生效参数经 coordinator 统一 mutation 协议保存即时生效，需重启 section 只读标注；Settings 页扩展 7 个 tab
- **日志**：[0003 大请求响应日志与请求关联信息](logging/changes/0003-request-correlation-and-large-body-response-log.md) — 修复三个可观测性缺口：body > 512 KiB 的请求早退路径不再跳过响应日志（补 status / elapsed_ms，不消费 body）；provider 流读取任务改用 `spawn_in_current_span()` 继承 provider/model span（ERROR 可关联 trace_id）；流式响应 commit 后的客户端断开补一条 WARN


## 2026-10-09

- **供应商**：[0006 Chat 入口容忍缺失 `content` 的消息（assistant tool_calls 形态）](providers/changes/0006-chat-message-content-optional.md) — zcode 多轮工具调用回放发送仅含 `tool_calls`、省略 `content` 键的 assistant 消息（OpenAI 规范合法），入口强类型反序列化因 `content` 无 `#[serde(default)]` 而误拒 422；补上 default 后缺失 → `Null`，native 透传重新序列化为 `"content": null`（OpenAI 规范输出形态），codec 既有 `Null → 空内容块` 兼容无需变更

## 2026-10-06

- **UI**：[0003 移动端弹框统一为底部抽屉](ui/changes/0003-responsive-modal-drawer.md) — 新增 `useResponsiveModal()` 收敛 Dialog/Drawer 双轨映射与 768px 断点；`ProviderAddForm`/`ProviderEditForm` 的两份内联映射改为复用，`RoutesPage`、`RouteList`（编辑/删除/清除 Active Target 三处）、`ModelManager`（快捷建单确认）从纯 Dialog 迁到双轨

## 2026-09-30

- **管理 API**：[0002 探针与模型测试的协议对齐](admin/changes/0002-probe-model-test-protocol-alignment.md) — `model/test` 改走 provider adapter（合成单 lane `ProviderConfig` → `create_provider_with_proxy`），per-API headers 分层、`body_overrides`/`strip_fields`/`model_strip_fields`/`role_mappings`/`anthropic_cache_control` 与生产一致，模型级 override 经 `target_extra` 传入；支持三协议（`image` → 400 `UNSUPPORTED_API_KIND`），`api_keys` 上限 16、per-API 超时钳到 30s；协议知识收敛到 `probe_request::minimal_request` + `sag_core::protocol_path`（删除重复的 `provider_chat_endpoint` / `probe_body`）；probe 保持原始 HTTP 与裸语义（2xx 恒 `supported`），`message` 补 body 预览
- **供应商**：[0005 共享 HTTP client 连接池 idle 调优（降低 stale 连接复用）](providers/changes/0005-http-client-pool-idle-tuning.md) — `build_http_client` 写死 `POOL_IDLE_TIMEOUT=30s` / `POOL_MAX_IDLE_PER_HOST=4`，降低「复用已被对端关闭的连接 → 流中途 close_notify EOF」概率；只限 idle 池、不截断在途流；未加回 client 级 total `.timeout()`（保持 0006 流式超时边界）；不修已断的流（C′ 另议）

## 2026-09-28

- **供应商**：[0004 供应商 role_mappings 编辑入口](providers/changes/0004-role-mappings-editor.md) — Provider 新增/编辑表单 Advanced tab 新增 Role Mappings 键值编辑区；编辑表单以 `JSON.stringify` 脏检查实现 PUT「缺省字段保留旧值 / 传 `{}` 清空」语义，新增表单仅在非空时携带该字段

## 2026-09-17

- **架构**：[0002 版本升级提交按实际变更文件过滤与 workspace 版本继承](architecture/changes/0002-version-bump-commit-files.md) — 版本升级脚本参考 `bumpp` 的 `updatedFiles` 语义，仅提交实际变更且已被 Git 跟踪的文件，避免被忽略的 `Cargo.lock` 触发无效 pathspec；版本收敛到根 `Cargo.toml` 的 `[workspace.package] version`，成员 `Cargo.toml` 通过 `version.workspace = true` 继承，脚本不再改写成员文件

## 2026-09-15

- **分析**：[0006 200 状态内上游错误识别 + 取消流写 call_logs](analytics/changes/0006-upstream-error-in-200-and-cancelled-call-logs.md) — 识别 HTTP 200 内交付的上游错误（SSE data frame / 200 body 顶层 `error` 对象）：流式进入 `model_failure` / `stream_error_after_commit` 失败路径，非流式记为失败尝试并可 failover、限流特征映射 `rate_limited`、`call_logs` 记 502（限流 429）+ 上游消息；被取消的流式请求在 `TerminalGuard` 之外补写 `call_logs` 行（499 / `client_cancelled`），保证管理面板日志列表可见
- **Sticky Targets**：[0006 target_index 改为优先级降序秩](sticky-targets-observability/changes/0006-target-index-priority-rank.md) — 前端用 `target_index === 0` 判定主目标，原始数组位置在配置 priority 时会指错主目标；`target_index` 改为优先级降序秩（stable sort by priority 降序、同优先级保持数组顺序、rank 0 = 最高优先级 target），响应 targets 数组保持 config 顺序，全默认优先级时行为不变

## 2026-09-11

- **分析**：[0005 共享 SQLite 连接池 + WAL / BEGIN IMMEDIATE 锁修复](analytics/changes/0005-shared-pool-lock-fix.md) — 旧 CallLogCollector 改为共享正式分析连接池（`new_shared` / `from_pool`，store 不关闭共享 pool）；file DSN 统一 WAL + `synchronous=NORMAL` + busy_timeout 连接选项（`:memory:` 跳过 WAL）；EventWriter / RollupEngine / 身份 sync / 旧 store 整批插入的写事务全部改 `BEGIN IMMEDIATE`，消除双 pool 锁竞争与 deferred-BEGIN 升级竞态

## 2026-09-08

- **配置**：[0005 出站代理配置：顶层 proxy_url + provider proxy_mode 三态](config/changes/0005-outbound-proxy-configuration.md) — 顶层 `proxy_url` 定义全局 HTTP/HTTPS 代理，provider 三态 `proxy_mode`（inherit/enabled/disabled）控制继承/强制/直连；SAG 显式屏蔽系统代理环境变量，`ProxyConfig` 仅存 scheme/host/port 拒绝 userinfo 防泄漏；启动/热重载/mutation/临时测试统一经 `effective_proxy_for` 解析，hot reload 按 effective proxy 差异重建受影响 provider；新增 GET/PUT `/api/settings` 与 Settings 页
- **Sticky Targets**：[0005 target_index 权威首项判定与策略切换残留清理](sticky-targets-observability/changes/0005-target-index-and-strategy-switch-cleanup.md) — DTO 携带 0-based `target_index` 作首项权威来源（取代对象身份）；GET 只对 StickyFailover 展示 pin（切换后残留报 idle）；DELETE 对每种策略都清 active_targets、FillFirst 额外清 order cache；route CRUD 与 gateway hot-reload（含 fallback retry）成功后立即 prune
- **Sticky Targets**：[0004 expired 圆点改 muted 灰绿/灰橙](sticky-targets-observability/changes/0004-expired-dot-muted-hue.md) — expired 不再统一灰色：命中 route.targets 首项 `bg-green-500/40`、非首项/无法确认 `bg-orange-500/40`（expired 表示过期 pin 的配置位置，非当前流量）；pinned 与 fill_first 行为不变

## 2026-09-07

- **Sticky Targets**：[0003 Routes 页 Active Target 状态圆点颜色](sticky-targets-observability/changes/0003-routes-active-target-status-colors.md) — 圆点改四态语义：fill_first 保持 emerald、sticky expired 改灰、pinned 命中 route.targets 原顺序首项改绿、非首项/无法确认改橙；helper 抽出到 `sag-ui/src/lib/activeTarget.ts` 并新增边界测试

## 2026-09-04

- **分析**：[0004 Token 命中率分母改为全部成功样本](analytics/changes/0004-token-cache-hit-rate-all-success.md) — `token_cache_hit_rate` 改为全部成功样本的 ΣCOALESCE(cached_tokens,0)/Σprompt_tokens，明细与 Rollup 混合路径口径一致

## 2026-09-03

- **配置**：[0004 配置未知字段告警重构：serde_ignored 统一 typed 反序列化](config/changes/0004-warn-unknown-config-fields.md) — 移除 raw TOML + 手写白名单，改用 `serde_ignored` 配合 `toml::Deserializer` 统一 typed 反序列化，未知字段仅 warn 不拒绝，load/reload 共用，动态 HashMap key 不误报
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