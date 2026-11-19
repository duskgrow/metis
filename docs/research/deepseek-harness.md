# DeepSeek Harness 集成调研

本文记录 DeepSeek Harness 的接入能力和 dsh-TUI 的发行经验。调研基于以下源码：

- DeepSeek Harness：[`39163dd30f703a79079e2d2446545cff3c453738`](https://github.com/deepseek-ai/deepseek-harness/tree/39163dd30f703a79079e2d2446545cff3c453738)，2026-11-13。
- dsh-TUI：[`11c03aced529d9d26c5eb1003abb0834d28b8100`](https://github.com/ccch1mneyyy/dsh-TUI/tree/11c03aced529d9d26c5eb1003abb0834d28b8100)，2026-11-15。

下文分别记录源码结论与里程碑 0 的实际验证；尚未实测的能力仍以产品里程碑清单为准。

## 首次运行验证

2026-11-17 在当前 Linux x64 开发环境中，使用 npm 发布的 `@deepseek-ai/dsh@0.2.2-rc.3` 验证了最小插件链路：Metis 的 TypeScript 插件编译为 JS，通过公开 `--patch` 叠加层加载，随后 dsh 在本地回环地址和随机端口启动 Web 运行时。该测试使用隔离的 `DSH_HOME`，不需要模型凭据。

Node.js 补丁版本会直接影响可用性。同一 dsh 在 Node.js 24.18.0 上通过验证，在 24.19.0 与 24.21.0 上均在插件加载前失败，错误来自 `node-addon-require-builtin` 对 Node/V8 私有布局的运行时探测。因此仓库通过 `.node-version` 固定已验证的 Node.js 版本，不能用上游的最低版本声明代替补丁版本验证。

2026-11-18 在当前 Linux x64 开发环境中，Rust 监督层已验证真实 dsh 的启动与关闭、启动超时清理、父进程被强制终止后的普通进程组清理和重新启动。探测确认 dsh web 安装了 `SIGTERM` 处理器并以成功状态退出；测试替身同时覆盖了 dsh 正常执行清理时，其脱离原进程组的受管子进程随之停止的路径。

dsh 的原生 Linux 子进程可以脱离原进程组并交由用户级 systemd scope 管理。当前监督层无法回收 dsh 卡死或被提前强杀后遗留的独立 scope，因此[里程碑 0 的强制终止验证](../product-scope.md)仍未完成；需要真实模型任务验证，并为这个失败路径补充外部 scope 清理。多客户端与重连、干净 NixOS 安装以及无显示发行包也仍未验证。

2026-11-19 在同一环境使用公开发布包验证了私有 Node.js Host：公开 `runProfile` 装配 `dsh-base`、webserver、client-connection 和 Metis 插件，无需 Electron、private Desktop Host 或上游源码投影。Host 不加载 `dsh-web-app` 时根路径返回 404，但公开 BrowserAuth、HTTP RPC、Cordis 事件、智能体创建与销毁、应用级关闭均通过；固定 Node.js 直接启动原 Desktop Host 的 ready、shutdown 和父进程断开路径也已通过。验证尚未覆盖真实模型轮次、跨客户端流式事件与重连；当前完整 CLI 包的依赖闭包仍包含未装载的 Web 组件，发行裁剪不在本次结论内。

## 接入方式

Metis 的私有 Node.js Host 调用公开 `runProfile`，通过 app-owned profile 和 Cordis 插件为 dsh 添加项目、Git 以及主机／项目／会话映射。工作台直接连接该 Host，使用 Harness 的会话、事件、审批和任务能力。

上游仍处于开发者预览阶段，集成需要固定版本，并用契约测试验证升级。

## 与跨设备工作相关的能力

- 会话使用仅追加的事件日志，支持持久化、历史恢复和连续序号。[会话模型](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/docs/subsystems/session.md)
- 每个客户端都有独立订阅，先取得快照，再接收增量事件；重连后可以补齐缺失事件。[订阅控制](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/api/session-controller/src/web/controller.ts) · [事件修复](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/api/gateway/src/client/journal-stream.ts)
- 提示词请求标识符可以去重，响应丢失后可以用同一标识符重试。被接受表示进入会话收件箱，不表示执行完成。[提示词控制](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/api/session-controller/src/commands.ts)
- 审批与提问支持多客户端分发和首次回答生效，重连后仍能收到未处理请求。[交互网关](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/api/gateway/src/index.ts)
- 文件工具提供写入前钩子，可以用来协调智能体修改与编辑器保存。[工具执行钩子](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/core/tools/src/events.ts) · [文件系统钩子](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/fs/fs/src/index.ts)

这些能力支持“另一台设备接入原主机”的产品模型。

Harness 重启后，历史会话仍可恢复；实时任务、输出缓冲和等待回答的交互需要根据持久化记录重新核对。[任务状态](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/docs/subsystems/jobs.md) · [审批记录](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/interaction/user-approval/src/index.ts)

## dsh-TUI 是什么

dsh-TUI 是独立发布的终端应用和适配层，不是 DeepSeek Harness 的分支。它通过公开导出接入 Harness，也保留 `dsh --profile` 启动方式。[适配边界](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/ADAPTER.md)

它的独立入口可以在界面进程内加载 Harness，并复刻部分宿主启动逻辑，通过能力检查和源码指纹跟踪上游变化。[独立宿主设计](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/docs/standalone-host-design.md) · [宿主契约](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/ADAPTER.md#独立入口的宿主契约)

Metis 主要参考它的插件与便携发行方案，会话与主机扩展由同一个 dsh 运行时承载。

## dsh-TUI 的发行方案

**插件模式**面向已有 Harness 环境的用户，通过 npm 安装 dsh-TUI。初始化配置时需要 Node.js、官方 Harness 和 pnpm。[安装说明](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/docs/getting-started.en.md)

**便携模式**把 Node.js 24、Harness、dsh-TUI 及依赖一起发行。构建时生成 `runtime.tar.gz`，启动时按 TUI 与 Harness 的版本组合解压到缓存，再进入应用。[便携包构建](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/scripts/make-standalone-bundle.mjs) · [启动入口](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/standalone/entry.mjs)

Windows 离线安装包还包含便携 Node.js、pnpm、Harness 和 dsh-TUI，并记录依赖锁与 Node.js 官方校验和。[Windows 载荷构建](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/scripts/windows/build-installer-payload.mjs)

便携版的独立自动化命令仍会调用系统 Node.js，因此它并不是所有入口都完全脱离系统环境。[独立命令入口](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/standalone/entry.mjs)

## 对 Metis 的参考

- 发布包同时携带 Harness、Node.js 和依赖，用户只安装 Metis。
- 按 Metis 与 Harness 的版本组合隔离运行时缓存，并校验解压结果。
- 固定依赖锁与运行时校验和，在干净 Linux、NixOS 和 macOS 环境中验证安装。
- 验证公开插件和 Web 接口能否承载 Metis 扩展，升级时跟踪实际使用的接口。
- 针对会话接续、提示词重试、审批、重启和运行时升级建立契约测试。
