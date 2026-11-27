# Metis 工作台原生组件契约（Draft）

本文帮助后续实现者与评审者把已确认的工作台体验转化为可验证的原生探针和小切片实现。[工作台体验与设计边界](workbench-experience.md)是体验正本；[Metis 架构](architecture.md)与 [ADR 0006](decisions/0006-use-gpui-kit-for-mvp-ui.md)决定工程边界；[产品范围与里程碑](product-scope.md)持有交付状态、唯一下一阶段和后续顺序，本文不另排计划。

## 当前接手入口

实现保持 crate-private，模块组成见 [workbench 代码](../crates/metis/src/workbench/mod.rs)。无上下文读者按以下地图进入代码、rustdoc 和测试，本文不复述 API：

- 纯行为与本地状态：[`identity.rs`](../crates/metis/src/workbench/identity.rs)、[`stats.rs`](../crates/metis/src/workbench/stats.rs)、[`history.rs`](../crates/metis/src/workbench/history.rs)、[`focus.rs`](../crates/metis/src/workbench/focus.rs)。
- 主题与基础适配：[`theme.rs`](../crates/metis/src/workbench/theme.rs)、[`markdown.rs`](../crates/metis/src/workbench/markdown.rs)、[`disclosure.rs`](../crates/metis/src/workbench/disclosure.rs)。
- 原生 Session 纵切：[`session.rs`](../crates/metis/src/workbench/session.rs)及其 `session/` 子模块；Host 快照与有序事件的纯适配位于 [`host_projection.rs`](../crates/metis/src/workbench/session/host_projection.rs)，交互覆盖从 [Session 测试](../crates/metis/src/workbench/session/tests.rs)和 [review 测试](../crates/metis/src/workbench/session/review_tests.rs)进入。
- Host 接入：[`session_transport.rs`](../crates/metis/src/session_transport.rs)负责本地 Host／项目／Session 冷启动准备、连接、follow 与 `$events` 原样转发和 prompt/审批命令；UI 线程桥接位于 [`host_bridge.rs`](../crates/metis/src/workbench/session/host_bridge.rs)，把事件喂入 reducer 生成投影并给出生产 effect。真实 Host 提示词与审批闭环见 [`session_transport.rs` 集成测试](../crates/metis/tests/session_transport.rs)，产品入口冷启动、通知和关闭清理见 [`cold_start_tests.rs`](../crates/metis/src/workbench/session/cold_start_tests.rs)。

[产品入口](../crates/metis/src/main.rs)启动私有 Host，打开启动时所在目录并创建 Session，再通过桥接更新真实投影；启动失败保持 Offline，关闭时清理传输与 Host。交互 preview 仅由测试注入投影和 effect，不是产品运行模式。Host 纯投影适配器复用 `SessionJournal` 校验顺序并保留原始审批请求；Queue 是客户端本地队列，入队不向 Host 提交，Session 空闲时由 Composer 沿普通提示词路径派发队首（规则见 5.3），提交内容只有提示词文本。模型与 effort 选择、添加项目流程和断线自动重连尚未实现。模型投影为空，原生 Composer 的提示词动作仍不可用，产品内的入队与派发也因此不可达。Workbench 已通过 Host `git/diff` RPC 展示真实 Worktree patch，包含 Unified/Split、空 patch、Offline、Host 错误与返回焦点；请求结果按 Worktree 和请求 ID 过滤。Subagent transcript 尚未进入 Host 投影，故其路由仍为不可交互的返回占位。`FocusTopology` 的方向移动只有纯模型，未绑定产品键位。

已有纯行为测试、Session 测试、组件 headless probes、产品入口真实 Host 冷启动测试和传输层真实 Host 集成测试通过，具体覆盖以测试代码为准。新增 Worktree Diff headless 测试覆盖原生入口、真实 patch 投影、模式切换、离线状态及来源焦点恢复；真实 Host 集成测试覆盖 Git patch 成功与项目错误。这些证据不覆盖 Diff 的 OS 原生视觉验收、Subagent transcript、原生 Composer 经真实 Host 的端到端提交、Queue 空闲派发的真实 Host 验收或完整冷启动；高对比、200% 缩放、真实 IME、真实辅助技术、性能与完整原生视觉验收仍未完成。本文继续是 Draft 工程契约，局部落地不自动晋升 Stable。

## 1. 规范来源与冲突处理

