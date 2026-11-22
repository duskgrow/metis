# MVP 技术选型调研

本文记录 MVP 选型所依据的外部事实，调研日期为 2026-11-16。界面与主机运行时决策分别见[架构决策记录 0006](../decisions/0006-use-gpui-kit-for-mvp-ui.md)和[0010](../decisions/0010-extend-harness-as-host-runtime.md)，验证标准见[里程碑 0](../product-scope.md#里程碑-0验证关键集成)。

## GPUI Kit 发布与组件

官方安装指南使用 crates.io 的 `gpui-kit = "0.9.1"`。该版本于 2026-11-09 发布，应用通过这一个 crate 使用匹配版本的 GPUI、Base、Component 和资源。[安装指南](https://gpui-kit.com/docs/installation) · [crates.io](https://crates.io/crates/gpui-kit/0.9.1)

调研同时阅读了主分支 [`5bece8fc`](https://github.com/longbridge/gpui-kit/tree/5bece8fc054c92c60a083fbf6df25bd11c6025d1)。其中包版本仍为 0.9.1，但包含发布后的提交，并固定 `gpui-pre = 0.3.12`。因此分支名称和包版本号都不能单独标识一份可复现的上游源码。维护者在[#2532](https://github.com/longbridge/gpui-kit/issues/2532)中建议应用只依赖 `gpui-kit`，避免直接 GPUI 依赖产生重复类型。上游安装指南还指出当前锁定依赖图需要 Rust 1.92 或更高版本，接入时需要核对工作区的最低 Rust 版本声明。

MVP 使用三个现有组件：

- [Editor](https://gpui-kit.com/component/editor)：代码编辑、搜索、多光标、Tree-sitter 或 `syntect` 高亮。
- [Diff](https://gpui-kit.com/component/diff)：解析 Git patch，提供多文件虚拟列表、Unified/Split 视图和行内高亮。
- [TextView](https://gpui-kit.com/component/text-view)：保留状态的 Markdown 流式追加、代码块和文本选择。

## 高亮后端

上游在[#3512](https://github.com/longbridge/gpui-kit/pull/3512)中记录了两套后端的实测差异：

- 全部 35 个 Tree-sitter 语法约增加 38 MB，提供语法树、结构化折叠和语法上下文。
- `syntect` 约增加 3.4 MB，覆盖约 200 种语言，但没有语法树，折叠主要依据花括号，编辑使用普通代码上下文。
- 同时启用时 Tree-sitter 优先，`syntect` 不参与自动降级。

当前原生 Gallery 默认启用 `tree-sitter-languages`；独立的 `syntect` 示例用于轻量、广覆盖的高亮场景。[Gallery 配置](https://github.com/longbridge/gpui-kit/blob/5bece8fc054c92c60a083fbf6df25bd11c6025d1/crates/story/Cargo.toml)

## 客户端通信与 Git

`jsonrpsee` 0.26.1 提供可选的 WebSocket 客户端、服务端、接口宏和订阅，支持按 feature 裁剪依赖。[API 文档](https://docs.rs/jsonrpsee/0.26.1/jsonrpsee/)

GPUI Kit 自带的 Git 历史示例通过 `std::process::Command` 调用 `git --no-pager`，再把输出交给 `DiffFile::parse`，说明 Git CLI 输出可以直接作为 Diff 组件输入。该示例使用提交历史 patch；工作区新增、修改、删除和重命名仍由里程碑 0 验证。[示例源码](https://github.com/longbridge/gpui-kit/blob/5bece8fc054c92c60a083fbf6df25bd11c6025d1/examples/tig/src/repository.rs)

## 验证边界

以上结论来自源码、发布记录和官方文档。选型时尚未实测的能力以里程碑 0 的持续验收为准。

2026-11-22 已将 Host 返回的真实 Git 工作区新增、修改、删除和重命名 patch 交给 Diff，并在同一工作区依赖栈的无头窗口中验证可见源码行、重命名文件头及 Unified/Split 行列布局；这不覆盖原生图形会话中的像素渲染。

同日在同一依赖栈中验证 Editor：真实无头窗口从 Rust 源码生成 Tree-sitter 折叠候选并接受键盘输入，高亮后端产生不同的关键字与字符串样式；编辑后的文本经 Host 原子写入并读回一致，项目路径外、外部符号链接和超限内容的写入均被拒绝。这些结果不覆盖原生图形会话中的像素渲染。

同日在真实 TextView 无头窗口中分段追加未闭合粗体和 Rust 代码围栏，验证 Markdown 源与渲染文本更新，并记录到可见代码行。组件的 Tail 跟随在底部随追加移动，上滚后追加保持逻辑滚动位置，回到底部后恢复跟随；这不覆盖原生图形会话中的像素渲染。
