---
created: 2026-11-09
updated: 2026-11-09
type: 新增
---

# 0016 — Route 表单支持 `systemone` pipeline

## 动机

后端把 route 的 `pipeline` 扩成 `chat` / `image` / `systemone` 之后，前端仍停在两值：`Pipeline` 类型只有 `"chat" | "image"`，两个路由表单的 ToggleGroup 只渲染两项，`api_endpoint` 的显示条件写成 `pipeline === "image"` 的反面（`!== "image"` 即「不显示」，加一个值就会静默错位），`RouteList` 的 pipeline 列直接渲染原始值。于是在 UI 上**根本无法创建或编辑 systemone 路由**，也无法从列表上认出来。

同时，systemone 的互斥字段比 image 更多一层：既不能带 `api_endpoint`，也不能用 `session_sticky`（后端 `dispatch_systemone` 丢弃 session write-back）。这三个字段的显示条件、切换清空与 payload 形态必须成组处理，否则只会得到「表单显示一个后端必然拒绝的组合」或「提交里留着一个上一 pipeline 的残值」。

本轮把 pipeline 扩到三值，并把互斥字段规则收敛进新的纯逻辑层 `sag-ui/src/lib/routePipeline.ts`，两个表单只做绑定。**纯前端**，不改后端 Rust、不改 API 请求形状。

## 需求变更

- `Pipeline` 类型（`sag-ui/src/types/api.ts`）扩为 `"chat" | "image" | "systemone"`，与后端 serde 名逐字对齐；`CreateRoutePayload.pipeline` / `UpdateRoutePayload.pipeline` 随之接受第三值。
- 两个表单（`RouteAddForm.vue` / `RouteEditForm.vue`）的 pipeline ToggleGroup 渲染三项，zod schema 复用 `z.enum(PIPELINES)`；切到 `systemone` 时显示提示行（「此路由仅供 `/v1/systemone` 使用；目标供应商需配置 `apis.systemone`」）。
- 互斥字段规则（三字段）：

| 字段 | 显示 / 可编辑 | 切到 `systemone` 时 | 提交形态 |
|---|---|---|---|
| `api_endpoint` | 仅 `image` 显示输入框（`showsApiEndpoint`） | 清空输入框（`pipelineSwitchReset` 令 `api_endpoint = ""`） | 创建：非 image **省略该 key**（`createEndpointField` 返回 `{}`）；编辑：非 image 发**显式 `null`**（`updateEndpointField`，triple-state patch 语义 = 清除已保存值） |
| `strategy` | 总是显示；`session_sticky` 选项在 `systemone` 下 **disabled** | 若当前是 `session_sticky` 则强制回退 `sticky_failover` | 正常提交；另有对象级 refine 兜底 |
| `session_ttl_secs` / `session_max_entries` | 仅 `session_sticky` 显示 | 两个输入一并清空 | **永不下发**（仅在 `strategy === "session_sticky" && supportsSessionSticky(pipeline)` 时才并入 payload） |

