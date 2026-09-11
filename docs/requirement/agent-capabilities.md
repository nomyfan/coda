## Problem

目前 background 由 session 是否有任务存储决定，并向所有 agent 注入配套工具；PTC 则由普通 `tools` 中的 `run_javascript` 间接开启。另外，根 agent 的 tools 默认全部启用，子 agent 默认空，通用子 agent 需要重复列出常用工具。
引入每个 agent 的 `capabilities` 声明，由能力统一决定配套工具及执行入口是否可用；普通工具继续由 `tools` 选择。根 agent 与子 agent 统一默认启用全部普通工具和 capabilities，各自显式配置覆盖默认值，使共享子 agent 的配置保持确定。

## Scenarios

- 配置根 agent 或子 agent 时，通过 `capabilities: [background, ptc]` 启用两项能力，无需逐个列出配套工具。
- 配置只做同步工作的 agent 时，禁用 background 后，不提供后台执行参数及任务查询、停止工具，即使同一 session 的其他 agent 启用了 background。
- 为已有文件工具的 agent 启用 PTC 后，可以通过 JavaScript 组合调用其中符合现有 PTC 规则的工具；未授予或需要审批的工具不会因此变得可调用。
- 通用 agent 省略 tools 和 capabilities 即使用全部默认项；专用 agent 可以显式选择能力、列出所需工具，或仅用 `tools.exclude` 从全部普通工具中排除不需要的项。
- A、B 都可调用 C 时，C 按自身配置和统一默认值获得工具及能力；A、B 的工具或能力配置差异不影响 C。
- 根 agent 可以将没有 background capability 的子 agent 作为后台任务调用；该子 agent 自己能否启动后台 shell，取决于它生效的能力配置。

## Scope

In:

- 根 `.coda/agents/AGENT.md`、子 agent frontmatter 和 Rust `AgentSpec` 支持 capabilities，首批提供 `background`、`ptc`。
- capabilities：所有 agent 省略声明时默认启用全部已支持的能力（首批 background、ptc）；显式列表完整替换默认值，`[]` 关闭全部能力。
- tools 和 capabilities 分别按每个 agent 自己的声明及统一默认值确定；父 agent 的选择和排除规则不会传播给子 agent，共享或多层调用均遵循此规则。
- 配套工具完全由 capabilities 管理，不参与普通 `tools.include/exclude`；`run_javascript` 从普通可声明工具中移出。显式将能力工具写入普通工具选择时，应报出清晰的配置错误。
- 普通 tools：所有 agent 省略 tools 或 include 时，均以工具注册表中的全部可声明工具（含已注册 MCP 工具）为基础，再应用自己的 exclude。
- 显式 tools 列表或 tools.include 完整替换默认值，再应用 tools.exclude；`tools: []` 或 `include: []` 选择空集合。普通工具的选择与 capabilities 的选择独立，前者的空集合不会关闭后者。
- `background` 提供 `task_output`、`task_kill`，为已授予的 `shell` 开放后台参数，并为实际 session 根进程已有的子 agent 调用开放后台参数；不额外授予 `shell` 或子 agent。
- `ptc` 统一提供 `run_javascript`、`list_javascript_tools`；继续使用现有可调用工具范围、审批过滤和调用快照规则，没有合格工具时不向模型提供 PTC 入口。
- 更新受影响的配置示例、配置文档、默认 system prompt 和 prompt 模板，使指引与实际可用能力一致。

Out:

- 增加其他 capabilities、第三方能力注册机制、运行时热切换或前端配置界面。
- 沿调用链继承 tools 或 capabilities，调整 subagents 调用拓扑，或继承父 agent 的委派目标。
- 扩展 PTC 可调用的工具种类，或改变后台任务的通知、取消、持久化、重启和 UI 查看语义。

## Constraints

- capability 控制 agent 可以使用哪些能力，不替代工具审批；原有权限规则继续生效。
- background 仍要求 session 的任务存储可用；不可用时不暴露后台入口及配套工具。未启用或不可用时，执行阶段也必须拒绝后台请求，不能仅隐藏 schema。
- 只有实际 session 根进程可以发起后台子 agent 调用；子 agent 即使启用 background，也只能据其已有工具启动后台 shell。
- 一个 agent 未启用 background，不影响其他 agent 的能力配置，也不关闭 session 的任务通知、清理和用户查看功能。
- 项目允许 breaking changes：无需兼容通过 `tools: [run_javascript]` 开启 PTC；旧子 agent 省略 tools 将从默认空变为全部可声明工具，需同步检查配置示例并明确记录此变化。

## Success Criteria

- 根 agent 和子 agent 使用相同默认值，显式空列表及单项和双项覆盖均有明确且可验证的行为；未知 capability 在配置加载或构建时报告错误。
- tools 的省略、仅 exclude、显式 include 和空列表行为均有覆盖；A、B 配置不同时，共享 C 的普通工具及声明的 capabilities 始终按 C 自身配置确定，多层调用同样成立。
- 未启用 background 的 agent 无后台参数、任务查询或停止工具，绕过工具 schema 的后台请求也无法执行；启用且资源可用时配套入口完整提供。
- 未启用 PTC 的 agent 无 JavaScript 调用或发现入口；启用后只提供现有规则允许的工具，不扩大普通工具授权。
- 普通 `tools` 选择与 capability 声明职责清楚，能力工具不能被普通工具选择意外开启或拆散。
- 混合能力的 agent 团队可正常工作，包括根 agent 将无 background 的子 agent 作为后台任务调用、以及子 agent 不能发起后台委派。
- 配置示例和 prompt 指引与上述行为一致，相关配置、工具暴露及运行时校验得到测试覆盖。
