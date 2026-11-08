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

review 又发现一处**空白 key 漏洞**：校验只看「列表是否为空」而不看 key 内容，于是 `api_keys = [""]`（或 `["   "]`）能通过加载；中间件把 `Authorization: Bearer ` 剥前缀后 trim 得到空字符串，再用 `api_keys.iter().any(|k| k == &provided_key)` 匹配时正好命中那个空白条目——**一个空白 key 等于给空凭据发了通行证**。绕开的正是「空列表 = 关闭鉴权」这个约定的反面：配置里看不见密钥，实际却是「空密钥有效」。收口方式是把「key 是否可用」收敛成唯一判定入口：

- **load 拒绝全空白列表**：`validate_gateway_auth` 对 `has_usable_key() == false` 的 `gateway_auth` 报错，`Some(empty)` 与 `Some(全空白)` 同一口径；因此只含空白 key 的文件在启动 / 热重载即被拒绝，而不是静默变成「空凭据可登录」。含至少一个可用 key 的列表（哪怕混了空白条目）仍通过。
- **管理面写入拒绝空白条目**：`ConfigMutationCoordinator::update_settings` 在构造候选配置前就检查提交的 key 数组，含空白条目即 `AppError::Config`（admin API 报 400），所以空白 key 既进不了内存也落不了盘；空数组仍合法（关闭鉴权）。这条比 `has_usable_key()` 更严：`["sk-ok", ""]` 在管理面被拒，而在文件里混写是合法的（见下条）。
- **单一事实来源**：`GatewayAuthConfig::has_usable_key()`（至少一个 key 含非空白字符）是「鉴权配置是否可用」的唯一判定入口，校验与中间件都经它判断，不再各自数 `api_keys.len()`。
- **中间件的空白安全比较**：中间件把该分支的条件从 `is_empty()` 改为 `!has_usable_key()`，并让凭据先 `trim()`、trim 后为空一律不匹配；于是即使一个空白 key 绕过校验到达内存，也不可能被空 `Bearer` 凭据命中。
- **加载时裁剪 key 首尾空白**（review 二轮补丁）：`normalize()` 在折叠空列表之后把每个 `gateway_auth.api_keys` 条目原地 trim。此前 `api_keys = [" sk "]` 能通过校验并启动，但中间件拿 trim 过的 `"sk"` 去比对未 trim 的配置值，永远不相等——网关能启动却 401 所有请求。trim 放在归一化而不是比对处，使 `has_usable_key()`、校验与中间件看到同一份规范形式，也与管理面写入侧（已 trim）对齐；空白条目不被丢弃，`["good", ""]` 仍合法，纯空白列表 trim 后变成 `[""]`，仍在随后的校验里报「no usable key」。

## 需求变更

