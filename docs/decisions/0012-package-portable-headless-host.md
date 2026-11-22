# 0012. 裁剪 profile 启动依赖并提供可移植 Linux 运行包

- 状态：已接受
- 日期：2026-11-22

## 背景

Metis 只使用 dsh 的公开 `runProfile`，但上游把该入口放在完整 CLI 包中，其依赖清单同时包含 Web 前端、办公文档和实验功能。按完整清单发布会把未装载的界面带入无显示主机。pnpm 在安装完成后才应用补丁，因此仅裁剪安装后的清单不会自动缩小锁文件中的解析快照。

Nix 开发环境构建的 GNU Linux 二进制记录 Nix store 中的动态加载器，随包 Node.js 也依赖该系统环境。直接在另一发行版运行这些文件不能形成可移植安装包。

## 决策

继续使用公开 `runProfile` 和 app-owned profile。Host 以自身 `package.json` 作为插件安装锚点，所有装载的插件都必须是 Host 明确声明的依赖。

通过 pnpm 补丁只缩减 dsh 的依赖清单，不修改或复制其 JavaScript 源码。清单保留 profile 启动实际导入的公开模块；打包器从 Host 的生产依赖出发，按安装后的 dependencies、optionalDependencies 和已安装 peerDependencies 计算可达包，再删除不可达内容。补丁文件和锁文件均由 pnpm 命令生成。

Linux 的 Metis 可执行文件使用静态 musl 构建。运行包携带 Node.js、其动态加载器和所有随包原生模块所需的运行库，`bin/node` 使用相对路径调用这些文件，不依赖宿主 Nix store。归档同时携带运行库许可证与对应源码定位信息。Git 仍由主机系统提供。

## 影响

dsh 升级时需要重新核对 profile 启动的导入图并更新补丁；完整 Host 契约测试和解包冒烟测试必须同时通过。裁剪发生在发行包生成阶段，开发工作区仍保留上游锁文件解析的完整依赖图。

同一 Linux 归档可以在 Ubuntu 和 NixOS 上验证，不需要为宿主预设 Nix 动态加载器。其他平台的运行包继续使用各自构建产物。
