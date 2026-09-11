---
created: 2026-06-03
updated: 2026-08-11
---

# 分析

## 目标

为每次 API 请求记录请求级最终结果与 target 级尝试事实，提供可验收的失败率、延迟、Token、缓存统计，并支持按 route / target 两个层级查询，同时保留旧日志与旧统计接口的兼容性。

## 预期行为

### 分析层级

- 统计必须区分两个层级：**route 级**与**route target 级**（下称 target 级）。
- route 级以一次用户请求的**最终结果**为主：一次请求只产生一个 route 级结果。
- target 级以**真正发起上游调用的 target 尝试结果**为主：实际尝试了几个 target，就产生几条 target 级结果。
- 示例（route 内 target A 失败、target B 成功）：route 级成功 1 次；target 级 A 失败 1 次、B 成功 1 次。
- route 成功率只依据最终结果；target 成功率只依据实际尝试。

### 身份模型

- `route_name` 是 route 的**稳定业务身份**：用户在配置中编辑 route 语义（target 列表、策略等），但 route name 一旦建立即为稳定身份，前端不允许修改 route name。
- route_id 与 target_id 必须由系统生成（UUID v7），**只存在数据库中**：不写入 TOML，不向 API/UI 直接暴露。
- target 的稳定业务身份为 `(route_name, provider, real_model)`：
  - target 重排或策略字段（优先级、策略、重试、超时等）变化，不改变 target 身份；
  - provider 或 real_model 变化，形成**新**的 target 身份；
  - target 被移除后重新出现相同的 `(route_name, provider, real_model)`，必须复用原身份。
- 旧历史不做猜测性身份回填：旧记录的身份字段保持为空。
- `request_id` 与 `route_id` / `target_id` 使用不同 UUID 版本分工（详见 spec.md「request_id 用 v4、身份 ID 用 v7 的分层」）：
  - `request_id`（UUID v4）：每请求一个的临时追踪 token，纯随机不泄露时间，作为 `request_results` 主键与幂等去重依据；不进 API 响应。
  - `route_id` / `target_id`（UUID v7）：持久身份，时间有序、DB 插入友好、可调试可读；不进 API 响应。
  - 两者共存是「临时追踪 vs 持久身份」的语义分层，不统一为单一版本。

### 数据事实

- 保留现有 `call_logs` 作为**日志/诊断数据源**，同时作为旧分析接口的数据源。
- 新增请求级最终事实（下称 `request_results`）：每个纳入分析的请求恰好一条，记录最终结果。
- 新增 target 级事实（下称 `target_attempts`）：每次实际发起上游调用的 target 尝试一条，一次请求 0..n 条。
- `actual_model`：运行时实际调用的模型。配置 `real_model=None`（passthrough）时为请求模型；显式 `real_model` 时为路由解析后的实际模型。`request_results` 保存最终成功的 `actual_model`（仅成功非 NULL）；`target_attempts` 保存每次实际尝试的 `actual_model`。
- 正式新分析必须基于上述两级事实计算，**不**直接从 call_logs 推导失败率。
- 新写入 call_logs 的 route_id/target_id 可以为空，并与对应的新事实关联；旧记录保持为空。
- 每个事实记录必须同时保存开始时间与完成时间，按完成时间归属分桶。

### 写入可靠性与原子性

- 一次请求的请求级最终事实与它的**全部** target 级事实，必须作为一个**原子分析事件**写入：要么全部落库，要么都不落库。
- 正式分析事件必须异步写入，不阻塞请求处理；写入队列满时系统应**背压**（放慢/等待），不得静默丢弃。
- 正常关闭时，系统必须**排空**待写入的分析事件后再退出。
- 异常崩溃允许少量未提交的分析事件丢失（不要求崩溃恢复）。
- call_logs 保留原有“可丢弃”的诊断语义，不纳入上述原子性与背压保证。
- 旧 call_logs 链路与正式分析路径**共用同一个 SQLite 连接池**（生产装配经 `CallLogCollector::new_shared(pool)` 构建，不再为同一文件另开私有 pool）；file-backed 分析库统一使用 WAL journal mode + `synchronous=NORMAL` + 5s busy_timeout 的连接选项，`:memory:` 等内存 DSN 除外（SQLite 不支持内存库 WAL）。
- 旧 store 的批量插入必须整批走单个 `BEGIN IMMEDIATE` 事务（原子落库，单次短写锁）；正式 writer / rollup / 身份 sync 的写事务同样以 `BEGIN IMMEDIATE` 开始，先取写锁，避免 deferred BEGIN 升级竞态（SQLITE_BUSY_SNAPSHOT）。

