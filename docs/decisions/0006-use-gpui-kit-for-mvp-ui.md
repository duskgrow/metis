# 0006. 使用 GPUI Kit 构建 MVP 界面

- 状态：已接受
- 日期：2026-11-16

## 背景

MVP 需要流式 Markdown、代码编辑和多文件差异审查。自行实现这些控件会分散对智能体主链路的验证。相关源码与发布方式见 [MVP 技术选型调研](../research/mvp-technology-selection.md)。

## 决策

界面使用 crates.io 发布的 GPUI Kit，由 `Cargo.lock` 固定实际版本。应用只通过 `gpui-kit` 使用其重新导出的 GPUI，不跟随 `main`，不直接添加 GPUI 依赖，使用默认上游 GPUI 后端。

对话、代码编辑和差异审查分别使用 `TextView`、`Editor` 和 `Diff`，差异由系统 Git CLI 生成。语法高亮启用 `tree-sitter-languages`，不同时启用 `syntect`。Tree-sitter 的语法树和上游原生示例的主要使用路径更适合以代码编辑为核心的工作台；全量语法避免为未知项目预设语言清单，其构建与体积成本在 MVP 中接受。

## 影响

MVP 可以集中验证项目、会话和审批流程。这些界面依赖只进入工作台构建，Server 构建边界见[架构决策记录 0008](0008-provide-headless-and-workbench-builds.md)。GPUI Kit 及其固定的 GPUI 快照仍处于 `0.x` 阶段，升级需要独立审查并重新验证中文输入、编辑和差异视图。后续需要缩减包体时，优先裁剪 Tree-sitter 语法 feature。
