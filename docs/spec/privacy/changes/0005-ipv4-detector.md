---
created: 2026-11-11
updated: 2026-11-11
type: 新增
---

# 0005 — IPv4 内置检测器

## 动机

内置检测器覆盖了邮箱 / 手机号 / 身份证 / 银行卡 / IBAN / API key / PEM 私钥，但没覆盖 IP 地址。IP 是基础设施类 PII 里最常见的一种：用户在 prompt 里贴 `10.0.0.1`、`192.168.1.10:8080`、`10.0.0.0/8` 时，网关今天会把它原样发往上游 provider，而它既不在邮箱也不在其它形态的覆盖里。补一个 `ipv4` 检测器即可复用既有的占位符 / 还原 / `allowlist` / 三模式机制，不需要新增任何管线。

难点在误报而不是漏报：`1.2.3.4.5`（四段版本号、`in-addr.arpa` 路径）与 `010.0.0.1`（前导零）都不是地址，把它们脱敏会改坏正常对话内容；而 `host 10.0.0.1.`（句末）与 `10.0.0.1-10.0.0.5`（区间）又必须命中，否则漏检。

## 需求变更

- `requirements.md`「检测范围 → 内置检测器」：新增 `ipv4` 一行——点分四段 IPv4，结构校验为「四段均 0–255 且无前导零（范围写进 pattern 本身）」；内置检测器计数由 7 改 8。
- `requirements.md`「检测范围 → 候选解析规则」：新增一条 IPv4 的邻接语义——两侧不得紧邻字母 / 数字 / `_`；左侧不得是 `.`；右侧的 `.` 只在后面还跟数字时作废（版本号 vs 句末地址）；`/`、`:`、`-`、`+` 不算邻接字符，故 CIDR、端口与区间两端的地址都会命中。
- `requirements.md`「配置 Schema」：`[privacy.builtins]` 增 `ipv4 = true`（默认 `true`，与其余检测器一致；已有配置不写该键即为开启）。
- `requirements.md`「管理面配置」与「功能边界 → 包含」：内置检测器计数 7 → 8。
- `requirements.md`「功能边界 → 不包含」：新增三条边界——IPv6 / MAC / 非点分形式的 IP 变体不在范围；`1.2.3.4.5` 这类被更长点分串包住的地址整段不脱敏；`url` / `data` / `file_data` 字段里的地址按既有例外不扫描（正文里出现的同一串地址仍脱敏）。
- `spec.md`「关键文件」：`Adjacency` 增 `Dotted`、内置 pattern 常量 7 → 8（`IPV4_PATTERN`）；「检测流水线」规则顺序末尾追加 `ipv4`；新增关键决策「IPv4：八位组范围写进 pattern，点分串邻接单独判定」；「不变式」补 3 条（pattern 自身约束四段、`Dotted` 的 `.` 规则、两者互不替代）；「测试策略」计数 57 → 60（`rules.rs` 9 → 11、`engine.rs` 22 → 23）。
- `ui/requirements.md` / `ui/spec.md` / `guide/admin-api.md`：内置检测器计数 7 → 8，`builtins` 键列表补 `ipv4`，快照示例 JSON 补该键。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-privacy/src/rules.rs`](../../../../sag-privacy/src/rules.rs) | 新增 `Adjacency::Dotted`（两侧禁字母/数字/`_`，左侧 `.` 恒拒，右侧 `.` 仅当后随数字时拒）与 `IPV4_PATTERN`（四段八位组 `0–255`、无前导零，范围写在 pattern 里，不新增 `Checksum` 变体）；`boundary_ok` 加一条只服务 `Dotted` 的右侧点判定（读右侧第二个字符） |
| [`sag-privacy/src/engine.rs`](../../../../sag-privacy/src/engine.rs) | `compile` 在 `private_key` 之后追加 `ipv4` 规则（`Adjacency::Dotted`、无 checksum），注册顺序即决胜顺序 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `PrivacyBuiltins` 增 `ipv4: bool`，`Default` 为 `true`（与其余检测器一致） |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | `builtin_pairs` 由 7 项扩为 8 项，差异写回时逐键比较 `ipv4` |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `PrivacyBuiltins` 增 `ipv4: boolean` |
| [`sag-ui/src/lib/privacyConfig.ts`](../../../../sag-ui/src/lib/privacyConfig.ts) | `BUILTIN_KEYS` 与 `cloneBuiltins` 补 `ipv4`（脏标记与 payload 依赖这两处） |
| [`sag-ui/src/components/settings/PrivacyForm.vue`](../../../../sag-ui/src/components/settings/PrivacyForm.vue) | `BUILTINS` 表追加一行（开关 key `ipv4`），`已选 n / 8` 由 `BUILTINS.length` 自动跟随 |
| [`sag-ui/src/lib/i18n/{en,zh}.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增 `settings.privacy.builtin.ipv4` / `.ipv4.hint`；自定义区块文案「seven / 7 项」改为「eight / 8 项」 |
| [`gateway.example.toml`](../../../../gateway.example.toml) / [`gateway.test.example.toml`](../../../../gateway.test.example.toml) | `[privacy.builtins]` 示例补 `# ipv4 = true`，段首摘要补「IPv4 addresses」 |

## 测试

- `sag-privacy/src/rules.rs` 新增 2 例：
  - `dotted_adjacency_rejects_a_continuing_run_but_not_a_sentence_period` — 句末 `.`、`:`、`/`、`-` 区间两端都放行；`1.2.3.4.5` 的任一段、粘连 token（`ip10.0.0.1` / `10.0.0.1x` / `10.0.0.1_2`）都拒绝。
  - `ipv4_accepts_valid_quads_only` — `192.168.0.1` / `0.0.0.0` / `255.255.255.255` 命中；`256.1.1.1`、`1.2.3.256`、`010.0.0.1`、三段形态、`v1.2.3.4.5`、`1.2.3.4.5` 全部无命中。
  - `builtin_patterns_match_expected_shapes` 的用例表由 5 条扩为 6 条（补 `ipv4`）。
- `sag-privacy/src/engine.rs` 新增 `ipv4_masks_addresses_but_not_version_runs` — 地址命中且 `kind = ipv4`；同一地址重复出现与 `10.0.0.1-10.0.0.5` 区间共 3 段全部命中；句末地址命中；版本号 / 越界八位组 / 前导零 / 粘连 token 不命中。
- 既有 fixture 同步（仅补字段，无行为断言变化）：`sag-privacy` 的 `bare()` / `only()`、`sag-config` 的默认值与 round-trip 断言（新增 `ipv4 = true` 用例）、`sag-config/tests/privacy_persistence.rs`、`sag-state/tests/privacy_config_mutation.rs`、`sag-state/src/config_mutation.rs` 的命令 fixture、`sag-ui/src/lib/privacyConfig.test.ts` 的快照；`sag-admin/tests/privacy_config.rs` 的默认快照 spot-check 增加 `builtins.ipv4 == true`。
- 计数：`cargo test -p sag-privacy` 60（原 57）；其余 crate 与前端用例数不变。