### 最终状态与失败率

- 每个请求级最终结果必须有一个**最终状态**，取值：`success` / `client_error` / `no_route` / `all_disabled` / `model_failure` / `client_cancelled`。
- `failure_reason` 是独立的诊断字段，与最终状态正交。
- 认证失败（未通过认证）的请求必须**完全排除**：不产生分析事实，不计入任何失败率。
- 通过认证但请求输入/参数错误 → `client_error`：计入整体失败率与客户端错误率；不进入 route/model/target 延迟统计，也不进入模型失败率。
- `no_route`（无匹配路由）与 `all_disabled`（匹配路由但 target 全部不可用）属于路由/配置失败。
- `model_failure` 涵盖：目标全部耗尽、上游错误/超时、流式已开始输出后的错误。
- `client_cancelled`（客户端取消）独立计为整体失败率，不计入模型失败率与客户端错误率。
- target 级尝试结果枚举：`success` / `failure` / `cancelled`（客户端取消的在途尝试）；`cancelled` 仍属实际尝试（计入分母），但不计 success 也不计 failure。
- 客户端取消发生在尚无在途 target 尝试时（如重试/退避间隙）：`request_results` 仍产生一条 `client_cancelled`，`target_attempts` 为 0 条。

失败率口径必须按下述分母计算：

- **整体失败率** = 所有纳入分析请求中非 success 的请求数 / 所有纳入分析请求数。
- **客户端错误率** = client_error 请求数 / 所有通过认证进入 API 处理的请求数。
- **route 成功率** = 匹配到该 route 的非 client_error 请求中 success 的请求数 / 匹配到该 route 的非 client_error 请求数；all_disabled 计入该 route 失败，no_route 不归属任何 route。
- **路由/配置失败率** = (no_route + all_disabled) 请求数 / 所有通过认证进入 API 处理且非 client_error 的请求数。
- **模型失败率** = model_failure 请求数 / (success + model_failure) 请求数；client_error、no_route、all_disabled、client_cancelled 均排除。
- **target 成功率** = 成功的 target 尝试数 / 实际发起的 target 尝试数。

### 重试与故障转移

- 不单独提供"重试次数"指标。
- 同一 target 内多次重试最终成功：target 级只记成功一次；重试耗时计入 route 总耗时。
- 请求内 target A 失败后由 target B 成功：不降低 route 成功率；A 记一次失败尝试，B 记一次成功尝试。
- **route 延迟**：从请求进入调度开始，到最终完成/失败/取消为止，包含重试、退避与 fallback 时间。
- **target 延迟**：从实际发起该 target 的上游调用开始，到该 target 完成/失败/取消为止。
- 流式响应在**首个数据块输出前**失败：允许重试/fallback。
- 流式响应**已开始输出后**发生错误：不得切换 target，直接记为最终 `model_failure`。
- HTTP 状态码为 200 不保证分析事件已成功写入（分析异步落库，客户端可能先收到响应）。

### 指标口径（Token 与缓存）

- 正式成本与吞吐统计**只统计最终成功响应**的 Token；失败的 target 尝试 Token 未知，统计中不得当作 0。
- 必须提供：请求吞吐、输入 Token 吞吐、输出 Token 吞吐。
- 必须提供缓存命中指标：缓存请求命中率与缓存 Token 命中率。
- 缓存字段为 NULL 表示**未知**，不等于未命中；UI 必须将"未知"与"未命中"区分显示。
- 请求命中率样本集 = 缓存信息已知（非 NULL）的最终成功样本（route 级 = success 请求，target 级 = success 尝试）；**未知样本不计请求命中率分母**，不得当作 0。
- Token 命中率样本集 = 全部最终成功样本（route 级 = success 请求，target 级 = success 尝试）；Token 命中率 = Σ COALESCE(cached_tokens, 0) / Σ prompt_tokens（token 加权 SUM/SUM，非每请求比例的算术平均）；cached_tokens 为 NULL 或 0 时分子贡献 0，其 prompt_tokens 仍计入分母；分母为 0（无成功样本）时返回 null。
- 请求命中率 = 命中样本数 / 已知样本数。
- 命中率统计必须可由 1h Rollup 跨桶正确合并（只依赖可合并计数，不得依赖明细推导）。

### 延迟指标

- 端到端、上游、TTFT 三类延迟必须提供：平均 / P50 / P95 / P99。
- 延迟缺失值必须排除，每个指标必须提供独立覆盖率（有值样本占比）。
- 有效样本少于 100 时，指标必须标记 `low_sample`。

### 时间与分桶

