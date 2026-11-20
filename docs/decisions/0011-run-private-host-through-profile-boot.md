# 0011. 通过公开 profile 启动入口承载私有 Host

- 状态：已接受
- 日期：2026-11-19

## 背景

直接启动 dsh CLI 需要预先分配端口、探测 TCP listener，并从进程信号推断应用清理状态。Desktop 已证明 Host 可以独立于 Electron 窗口存在，但其私有入口包含桌面 profile、IPC 认证和桌面控制职责。Metis 已有原生客户端，只需要 Harness 的智能体、插件扩展和客户端通信能力。

## 决策

Metis 在现有 `@metis/dsh-host` 包中提供私有 Node.js Host，通过 dsh 公开导出的 `runProfile` 启动 app-owned profile。profile 装配 `dsh-base`、webserver、client-connection 和 Metis 插件；认证使用公开 BrowserAuth，不加载 `dsh-web-app`，不修改或复制 dsh 源码。

Host 从实际 listener 取得回环地址，并通过私有控制通道报告 ready 和实例认证入口。stdin 同时承载 shutdown 命令与父进程生命周期，Host 以退出码区分应用清理成功与失败。Rust 监督层继续拥有 process group 或 Job Object，在应用级清理失败时强制回收进程树。

本地工作台与未来的独立 Server 复用同一 Host。运行数据、监听方式和生命周期所有者可以不同，智能体与插件的装配边界保持一致。

## 影响

Metis 直接依赖实际使用的公开 dsh 包，并用契约测试覆盖启动、认证、通信和关闭。上游升级仍需固定版本和 Node.js 补丁版本验证。

该决策解决运行时组件边界，不要求完整 CLI 包的依赖体积已经最小。后续可以更换更小的公开启动包或裁剪发行依赖，而不改变 Host、插件和 Rust 监督层之间的协议。