- **领域事实与工程能力轨**：真实 Harness 行为、架构、已接受 ADR，以及当前锁定的 gpui-kit 源码/API，决定 Host 事实、依赖边界与可实现能力。gpui-kit 实际版本以 `Cargo.lock` 为唯一 SSOT，本文不手抄版本号。
- **体验契约轨**：[工作台体验与设计边界](workbench-experience.md)决定已确认的体验语义。本文只能映射这些决策，不能自行改写。

同一问题同时涉及两轨时，分别记录事实限制与体验缺口。不得虚构 Host 能力，也不得因工程限制自动降级体验。gpui-kit 未公开提供的组件、插槽或行为不得假定存在；先用最小 probe 确认，再评审本地薄适配或上游改动，并由体验评审决定如何满足已确认契约。

## 2. 身份、视图与本地状态

领域归属、展开和三视图体验以[体验正本](workbench-experience.md)为准；本章只固定逻辑身份、原生导航状态与归属校验。

### 2.1 逻辑身份

实现使用类型化逻辑身份，不以显示名称或可见行号充当持久身份；具体父子组成见 [`identity.rs`](../crates/metis/src/workbench/identity.rs)。文件和默认 Diff 属于 Worktree，Subagent 属于父 Session 且不进入领域 Session、Commands 或 Inbox，审批引用原 ToolCall；Queue item 和本地控件也使用稳定 ID。

`FocusAnchor` 以视图、语义区域和稳定 item ID 描述本地焦点，在渲染时解析为仍存活的原生目标；目标失效时回退到合法区域，不要求 GPUI 元素跨视图永久存活。字段与回退规则见 [`history.rs`](../crates/metis/src/workbench/history.rs)和 [`focus.rs`](../crates/metis/src/workbench/focus.rs)。

### 2.2 三套展开状态

`SessionNavigation` 分别持有 Project、Worktree 和父 Session Subagent 的展开状态，三者互不重置；原生 Tree 只保存投影，不是这些集合的正本。具体字段见 [`navigation.rs`](../crates/metis/src/workbench/session/navigation.rs)。

### 2.3 `MainView` 与路由归属

`MainView` 区分 Session、Worktree Diff 和父 Session 下的只读 Subagent。Diff 的来源 Session 只负责返回导航和焦点恢复，不改变 Diff 的 Worktree 归属；Subagent 路由同样只记录父 Session 来源与返回焦点。类型见 [`history.rs`](../crates/metis/src/workbench/history.rs)。

路由恢复、深链或 Back/Forward 必须先校验目标存在性及父子、Session、Worktree 归属。非法或跨归属目标不得定位隐藏内容，应清理导航意图并回退到合法 Session 视图。

Back/Forward 只恢复视图和本地导航上下文，不回滚 Host 连接、Session 历史、审批决定、执行结果、验证或审查事实。领域事实来自 Host 快照和有序事件；本地视图历史不是另一份可漂移正本。

## 3. 最小语义令牌与主题写路径

### 3.1 单向解析

同一 Rust `MetisThemeSpec` 持有完整 `ResolvedTokens`，并单向生成 GPUI config 与 projection；`ThemeController` 持有当前完整快照并执行写入，产品组件读取该快照，不从 GPUI Theme 反读正本。config 与 projection 都由同一份值派生，没有独立维护的主题值。实际字段、单向写路径和 Light/Dark 配置重载顺序见 [`theme.rs`](../crates/metis/src/workbench/theme.rs)，对应 headless 覆盖见 [`workbench_theme.rs`](../crates/metis/tests/workbench_theme.rs)；本文不复制令牌或 API 清单。

主题切换允许重排，但当前视图、逻辑实体、FocusAnchor、文本选择、滚动锚点、草稿和待处理上下文必须保留。现有 Session/Theme 测试只证明其注入状态下的行为，不代替原生视觉验收。

### 3.2 语义与偏好边界

语义令牌优先复用 gpui-kit，Metis 只补产品独立语义；统计组件的私有样式不扩大为全局 token。Diff 保留原生 renderer，不虚构上游样式插槽。

当前运行时只实现 Light/Dark；高对比、文本缩放、密度和减少动效仍有未完成的实现与验证。后续文本缩放必须同时覆盖正文、metadata、Composer 和代码，重排不得丢失动作，状态也不得只靠颜色表达。

## 4. 组件与依赖边界

### 4.1 只通过 gpui-kit