- zod 兜底：两个表单各加一条对象级 `.refine`，拒绝 `strategy === "session_sticky" && pipeline === "systemone"`，错误挂在 `pipeline` 字段上，文案走 `routes.form.systemoneSessionStickyUnsupported`。它的存在是为了覆盖「程序化设值 / 切策略后切 pipeline」这类绕过 `onPipelineChange` 的路径——与后端 `validate_routes` 的 systemone × session_sticky 规则等价。
- `RouteList.vue` 的 pipeline 列改用 i18n 标签（`pipelineLabels` computed，与既有 `strategyLabels` 同形，含 `chat` / `image` / `systemone` 三项，未知值回退原始字符串）；image 行仍拼 `· {api_endpoint}`（`title` 保留全量路径），systemone 行只显示标签。
- i18n 新增 3 个 key（en / zh 齐平）：`routes.form.pipelineSystemone`、`routes.form.systemonePipelineHint`、`routes.form.systemoneSessionStickyUnsupported`。既有 `pipelineChat` / `pipelineImage` 不变。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `Pipeline` 扩为三值（注释写明 systemone 只服务 `/v1/systemone`、且 route 身份只有 `model_name`）；`CreateRoutePayload.api_endpoint` 注释改为「chat 与 systemone 必须省略」，`UpdateRoutePayload.api_endpoint` 保持 triple-state（非 image 发 `null`） |
| [`sag-ui/src/lib/routePipeline.ts`](../../../../sag-ui/src/lib/routePipeline.ts) | 新增纯逻辑层：`PIPELINES`（`as const satisfies readonly Pipeline[]`）、`isPipeline`（DOM / API 值收窄）、`showsApiEndpoint`（仅 image）、`supportsSessionSticky`（仅 systemone 为 false）、`pipelineSwitchReset(to, strategy)`（返回需要改写的字段，缺键 = 不动）、`createEndpointField`（非 image 返回 `{}`）、`updateEndpointField`（非 image 返回 `null`） |
| [`sag-ui/src/lib/routePipeline.test.ts`](../../../../sag-ui/src/lib/routePipeline.test.ts) | 新增 node test runner 用例（顶层断言块）：`isPipeline` 三值 / 非法值、`showsApiEndpoint` 与 `supportsSessionSticky` 真值表、`pipelineSwitchReset` 的「切到 chat / systemone 清 endpoint、切到 image 保留 endpoint + 两个 session 字段 + session_sticky 回退」、`createEndpointField` 省略键、`updateEndpointField` 发 `null` |
| [`sag-ui/src/components/routes/RouteAddForm.vue`](../../../../sag-ui/src/components/routes/RouteAddForm.vue) | schema 改 `z.enum(PIPELINES)`；pipeline ToggleGroup 加 `systemone` 项与提示行；`onPipelineChange` 经 `isPipeline` 守卫后套用 `pipelineSwitchReset`；payload 用 `createEndpointField`，session 字段仅在 session_sticky 且 `supportsSessionSticky` 时并入；新增 systemone × session_sticky 的 refine |
| [`sag-ui/src/components/routes/RouteEditForm.vue`](../../../../sag-ui/src/components/routes/RouteEditForm.vue) | 同上（`z.enum(PIPELINES)` / ToggleGroup 第三项 / `pipelineSwitchReset` / refine），但 endpoint 走 `updateEndpointField`（非 image 发 `null`）；`session_sticky` 下拉项按 `supportsSessionSticky(pipeline)` 置 `disabled` |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | pipeline 列改用 `pipelineLabels` computed（`routes.form.pipeline*` 三个 key，未知值回退原值）；image 行仍拼 `· {api_endpoint}` |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增 `routes.form.pipelineSystemone` / `routes.form.systemonePipelineHint` / `routes.form.systemoneSessionStickyUnsupported` |
| [`sag-ui/src/lib/i18n/zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 同步三个 key 的中文译文，键集与占位符与 en 齐平 |
| [`docs/spec/ui/requirements.md`](../requirements.md) | 记录本变更的前端行为需求与技术细节 |
| [`docs/spec/ui/spec.md`](../spec.md) | 记录本变更的前端行为需求与技术细节 |
| [`docs/spec/CHANGELOG.md`](../../CHANGELOG.md) | 登记 UI 0016 条目 |

## 测试

- `sag-ui/src/lib/routePipeline.test.ts`：`PIPELINES` 恰为三值且顺序稳定；`isPipeline` 接受三值、拒绝 `""` / `"system_one"` / `null`；`showsApiEndpoint` 只对 image 为真；`supportsSessionSticky` 只对 systemone 为假；`pipelineSwitchReset("systemone", "session_sticky")` 同时给出 `api_endpoint: ""`、`strategy: "sticky_failover"`、两个 session 字段清空，而 `pipelineSwitchReset("chat", "session_sticky")` 只清 `api_endpoint`、**不**动 strategy 与 session 字段；`createEndpointField("chat", …)` 返回 `{}`、`updateEndpointField("chat", …)` 返回 `{ api_endpoint: null }`。
- 门禁：`just ui-test`（node test runner）+ `just ui-typecheck`（`vue-tsc --noEmit`，验证三值联合类型扩展后所有消费点收窄）+ `just ui-lint` + `just ui-i18n-check`（en / zh 键集与占位符齐平）。
- 人工核对：新建表单切到 SystemOne → endpoint 输入框消失、session_sticky 选项置灰、提示行出现；编辑一条 systemone 路由并保存 → 请求体含 `"pipeline": "systemone"` 与 `"api_endpoint": null`，不含 `session_ttl_secs` / `session_max_entries`；路由列表的 pipeline 列随语言切换显示「Chat / Image / SystemOne」。