- 所有分桶按 **UTC** 对齐。
- 事实按**完成时间**归属小时桶；同时保存开始/完成时间用于明细。
- 查询参数 from/to 必填：Unix 毫秒，区间**左闭右开**（含 from、不含 to）。
- 跨度 ≤ 24h：必须全部查明细，结果精确。
- 跨度 > 24h：历史部分使用单层 1 小时 Rollup，最近 24 小时用明细；边界/缺失处必须由明细补齐。
- Rollup 的近似分位延迟目标约为 1% 值误差；低样本/溢出的分位不绝对保证精度。
- 延迟分布使用规范化的**稀疏对数桶**：1ms 起、约 1.01 倍递增，0 单独成桶，超长值进入溢出桶，NULL 不入桶。

### Rollup 运维

- 每小时执行一次后台批处理。
- 启动时只**检测**缺失/标脏的 Rollup 桶并登记，不立即计算。
- 每轮最多补偿 24 个小时桶。
- 只重算缺失或标脏的桶；重算状态必须持久化、可重试、幂等。
- route 级、target 级与延迟直方图属于同一小时桶时，必须**原子提交**（同桶全部成功或全部不成功）。
- Rollup 只加速查询，**不得删除**事实明细。

### API 与 UI

- 保留旧分析接口与旧日志接口，行为与数据源不变（按旧明细聚合）。
- 新增统一查询模型，通过 `scope=route|target` 选择统计层级；默认 route 视角，UI 提供 target 视角切换。
- route 视角不提供 provider / actual_model 过滤；供应商与实际模型维度的过滤只在 target 视角提供。
- 查询结果必须包含：总体汇总、按可归属对象（route 或 target）的分组、未归属失败（如 no_route）的分类。
- 趋势查询默认返回 Top20 + others，最多 50 个序列。
- route_id / target_id 不得向 API/UI 直接暴露。

## 配置 Schema

```toml
[analytics]
enabled = true
storage_dsn = "sqlite://analytics.db"   # 或 ":memory:"
```

配置只保存用户可编辑的 route 语义；系统生成的 route_id/target_id 不写入 TOML。

## 兼容性说明（迁移前现状）

- 旧 `call_logs` 可能一条请求对应多行记录；旧 `/v1/analytics/*` 接口按旧明细聚合，保持兼容，不迁移为两级事实口径。
- 旧 call_logs 写入链路保留“可丢弃”诊断语义：可能丢弃诊断事件，不承诺背压或原子性。
- 旧 call_logs 的落库路径并入与正式分析共享的连接池（`SqliteLogStore::from_pool` / `CallLogCollector::new_shared`）：store 自身不关闭共享 pool，旧链路的批量插入与正式写事务共用 WAL + `BEGIN IMMEDIATE` 的并发安全设置。
- 旧历史不强行回填 route/target 身份，旧记录中的身份字段为空。
- 旧接口的既有响应字段（如聚合错误率、按 provider/model 过滤）按迁移前现状保留；新分析的口径以本文「最终状态与失败率」为准。

## 功能边界

### 包含
- 两级事实记录（请求级最终结果、target 级尝试）
- 最终状态分类与统一失败率口径
- 延迟 / Token / 缓存指标
- UTC 小时分桶与 1 小时 Rollup
- 统一查询 API 与 UI（route/target 视角切换）
- 旧日志接口与旧分析接口兼容保留

### 不包含
- 认证失败、未进入 API 处理的请求（不纳入业务分析）
- 旧历史身份回填
- 任意多级交叉分组（第一版只支持单一主分组维度）
- 单独的重试次数指标
- 实时推送（管理面板通过定时轮询刷新）
- 数据导出
- PostgreSQL / MySQL 支持（仅 SQLite）

## 与其它模块的交互

- **调度**：请求处理完成后提供最终结果与 target 尝试事实；分析事件异步写入，不阻塞请求。
- **路由**：提供 route/target 的稳定身份与最终结果归因（no_route / all_disabled）。
- **认证**：认证失败的请求不产生分析事实。
- **管理面板 / UI**：调用统计接口展示图表，支持 route/target 视角切换；保留旧日志页面。
- **配置**：读取 `[analytics]` 配置与 route 语义。

## 变更记录

- [0005 共享 SQLite 连接池 + WAL / BEGIN IMMEDIATE 锁修复](changes/0005-shared-pool-lock-fix.md)
- [0004 Token 命中率分母改为全部成功样本](changes/0004-token-cache-hit-rate-all-success.md)
- [0003 UUID v4/v7 版本分层决策](changes/0003-uuid-version-layering.md)
- [0002 双层事实与 Rollup](changes/0002-two-level-facts.md)
- [0001 初始化](changes/0001-initial.md)
