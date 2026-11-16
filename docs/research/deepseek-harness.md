# DeepSeek Harness 集成调研

本文记录 DeepSeek Harness 的接入能力和 dsh-TUI 的发行经验。调研基于以下源码：

- DeepSeek Harness：[`39163dd30f703a79079e2d2446545cff3c453738`](https://github.com/deepseek-ai/deepseek-harness/tree/39163dd30f703a79079e2d2446545cff3c453738)，2026-11-13。
- dsh-TUI：[`11c03aced529d9d26c5eb1003abb0834d28b8100`](https://github.com/ccch1mneyyy/dsh-TUI/tree/11c03aced529d9d26c5eb1003abb0834d28b8100)，2026-11-15。

结论来自源码阅读，实际运行与安装验证安排在里程碑 0。

## 接入方式

Metis 可以由主机服务管理 DeepSeek Harness Web 运行时，通过适配器接入会话、事件、审批和任务。Harness 保存智能体历史，Metis 保存主机、项目与会话之间的对应关系。

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

Metis 的客户端与智能体生命周期不同，因此主要参考它的发行方案，会话仍由主机服务托管。

## dsh-TUI 的发行方案

**插件模式**面向已有 Harness 环境的用户，通过 npm 安装 dsh-TUI。初始化配置时需要 Node.js、官方 Harness 和 pnpm。[安装说明](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/docs/getting-started.en.md)

**便携模式**把 Node.js 24、Harness、dsh-TUI 及依赖一起发行。构建时生成 `runtime.tar.gz`，启动时按 TUI 与 Harness 的版本组合解压到缓存，再进入应用。[便携包构建](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/scripts/make-standalone-bundle.mjs) · [启动入口](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/standalone/entry.mjs)

Windows 离线安装包还包含便携 Node.js、pnpm、Harness 和 dsh-TUI，并记录依赖锁与 Node.js 官方校验和。[Windows 载荷构建](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/scripts/windows/build-installer-payload.mjs)

便携版的独立自动化命令仍会调用系统 Node.js，因此它并不是所有入口都完全脱离系统环境。[独立命令入口](https://github.com/ccch1mneyyy/dsh-TUI/blob/11c03aced529d9d26c5eb1003abb0834d28b8100/standalone/entry.mjs)

## 对 Metis 的参考

- 发布包同时携带 Harness、Node.js 和依赖，用户只安装 Metis。
- 按 Metis 与 Harness 的版本组合隔离运行时缓存，并校验解压结果。
- 固定依赖锁与运行时校验和，在干净 Linux、NixOS 和 macOS 环境中验证安装。
- 通过公开 Web 接口接入 Harness，升级时只跟踪这一接口。
- 针对会话接续、提示词重试、审批、重启和运行时升级建立契约测试。