遵循 ADR-0006，只经 `gpui-kit` 使用重新导出的 GPUI、Base、Component 和 assets，不新增直接 GPUI 依赖。实际组件组合和上游 API 以 [Session 实现](../crates/metis/src/workbench/session.rs)及[组件 probe](../crates/metis/tests)为准；不得把本文或隐藏入口当作稳定公共 API。

### 4.2 Metis 本地基础适配

当前 crate-private 适配包括 [`ThemeController`](../crates/metis/src/workbench/theme.rs)、[`AgentMarkdown`](../crates/metis/src/workbench/markdown.rs)和 [`Disclosure`](../crates/metis/src/workbench/disclosure.rs)，分别负责当前完整主题快照与单向写入、统一不可信 Markdown 策略，以及 controlled disclosure 的可聚焦 summary。职责和 API 以代码及 rustdoc 为准；它们不因单个切片而晋升 Stable。

### 4.3 产品组件职责

最小 Session 切片由 `Workbench` 组合 Navigation、Timeline 与 Composer。`MainViewController` 只拥有本地导航和焦点，Host projection 与 effect 从外部注入，renderer 不伪造领域事实。Navigation 持有三套展开状态；原生 Textarea/MessageScroller 是 draft、selection 与 scroll 实际恢复的唯一 owner，生产路径的 `NavigationState` 只保存视图历史及按 Session 的 FocusAnchor，不复制这些原生状态。具体组件树见 [`session.rs`](../crates/metis/src/workbench/session.rs)。

Worktree Diff 已从 Session Header 接入 Host patch、Unified/Split 原生查看、Offline/空/错误状态与来源返回；独立 [Editor](../crates/metis/tests/workbench_editor.rs)/[Diff](../crates/metis/tests/workbench_diff.rs) probe 仍只验证组件边界，不代表完整冷启动或 OS 原生验收已经完成。reviewed/Feedback/Approval 动作仍未接入产品 Diff。Subagent transcript 尚无 Host 投影，因此继续保留不可交互的返回占位。

## 5. 关键行为契约

领域、三视图、审批、Permission、Composer、Diff 和 Subagent 的体验语义均以[体验正本](workbench-experience.md)为准；本章只保留独立状态轴、原生实现不变量和三域数据边界。

### 5.1 ToolCall 与 Approval 使用独立状态轴

实现不得把相互独立的事实压成一个庞大 enum，至少需要分别表达：

- 连接与快照事实：在线、离线、重连、缓存、Host/Event 代际及最后已知状态。
- 执行事实：未执行、运行、成功、失败、停止、未知。
- 审批请求、提交和决定：请求与待决定/提交中状态、AllowOnce 或 Reject，以及失效、被取代或其他客户端已处理。
- 验证、代码变更和审查：各自独立的 pending、结果或未知，不从一个总状态推导。

普通工具和需审批工具始终使用同一 `ToolCallKey`、renderer 和 output 状态，更新对应 Entity/ElementId，不追加执行卡；Pending/Submitting 诊断与决定后的真实 output 分开。

### 5.2 授权提交与代际

- 当前投影和 `EffectHandler` 均由外部注入；测试中的 `Recorded` 和 `Unavailable` 都不代表 Host 接受。真实 Host 必须以快照和有序事件更新投影，effect 提交前重验目标归属、连接及适用的 Host/Event 代际；旧闭包不得提交，intent 不得被改写成本地 Host 事实。
- `SessionPermissionPolicy::AskEachTime` 和 `AllowOnce` 必须映射到体验正本指定的 Session 与 `ToolCallKey` 作用域，原生视图不得另存一份授权正本；提交前校验 Session/call 归属、当前 Host/Event 代际和审批有效性，旧回答在断连、重连、被取代或其他客户端已决定后不得继续提交。
- Offline/Cached、Stale、Unknown 和 Not replayed 按体验正本投影为不同事实；Host 未确认执行结果时保持 `Execution: Unknown`，不由连接或本地重放行为推断执行结果。

