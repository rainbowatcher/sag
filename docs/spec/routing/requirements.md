---
created: 2026-06-26
updated: 2026-10-21
---

# 路由

## 目标

将客户端请求的模型名映射到具体供应商和实际模型，支持优先级故障转移、多种目标选择策略和通配符兜底。

## 预期行为

### 路由匹配

1. 遍历路由表，按 `model_name` **精确匹配**请求的模型名
2. 若无精确匹配，查找 `model_name = "*"` 的**通配符兜底路由**
3. 仍未匹配 → 返回 `404 MODEL_NOT_FOUND`

> 精确匹配优先于通配符。通配符路由应放在路由表末尾（validator 会对非末尾的通配符发 warning）。

### 目标选择

路由中的 targets 按 `priority` **降序**排列（值越大越优先）。同 priority 组内的行为由 `strategy` 控制：

| 策略 | 首次行为 | 后续请求 | 失败后 |
|------|---------|---------|--------|
| `sticky_failover` | 同 priority 随机打乱 + 缓存 | 优先复用上次成功的 target（sticky） | sticky 失败后 fallthrough，成功后重新 sticky |
| `fill_first` | 同 priority 随机打乱 + 缓存 | 严格按缓存顺序，始终先试首选 | 首选恢复后自动回归 |
| `round_robin` | 从计数器位置开始轮转 | 每次请求计数器 +1 | 失败时依次尝试下一个，计数器继续递增 |
| `session_sticky` | 无会话 pin：按 priority 加权随机全排列 | 会话请求粘住上次成功目标 | 粘住目标失败则 fall-through，成功后重绑到实际成功目标 |

### 会话粘性路由（`session_sticky`）

1. **会话识别**（内置提取链，首个命中者胜）：`metadata.conversation_id` / `conversationId`
   → 顶层 `conversation_id` / `conversationId` → 顶层 `prompt_cache_key` →
   首条 user 消息首个文本块的 SHA-256 归一化哈希兜底。
2. `user` / `user_id` / `metadata.user_id` **不是**会话来源；请求含用户标识时
   **抑制**内容哈希兜底（防单用户垄断）。
3. 会话请求粘住**上次成功的目标**；粘住目标不可用（配置变更 / 禁用 / 熔断跳过）时
   fall-through 到加权随机基底；失败不写回，failover 成功后**重绑**到实际成功目标。
4. 会话 pin 按 `session_ttl_secs` **滑动续期**（命中刷新计时，`0` = 永不过期），
   超容量按 LRU 淘汰；管理员可经 `GET|DELETE /api/session-affinity` 观测与清除。
5. 会话键只以**哈希**形式存在于状态、管理端快照与 `call_logs.session_key`；
   凡识别出会话，`call_logs.session_key` **不区分路由策略**一律记录（NULL 仅表示
   「请求未携带可识别的会话标识」）。
6. **仅支持 `pipeline = "chat"`**：image 管线不做会话写回，`session_sticky` +
   `pipeline = "image"` 在配置校验阶段直接报错，不静默退化为纯加权随机。

### 加权随机基底（priority 即权重）

`session_sticky` 未命中会话 pin 的请求按 **A-Res 加权随机全排列** 排序：

1. 目标权重 = `priority`（**不新增 weight 字段**；分层语义在本基底中取消）；
2. 全 0 → 均匀随机；单个 `priority = 0` 的目标排在尾部（只作最后手段，不离排列）；
3. 一次抽样同时决定加权首选与 fallback 顺序，每次请求重新抽样不缓存。

### 故障转移

- **4xx 错误**（BadRequest、Unauthorized、ModelNotFound）→ **立即停止**，不尝试下一个目标（客户端错误，换供应商也无用）
- **5xx / 网络超时 / 连接错误** → 收集错误，继续尝试下一个目标
- **全部目标耗尽** → 返回 `502 ALL_PROVIDERS_EXHAUSTED`，附带所有失败原因

### 熔断器集成

- 目标重试耗尽后，按错误类型打开对应冷却时长的熔断器
- 冷却期内目标被跳过（视为不可用）
- 冷却结束后自动重新加入路由

### Shuffle 缓存

`sticky_failover` 和 `fill_first` 策略的 shuffle 结果通过 TTL 缓存：
- 默认 7200 秒（2 小时），`shuffle_cache_ttl_secs` 可配置
- 设为 0 表示永不过期，仅在配置变更时重新打乱
- 路由 targets 列表变更时缓存立即失效

### 重试

每个目标的有效重试次数通过三级回落确定：
1. **目标级** `target.retries` — 最先使用
2. **路由级** `route.retries` — 目标级未设置时
3. **全局** `backoff.retries` — 前两级都未设置时

### 超时

每个目标的有效超时通过三级回落确定：
1. **目标级** `target.timeout_secs`
2. **路由级** `route.timeout_secs`
3. **Provider 级** `provider.timeout_secs`

## 配置 Schema

参见 [配置 — RouteConfig & RouteTarget](/guide/configuration#示例)

## 功能边界

### 包含
- 精确匹配 + 通配符兜底
- 四种目标选择策略（含 `session_sticky` 会话粘性）
- 会话识别与会话级粘性路由（priority 当权重的加权随机基底）
- 会话观测 / 清除（`/api/session-affinity`）与 logs 按会话过滤
- 4xx / 5xx 区分处理
- Shuffle 缓存 + TTL
- 三级重试回落
- 三级超时回落
- 与熔断器集成

### 不包含
- 独立的权重分配策略（加权随机只作 `session_sticky` 的内部基底，不单独暴露；
  若未来需要无粘性的纯加权分配再考虑独立策略）
- 请求内容路由（基于 prompt 内容选择供应商）
- 跨路由 fallback（一个模型匹配失败不会尝试另一个路由）

## 与其它模块的交互

- **配置**：读取 `routes` 数组和 `shuffle_cache_ttl_secs`
- **供应商**：检查 `provider.enabled` 和 `provider.enabled_models`
- **调度**：触发 dispatch → provider 调用
- **熔断器**：检查目标是否处于冷却期

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0002 目标模型有效性校验与 Provider 变更联动感知](changes/0002-target-model-validity-and-provider-sync.md)
- [0003 会话粘性路由与 priority 加权随机基底](changes/0003-session-affinity-weighted-random.md)
- [0004 call_logs 会话哈希不区分策略记录](changes/0004-session-hash-for-every-strategy.md)
