---
created: 2026-11-03
updated: 2026-11-03
type: 新增
---

# 0015 — systemone API 类型的前端支持

## 动机

后端 `/v1/systemone` 决策接口早已就绪（protected 树下的 `POST /v1/systemone`，作为与 image 同构的「无 IR 分发物种」透传请求与响应），但 `sag-ui/` 对 `systemone` 零支持：Providers 页的架构编辑
（`ApisEditor`）只认识 chat IR 三协议与 `image`，筛选下拉、路由 Target 短标签、模型 tab 与
i18n 字典里都没有这个词，运维无法在面板上给 provider 声明 `apis.systemone`。

同一处还藏着一个**既有 bug**：`ApisEditor.vue` 的 `emitAll()` 只遍历由 `API_KIND_ORDER` 派生的
sections 回写 `apis`，凡是配置里已经存在、但当前 `API_KIND_ORDER` 没有对应 section 的 `apis` key
（例如手改 TOML 写进去的 `systemone`，或任何未来新增协议），在 UI 上做**任意一次保存**都会被静默
丢弃——用户没有报错、没有提示，配置却在服务端被删掉。

本轮把 `systemone` 补进 UI 的 ApiKind 目录，同时把回写路径改为「按基底保真合并」，从根上堵住
「未识别的 `apis` key 被保存动作抹掉」这条数据丢失路径。改动是**纯前端**：不新增 systemone
playground 页面、不新增路由、不加侧边栏入口，后端 Rust 零改动。

## 需求变更

对应改动已落在 [`docs/spec/ui/requirements.md`](../requirements.md) 与
[`docs/spec/ui/spec.md`](../spec.md)：

- `requirements.md` 的「Provider 管理」补一条行为需求：架构编辑支持 `systemone` API 类型；
  `systemone` 与 `image` 同为非 chat-IR 协议，不得作为 fallback 候选、不可协议探针；架构编辑
  保存时按基底合并回写，配置中已存在但当前 UI 无编辑入口的 `apis` key 必须原样透传、不得丢弃。
  「变更记录」追加 0015。
- `spec.md` 的「核心组件」补 `ApisEditor.vue` 条目（编辑 `providers[].apis` 目录、渲染顺序由
  `API_KIND_ORDER` 决定、回写经 `src/lib/apis-catalog.ts` 的 `draftsToCatalog` 做保真合并）；
  「关键决策」新增 `ApiKind` / `ProtocolApiKind` / `ChatIrApiKind` 的三层类型说明；「变更记录」
  追加 0015。

## 边界（明确不做）

- **`systemone` 不参与 fallback**：后端 `sag-config` 的 `fallback` 取值域与需求判定只统计
  chat IR 三协议，显式 `fallback = "systemone"` 在校验阶段直接报错。因此前端 `fallback` 字段
  类型不含 `systemone`（收窄为 `ChatIrApiKind`）。
- **`systemone` 不可探针**：后端探针层对 `systemone` 没有 `minimal_request`（返回 `None` → 400）。
  前端 `PROBE_KIND_ORDER`（`ProtocolApiKind[]`）保持 chat IR 三协议不变，`systemone` 故意排除；
  `defaultProbeKind` 的 `fallback` 参数类型仍是 `ApiKind | null`，对 `systemone` / `image` 的排除
  由运行时守卫（`!== "image" && !== "systemone"` + 末尾 `find` 兜底）完成，不依赖类型层。
  探针面板（协议探针 / 模型测试）始终只有三协议。