纯 Host 适配器以完整校验后的快照和连续事件原子更新投影；重连快照只重建一次，live append 增量应用，保留的 Timeline 窗口有界。持久 `approval/asked` 与当前 `$events` 代际的唯一匹配请求同时存在时才允许回答；取消、决定、断代、实时候选歧义或容量溢出均失败关闭。由于 `$events.event_id` 不是 durable approval ID，同一 ToolCall 多个 durable 审批代际的未决请求保持 Stale，不按 `callId` 猜测请求的新旧；已确认决定仍按 outcome 投影。`cancelled`/`unavailable` 不映射成用户拒绝，`TOOL_NOT_STARTED`/`TOOL_OUTCOME_UNKNOWN` 也不映射成执行失败。无 `callId` 的 Host 审批、流式 Assistant chunk、历史分页与淘汰后恢复、Model/Effort、统计和 Host Queue 尚未形成产品投影。

### 5.3 Composer 原生连续状态

- 每个 Session 的原生 Textarea/MessageScroller 唯一持有并恢复 draft、selection 与 scroll；Send intent 仅被本地记录时保留 draft，Queue item 已在本地保存文本后可清空 Composer draft。Send/Queue 可用性遵循体验正本。
- Queue 只由本机记录，Host 投影不带队列内容，注入 projection 仅用于 headless 测试播种。每个 Session 的原生 Queue editor 持有本地文本与顺序，后续快照不重置这些编辑。稳定 item ID 同时用于定位、取消和 FocusAnchor 恢复；取消非末项后聚焦相邻存活项，空 Queue 回退 Composer。
- 入队是本地记录而不是提示词提交，Host 在该时刻收不到任何内容。Session 空闲即可派发队首：执行状态为未执行、成功、失败或停止（运行与未知不算空闲），连接在线、无待处理审批，且存在有效模型选择、队首文本非空。每段空闲只派发一项，先进先出，走普通提示词路径；同一空闲期内其他投影更新不会继续派发。
- 派发时条目离开本地队列，因此重连、切换 Session 或重复快照都不会重放已派发的提示词。未派发的条目在重新可提交时继续派发：恢复在线、切回该 Session、模型选择出现或队首补齐文本。传输层未接受时条目留在队首并如实提示未被 Host 接受，不记为已提交；Host 之后拒绝该提示词只产生通知，文本不恢复。
- 派发是 Host 驱动的更新，不把焦点移入其他 Queue 行；只有用户取消才把焦点交给相邻存活行。
- streaming 和审批异步更新不得抢走 draft、其他工具或 popover 的焦点，也不得重置 selection/scroll；jump-to-latest 恢复 tail，按钮即将隐藏且仍持有焦点时先聚焦对应 ToolSummary，再移除或禁用按钮。

### 5.4 三域统计数据边界

Activity 只描述完整 Session 的执行活动与耗时；Usage 只描述 token 用量，不表示货币成本；Context 只描述下一次请求的上下文估算。三域分别使用独立 `Option`；某域缺失时只隐藏该域，不补零、不跨域推导，三域均不可显示时不保留空 dock，摘要与详情共用同一数据域投影。

零分母不得伪造为 0、NaN 或 Infinity；Usage 不得把部分 cache hit 显示成 100%，Context 不得从累计 Usage 反推，也不得强制分类和等于 projected tokens。精确公式、负值钳制、舍入、单位和显示格式以 [`stats.rs` 的 rustdoc 与同文件测试](../crates/metis/src/workbench/stats.rs)为正本，本文不再复制。

### 5.5 Diff 与只读 Subagent 原生不变量

- Diff 和 reviewed/Feedback/Approval 的体验关系遵循体验正本；原生输入中的 patch、路径、行号和 annotation 目标必须来自当前 Worktree 的有效数据，reducer 不得互相补齐这些事实。
- 只读 Subagent 的组件组合只包含 Transcript 投影与来源导航，不创建 Session 动作区；continuable 等投影字段不得改变这个约束。

## 6. 安全边界

安全策略以生产 [`markdown.rs`](../crates/metis/src/workbench/markdown.rs)和[安全测试](../crates/metis/tests/workbench_markdown_security.rs)为正本，本文只保留边界，不维护参数表：

- `AgentMarkdown` 是智能体和工具 Markdown 的统一入口；raw HTML 只按文本呈现，不依赖上游默认值。
- 链接不得自动打开；只有完整 `https` 目标在用户显式激活并确认后才可打开。其他 scheme、相对链接、取消确认或截断目标均不得打开。
- 本切片拒绝所有 Markdown 图片，不得触发网络、文件或 `data:` 解码，也不得回退到不可信来源；后续放行策略另行评审。
- Diff、cwd、command、结构化参数、reason 和审批 ID 都按不可信输入处理；校验数据、归属和代际，不把显示文本拼接成新的 shell 命令。
- Git patch 和文件能力沿用真实 Host 边界。测试输入不得成为放宽路径、授权、图片、链接或队列规则的后门。