- `requirements.md`「验证规则」：原条目 `- api_keys 非空`（无条件硬性要求）改为——`gateway_auth` 存在时 `api_keys` 必须含至少一个非空白 key（`has_usable_key()`），加载归一化先裁掉每个 key 的首尾空白（`[" sk "]` → `["sk"]`）。**按来源区分**：配置文件里写零长度的 `api_keys = []` 在校验前被归一化为「`[gateway_auth]` 段缺失」（即关闭鉴权）并只给一条 warning，不报错；程序化构造的 `Some(empty)` / `Some(全空白)` 不是「关闭鉴权」的等价物，`validate()` 直接报错，绕过校验时中间件 fail-closed（401）。
- `requirements.md`「认证模式」：再收紧一格——`gateway_auth` 存在时要求至少一个**含非空白字符**的 key（`has_usable_key()`）；`api_keys = [""]` / `["   "]` 在校验时被拒绝，与 `api_keys = []` 不同：后者合法（归一化为段缺失），前者是配置错误。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `normalize()` 首步把 `Some(empty)` 的 `gateway_auth` 折叠为 `None`；`validate_gateway_auth` 改收 `warnings`，`None` 分支 push「鉴权已关闭」warning，`Some(empty)` 仍拒绝。review 补丁后可用性判定收敛到 `GatewayAuthConfig::has_usable_key()`（至少一个 key 含非空白字符），`Some(empty)` 与全空白列表同一口径拒绝。 |
| [`sag-config/src/loader/load.rs`](../../../../sag-config/src/loader/load.rs) | 新增 `load_config_treats_empty_gateway_auth_api_keys_as_disabled`：含 `api_keys = []` 的文件加载成功且 `gateway_auth` 为 `None`。 |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | 修正与实现相反的注释：空 key 列表的持久形态是省略 `[gateway_auth]` 段，而非写入 `api_keys = []`。 |
| [`sag-admin/src/admin_settings.rs`](../../../../sag-admin/src/admin_settings.rs) | 同上口径修正：空数组意味着 coordinator 归一化为缺失段，落盘不会出现 `api_keys = []`。review 补丁后 `ConfigMutationCoordinator::update_settings` 在构造候选配置前就拒绝含空白条目的 key 数组（`AppError::Config` → 400），因此只含空白字符的写入到不了落盘，也不会先提交再全量 401；空数组仍合法（关闭鉴权）。 |
| [`sag-router/src/middleware/auth.rs`](../../../../sag-router/src/middleware/auth.rs) | `api_keys.is_empty()` 分支由放行改为拒绝 + 一条 `warn!`，测试 `auth_empty_key_list_allows_all` 更名为 `auth_empty_key_list_denies_all`（断言 401 + `UNAUTHORIZED`）。review 补丁后该分支改为 `!has_usable_key()`（空列表与全空白列表同一道闸），且凭据先 `trim()`、trim 后为空一律不匹配——`api_keys = [""]` 认证通过空凭据的漏洞被堵上。 |

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
- review 补丁（空白 key 漏洞）新增的测试：
  - `sag-config/src/config.rs` tests：`rejects_blank_only_gateway_auth_keys` — 全空白 key 列表被判为无效配置；`accepts_gateway_auth_with_blank_entry_alongside_usable_key` — 空白条目与可用 key 混写被接受（且两个条目原样保留，校验既不丢弃也不把空白项当成凭据）；`has_usable_key_requires_non_whitespace_content` — 唯一判定入口本身的行为（空、`[""]`、`["", "   ", "\t"]` 为 false，`["key"]`、`["", " key "]` 为 true）；`validate_emits_auth_disabled_warning` — `validate()` 真的把收集到的「鉴权已关闭」warning 发到日志（收集 ≠ 输出）；`validate_emits_no_auth_disabled_warning_with_usable_key` — 有可用 key 时不误报「鉴权已关闭」。
  - `sag-config/src/loader/load.rs` tests：`load_config_rejects_blank_only_gateway_auth_api_keys` — 文件里写 `api_keys = [""]` 加载失败（与 `api_keys = []` 合法相反）；`load_config_keeps_auth_enabled_with_blank_entry_alongside_usable_key` — 混写时鉴权仍启用；`load_config_rejects_present_but_keyless_gateway_auth_section` — 写了 `[gateway_auth]` 段却没有可用 key 时拒绝；`load_config_trims_gateway_auth_api_keys` — `api_keys = [" sk "]` 加载后内存里是 `["sk"]`（空白条目保留）。
  - `sag-admin/src/admin_settings.rs` tests：`put_settings_blank_only_api_key_list_is_400` — `PUT /api/settings` 提交只含空白字符的 key 数组返回 400，配置不变（更早的 `put_settings_blank_api_key_is_400` 已覆盖「可用 key + 空白条目」混写被拒，不是本 commit 新增）。
  - `sag-router/src/middleware/auth.rs` tests：`auth_empty_bearer_returns_401` — 空 `Bearer` 凭据返回 401（review 发现的漏洞本身）；`auth_blank_only_key_list_denies_all` — 内存里是 `Some(全空白)` 时一律 401；`auth_blank_configured_key_never_matches_blank_credential` — 混写列表里空白条目永远不会被空凭据命中；`auth_authorizes_a_key_that_normalize_trimmed` — 经 `normalize()` 的 `[" sk "]` 存为 `["sk"]`，中间件放行 `"sk"`（两侧都 trim，故 `" sk "` 同样归一为同一凭据）并拒绝无关凭据；`auth_blank_only_key_list_warns_once_per_request` — fail-closed 分支每个请求恰好一条 warning。
