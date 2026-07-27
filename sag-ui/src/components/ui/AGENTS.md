## 硬规则

- **不要手动修改** 该目录下的任何文件（例外情况见下文“例外条款”）
- **不要手动新建** 文件, 只能通过 `cd sag-ui && pnpm dlx shadcn-vue@latest add <comp-name>` 添加
- **不要为该目录文件** 添加文档注释

## 例外条款

- **允许为 cva 组件追加项目自定义 variant**：允许为 `ui/` 下已有的 cva 组件（如 `alert`、`button`、`badge` 等的 `index.ts`）**追加**项目自定义 variant（如 `warning`）。
  - **前提**：必须使用项目 `style.css` 中已定义的主题 token（如 `--warning`、`--destructive` 等），不得使用字面色硬编码。
  - **风险与维护**：此类定制在未来执行 `shadcn-vue add <component> --overwrite` 时会被覆盖丢失。若重新添加或覆盖组件，需手动恢复自定义 variant。
  - **约束**：本例外仅放行追加 cva variant 且使用主题 token 的窄场景，仍严格禁止随意修改组件结构、手动新建组件文件或添加文档注释。

## 前端组件使用规则

- 涉及前端页面绘制时，优先查看已有的 shadcn-vue 组件 (`sag-ui/src/components/ui/`) 是否满足需求，避免使用原生 HTML 元素
