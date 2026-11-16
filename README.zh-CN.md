# metis

中文为正本；[English translation](./README.md) 仅供参考。

面向个人开发者的多主机智能体工作台，支持跨设备接续会话、任务和审批，并集成代码编辑与差异审查。

## 安装

首次发版后可使用 GitHub Release 的安装脚本：

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/duskgrow/metis/releases/latest/download/metis-installer.sh | sh

按 CONTRIBUTING.zh-CN.md「发布」启用 registry 发布并实际发布 crate 后，也可以从 crates.io 安装。

## 使用

见 `metis --help`。

## 设计文档

- [产品架构与运行方式](./docs/architecture.md)
- [产品范围与里程碑](./docs/product-scope.md)
- [DeepSeek Harness 集成调研](./docs/research/deepseek-harness.md)

## 开发

    direnv allow   # 或 nix develop（进入 devShell 会自动安装 git 钩子）
    just ci        # 本地核心质量门；CI 另跑平台矩阵与托管侧检查

详见 [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md)。

## 许可证

MIT OR Apache-2.0，见 [LICENSE-MIT](./LICENSE-MIT) 与 [LICENSE-APACHE](./LICENSE-APACHE)。
