## 硬规则

- **不要手动修改** 该目录下的任何文件
- **不要手动新建** 文件, 只能通过 `cd sag-ui && pnpm dlx shadcn-vue@latest add <comp-name>` 添加
- **不要为该目录文件** 添加文档注释

## 前端组件使用规则

- 涉及前端页面绘制时，优先查看已有的 shadcn-vue 组件 (`sag-ui/src/components/ui/`) 是否满足需求，避免使用原生 HTML 元素