## 7. 验证顺序与 probe 退出条件

原生组件测试从 [Session 测试](../crates/metis/src/workbench/session/tests.rs)、[review 测试](../crates/metis/src/workbench/session/review_tests.rs)，以及 [Theme](../crates/metis/tests/workbench_theme.rs)、[Markdown](../crates/metis/tests/workbench_markdown.rs)、[Markdown 安全](../crates/metis/tests/workbench_markdown_security.rs)、[弹层与 Disclosure](../crates/metis/tests/workbench_popups.rs)、[Editor](../crates/metis/tests/workbench_editor.rs)、[Diff](../crates/metis/tests/workbench_diff.rs) probes 进入。Editor/Diff 中的真实 Host 只提供独立组件输入，不能证明 Workbench 产品流程已接入 Host。

测试命令以 [`AGENTS.md`](../AGENTS.md) 为准：先用 scoped nextest 缩小验证范围，再运行完整测试；实现收尾必须通过 `just ci`，不能以 headless probe 或文档检查代替。

### 7.1 纯行为门

验证顺序固定为纯 projection/reducer、生产组件的 headless 集成、真实 Host/OS 验收。失败时先缩小并修复根因；前一层的通过不能代替后一层。纯行为测试已覆盖：

- **三域统计**：独立数据域、隐藏规则、公式与格式边界，详见 [`stats.rs`](../crates/metis/src/workbench/stats.rs)。
- **历史与归属**：Diff/Subagent 的来源恢复、Back/Forward、独立审查事实和只读约束，详见 [`history.rs`](../crates/metis/src/workbench/history.rs)。
- **方向焦点**：区域顺序、动态区域、Overlay scope 与历史锚点恢复，详见 [`focus.rs`](../crates/metis/src/workbench/focus.rs)。
- **Host 投影**：真实协议形状的快照、连续事件、重连窗口、Surface 替换、审批代际与歧义、恢复错误码和有界 Timeline，详见 [`host_projection.rs`](../crates/metis/src/workbench/session/host_projection.rs)。

这些结果只证明本地模型行为，不证明真实 Host 提交或原生交互。产品入口已订阅 Host，局部 Session 与传输测试不能证明分页、淘汰恢复、完整冷启动或性能验收已经完成；仍需真实 Host/OS 验收。

### 7.2 组件 probe 与原生验收状态

既有组件 probe 和 Session 测试已在 headless GPUI 环境运行。以下范围不得扩大解释，未完成的原生事项保持未完成：

