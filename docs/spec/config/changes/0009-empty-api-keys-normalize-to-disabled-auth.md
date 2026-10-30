---
created: 2026-10-30
updated: 2026-10-30
type: 修复
---

# 0009 — 空 `api_keys` 归一化为关闭鉴权并告警

## 动机

文档长期承诺「`gateway_auth.api_keys` 为空列表 = 关闭鉴权」（见 [使用指南 — 配置](../../../guide/configuration.md) 与 [鉴权需求](../../auth/requirements.md)），但配置校验 `validate_gateway_auth` 对 `Some(GatewayAuthConfig { api_keys: [] })` 直接报错 `gateway_auth.api_keys must not be empty when authentication is configured`，于是照文档写 `api_keys = []` 的 `gateway.toml` 在启动时被拒、进程以 exit 1 退出——**文档契约与实现互相矛盾**，且用户没有任何提示说明「该写的是删掉整个段」。

缺口在「文件里的空列表」这一形态：`normalize()` 此前只负责裁剪 `apis.<key>.base_url` 的尾部 `/`，没有把空 key 列表折叠成规范表示 `gateway_auth = None`。同时鉴权中间件 `sag-router/src/middleware/auth.rs` 的 `api_keys.is_empty()` 分支是 fail-open（放行一切请求）——任何绕过校验拿到 `Some(empty)` 的路径都会让网关静默变成「无鉴权」，方向与安全默认相反。

两条线一起收口：

- **load 侧归一化**：`GatewayConfig::normalize()` 的首步把 `Some(GatewayAuthConfig { api_keys: [] })` 折叠为 `gateway_auth = None`。`normalize()` 在 `load_config`（启动）与 `reload_config`（热重载）两条路径上都先于 `validate()` 运行，因此文件里写 `api_keys = []` 现在被接受，语义是「无鉴权」。
- **规范化后的规范表示**：关闭鉴权的 canonical 形式是 `[gateway_auth]` 段**缺失**（`gateway_auth = None`），不是 `api_keys = []`。保存路径（`sag-config/src/loader/save.rs`）与管理面（`sag-admin/src/admin_settings.rs`）的注释同步改为这一口径：空 key 列表绝不会持久化为 `api_keys = []`，而是整段不写。
- **保留兜底拒绝**：`validate_gateway_auth` 仍对直接构造的 `Some(empty)` 报错——归一化只发生在加载路径，程序化构造（`ConfigMutationCoordinator::mutate` 之类）若直接塞入空 key 列表仍应被拒绝，这是 belt-and-braces 的第二道闸。
- **新增 warning**：`validate_gateway_auth` 改为接收 `warnings` 向量，`gateway_auth == None` 时 push `gateway authentication is disabled: every request to protected routes is allowed`；`validate()` 统一把 warnings 走 `tracing::warn!("configuration warning: {w}")` 输出。于是「鉴权已关闭」在**启动**（`load_config` → `validate`）、**热重载**（`reload_config` → `validate`）、**管理面变更**（`ConfigMutationCoordinator` 的 `validate` 调用）三条路径上都可观测，而不是静默 fail-open。
- **middleware 改为 fail-closed**：`gateway_auth` 的 `api_keys.is_empty()` 分支由「放行」改为「拒绝」，并输出一条 `warn!` 指明这是配置错误及修复方式（删除 `[gateway_auth]` 段以关闭鉴权），请求以 401 + `UNAUTHORIZED` 错误码返回。理由：零个 key 的列表不可能匹配任何凭据，安全方向的读法只能是「拒绝一切」，而规范化后的正常配置根本不会再产生 `Some(empty)`——该分支是最后防线，绝不能 fail-open。

## 需求变更

- `requirements.md`「验证规则」：原条目 `- api_keys 非空`（无条件硬性要求）改为——`gateway_auth` 存在时 `api_keys` 非空；配置文件里写 `api_keys = []` 在校验前被归一化为「`[gateway_auth]` 段缺失」（即关闭鉴权）并只给一条 warning，不报错；仅程序化直接构造的空 key 列表被拒绝。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `normalize()` 首步把 `Some(empty)` 的 `gateway_auth` 折叠为 `None`；`validate_gateway_auth` 改收 `warnings`，`None` 分支 push「鉴权已关闭」warning，`Some(empty)` 仍拒绝。 |
| [`sag-config/src/loader/load.rs`](../../../../sag-config/src/loader/load.rs) | 新增 `load_config_treats_empty_gateway_auth_api_keys_as_disabled`：含 `api_keys = []` 的文件加载成功且 `gateway_auth` 为 `None`。 |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | 修正与实现相反的注释：空 key 列表的持久形态是省略 `[gateway_auth]` 段，而非写入 `api_keys = []`。 |
| [`sag-admin/src/admin_settings.rs`](../../../../sag-admin/src/admin_settings.rs) | 同上口径修正：空数组意味着 coordinator 归一化为缺失段，落盘不会出现 `api_keys = []`。 |
| [`sag-router/src/middleware/auth.rs`](../../../../sag-router/src/middleware/auth.rs) | `api_keys.is_empty()` 分支由放行改为拒绝 + 一条 `warn!`，测试 `auth_empty_key_list_allows_all` 更名为 `auth_empty_key_list_denies_all`（断言 401 + `UNAUTHORIZED`）。 |

## 测试

- `sag-config/src/config.rs` tests：
  - `normalize_collapses_empty_gateway_auth_to_none` — 解析出 `Some(empty)`，`normalize()` 后为 `None` 且 `validate()` 通过。
  - `normalize_keeps_non_empty_gateway_auth_keys` — 非空 key 列表不被误伤，仍是 `Some` 且 key 不变。
- `sag-config/src/config.rs` tests（`validate_tests`）：
  - `rejects_empty_gateway_auth_keys` — 直接构造 `Some(empty)` 仍被 `validate()` 拒绝（兜底闸不倒退）。
  - `warns_when_gateway_auth_is_disabled` — `gateway_auth = None` 时无 error、产生含 `gateway authentication is disabled` 的 warning。
  - `does_not_warn_when_gateway_auth_keys_are_configured` — 配了 key 时不产生 warning。
- `sag-config/src/loader/load.rs` tests：`load_config_treats_empty_gateway_auth_api_keys_as_disabled` — 端到端覆盖「文件里写 `api_keys = []` 不再启动失败」。
- `sag-router/src/middleware/auth.rs` tests：`auth_empty_key_list_denies_all` — 空 key 列表下请求被 401 拒绝、错误码 `UNAUTHORIZED`（原 `auth_empty_key_list_allows_all` 的错误方向已被钉死）。