- **UI 只做声明与保真读写**：不新增 systemone playground 页面、不新增路由、不加侧边栏入口，
  也不改后端任何 Rust 文件。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/apis-catalog.ts`](../../../../sag-ui/src/lib/apis-catalog.ts) | 新增 API 目录纯函数模块：`catalogToDrafts(apis, order)`（基底 → 有序草稿）、`draftsToCatalog(drafts, base, order)`（保真回写：`order` 内 key 由草稿决定，基底只透传**不在** `order` 里的 key）；另导出 `toChatIrFallback` 供两个 provider 表单共用 |
| [`sag-ui/src/lib/apis-catalog.test.ts`](../../../../sag-ui/src/lib/apis-catalog.test.ts) | 新增 node test runner 用例：目录 ↔ 草稿往返、保真回写保留未知 key、order 决定草稿顺序 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `ApiKind` 加 `"systemone"`；`ProtocolApiKind` 收窄为 `Exclude<ApiKind, "image" \| "systemone">`；新增 `ChatIrApiKind = ProtocolApiKind`；`fallback` 字段类型收窄为 `ChatIrApiKind` |
| [`sag-ui/src/lib/provider-probe.ts`](../../../../sag-ui/src/lib/provider-probe.ts) | `PROBE_KIND_ORDER` 的类型收窄为 `ProtocolApiKind[]`（取值仍是 chat IR 三协议，`systemone` 故意排除）；`defaultProbeKind` 的 `fallback` 参数类型仍是 `ApiKind | null`，对 `systemone` / `image` 的排除由运行时守卫（`!==` 判断 + 末尾 `find` 兜底）完成，不依赖类型层 |
| [`sag-ui/src/lib/provider-probe.test.ts`](../../../../sag-ui/src/lib/provider-probe.test.ts) | 新增用例钉住 `PROBE_KIND_ORDER` 不含 `systemone` / `image`，且 `defaultProbeKind` 对只有 `systemone` / `image` 的 provider 兜底结果不为它们 |
| [`sag-ui/src/components/providers/ApisEditor.vue`](../../../../sag-ui/src/components/providers/ApisEditor.vue) | `API_KIND_ORDER` 加 `"systemone"`（放在 `"image"` 之前）；`apiKindLabels` / `DEFAULT_MODELS_ENDPOINT_HINTS` 补 systemone 条目；`emitAll()` 改为调用 `draftsToCatalog` 保真回写，fallback 校正在同一处内联（守卫 `PROTOCOL_KINDS.has(kind)`，「协议数 ≤1 时清空 fallback」保持旧行为）；systemone section 内加「不支持协议探针 / 模型测试」灰色提示 |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 经 `toChatIrFallback` 收窄 `fallback`，POST 前把 `fallback` 从 `ApiKind` 收窄为 `ChatIrApiKind`，使 `systemone` / `image` 不会出现在请求体；`ProviderEditForm.vue` 同样处理 |
| [`sag-ui/src/components/providers/ProviderEditForm.vue`](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 编辑表单同样在保存前经 `toChatIrFallback` 收窄 `fallback`，`systemone` / `image` 不会进入 `PUT /api/providers` 的请求体；`fallback` 为 null 时仍无条件写该字段（triple-state patch 语义不变） |
| [`sag-ui/src/components/providers/ProviderModelsTab.vue`](../../../../sag-ui/src/components/providers/ProviderModelsTab.vue) | `apiKindLabels` 补 systemone 条目 |
| [`sag-ui/src/components/providers/ProviderList.vue`](../../../../sag-ui/src/components/providers/ProviderList.vue) | `apis` 过滤下拉加 systemone 选项 |
| [`sag-ui/src/components/routes/TargetEditor.vue`](../../../../sag-ui/src/components/routes/TargetEditor.vue) | `API_KIND_SHORT` 加 systemone 短标签 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增 `providers.apis.kindSystemOne` 与「不支持探针」提示的 i18n key |
| [`sag-ui/src/lib/i18n/zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 同步同一批 key 的中文译文，键集与占位符与 en 对齐 |
| [`docs/spec/ui/requirements.md`](../requirements.md) | 「Provider 管理」补 systemone 行为需求，「变更记录」追加 0015 |
| [`docs/spec/ui/spec.md`](../spec.md) | 「核心组件」补 `ApisEditor.vue` 与 `apis-catalog.ts`，「关键决策」补三层类型说明，「变更记录」追加 0015 |
| [`docs/spec/CHANGELOG.md`](../../CHANGELOG.md) | 登记 UI 0015 条目（含 `systemone` 进目录、保真回写、探针 / fallback 边界） |

## 测试

- `apis-catalog.test.ts`：`catalogToDrafts` 按 order 排布，且只为 `apis` 中**已存在**的 kind 生成
  草稿（缺节点不补，空草稿由 `ApisEditor` 的 `addSection` 补）；`draftsToCatalog` 回写时保留
  `base` 中不在 order 里的键（含 `systemone` 与任意未知 key）、order 内 key 由草稿决定（草稿没有
  则删除，`base` 无法复活）。纯函数不改入参。
- `provider-probe.test.ts`：`PROBE_KIND_ORDER` 恒为三协议；`defaultProbeKind` 对只有
  `systemone` / `image` 的 provider 不返回这两个值。
- 门禁：`just verify-ui`（`fmt-check` + `oxlint` + `vue-tsc` + 前端测试 + `just ui-i18n-check`）
  ——i18n 检查保证 en/zh 键集与占位符齐平，类型检查保证 `ApiKind` 联合类型扩展后所有消费点收窄正确。
- 人工核对：Providers 页架构编辑出现 systemone 行且带「不支持探针」提示；保存含未知 `apis`
  key 的 provider 后重新读取，未知 key 仍在。