1. **Theme / assets**：已有生产 `ThemeController` 的有损投影、Light/Dark 重载、状态保留和选择性图标来源测试；Light/Dark 的真实帧已按第 9 项复核，高对比与文本缩放仍按第 5 项验收。
2. **Focus / Disclosure / Tree / Popover**：已有纯焦点模型及菜单、弹层、Disclosure、Session 的 headless 覆盖。`FocusMove` 尚无产品键位绑定，不能声称方向性键盘主流程已完成；仍需验证真实焦点恢复以及 Tree 重建、Overlay 关闭后的行为。
3. **Streaming / tool identity**：已有流式 Markdown、阅读位置和 Session intent／同一工具卡覆盖；Queue 的本地入队、编辑／取消、空闲派发与失败保留、派发不转移焦点、审批完成及注入重连快照后不重放也已有 headless 覆盖。Queue 派发已接入普通提示词路径，但模型投影为空，产品内尚无可用入口；原生 Composer 经真实 Host 的端到端提交、提交中断连、真实重连代际与其他客户端事件仍未验证。
4. **Markdown security**：已有生产 `AgentMarkdown` 的 raw HTML、链接确认与图片拒绝 headless probe；这不扩展为 OS 桌面集成或原生视觉验收。
5. **200% 中文、长路径、窄布局与 DPI**：未完成。需在 200% 或平台上限验证内容和动作可达、局部滚动及焦点／选区／滚动锚点保留，并覆盖高对比和减少动效。
6. **真实 IME**：未完成。需在 Composer 的流式更新、弹层和 Session 切换中验证 preedit、候选提交、选区与剪贴板；里程碑 0 的 GPUI Kit IME 结果和无头输入结果均不能替代此项。
7. **真实辅助技术**：未完成。先做语义快照，再在目标平台用真实辅助技术验证核心任务、非颜色状态、焦点事件及 Transcript／状态／通知的分离。
8. **有界历史、流式与大文件性能**：参考平台、输入规模、warmed baseline 和延迟／内存／重建预算已预登记在[性能验收测试定义](../crates/metis/tests/workbench_performance.rs)；实际测量未完成。常规 CI 只编译并跳过这些定义，签署前必须实现测量体、在参考平台运行并记录结果，不得以“交互可用”等主观表述代替记录。大文件定义只覆盖 Editor 组件，不能据此关闭受当前 Host 文本上限约束的产品流程验收。
9. **少量受控原生视觉证据**：局部 Session 已完成，完整原生视觉验收仍未完成。2026-11-26 在 NixOS 的隔离 Sway headless Wayland 会话中，使用 Mesa lavapipe Vulkan、固定 CJK 字体和 1 倍缩放，先运行 `direnv exec . cargo build -p metis --bin metis`，再以 `direnv exec . node target/native-workbench-visual.mjs` 启动未入库的一次性 harness（环境变量选择状态和尺寸），由其调用 `grim -o HEADLESS-1 <临时 PNG>`。人工对照已确认原型，覆盖 1440×900 的 Dark/Light Session、1120×760 的审批加 Queue、以及 Offline/Stale；导航层级、审批 cwd/reason/来源入口、Queue、Composer 工具行与统计均可见，Offline 动作禁用。未覆盖原生弹层、成功/拒绝展开、多 Worktree、Diff/Subagent、Queue 空闲派发、焦点环视觉和真实输入。一次性 harness 与 PNG 在审查后删除，headless 断言未充当视觉证据。
10. **完整冷启动**：未完成。产品入口的真实 Host headless 测试已覆盖当前目录的环境准备、项目打开、Session 创建、Online 投影、通知、启动失败、启动中关闭，以及传输／Host／临时状态清理；添加／选择项目、原生提示词与差异审查尚未贯通，局部 Session preview 和这些生命周期测试也不能替代完整流程的 OS 验收。

局部实现的保留偏差已经体验与工程双评审，范围和回收条件如下：

- 阅读轴目前保留 User/Model/Tool 平铺；完整 Turn → Process → Tool 和 Test/Review 结果槽在真实 Host projection 接入时实现，并验证运行、失败、停止与渐进展开，不提前晋升 Stable。
- Diff 已通过 Session Header 的 Worktree 入口接入真实 Host `git/diff`，并按请求 ID 应用结果；Header 真实路径/分支、reviewed/Feedback/Approval 动作与 OS 原生验收仍未完成。Subagent transcript 尚无 Host 投影，因此继续保持只读占位而不开放导航。Queue 已按客户端本地队列实现，空闲时沿普通提示词路径逐条派发，不依赖 Host 队列投影；模型选择仍须等待 Host 侧模型目录，本地选择不冒充 Host 事实。
- Composer 目前按审批/Queue 模块分档并在溢出时局部滚动，尚未实现原型的内容测量与逐级收缩；后续原生自适应布局落地时回收，200%、窄布局和高 DPI 仍按第 5 项验收。
- Permission 详情与侧栏底部工具随对应动作流程接入；主题切换 probe 在正式入口定案后移除或替换。

每项记录实际命令、环境、输入、结果、审查范围和未覆盖限制。截图只用于当次对比审查，不作为交付物保存；视觉证据只补充行为测试，不建立大批 gallery。

## 8. 晋升与 canonical 同步

- 体验语义由 `workbench-experience.md` 持有；实现、API、统计公式和安全策略由代码、rustdoc 与测试持有。行为变化与对应文档在同一变更中同步，不复制细节。
- 本文和局部原生组件保持 Draft。共享组件、token 和交互模式遵循 `Draft → Preview → Stable`；Preview 需要真实使用、行为测试和明确所有权，Stable 还需要持续复用、契约、文档和迁移纪律。代码落地或 headless 通过不自动晋升。
- `product-scope.md` 中的高保真流程和各项关键探针不得因局部实现或单一 smoke 提前勾选；局部 Session 切片不冒充完整冷启动设计。
- 局部实现保持 crate-private。确有上游通用缺陷时单独评审上游修复，不把 Metis 产品组件或领域状态塞进 gpui-kit。
