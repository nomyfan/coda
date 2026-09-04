## Problem

Agent 的 `tools` 配置目前只能列出允许使用的工具。当 agent 需要“除少数工具外的全部工具”时，必须复制并维护完整工具列表；新增内置工具或 MCP 工具后，这份列表还会过时。需要让 root agent 和 sub-agent 都能通过 include 与 exclude 组合得到最终工具集。

## Scenarios

- 配置 root agent 时，只排除少数危险或无关工具，其余当前及以后注册的工具自动可用。
- 配置者可以只声明 exclude；未声明 include 时，仍使用该 agent 原有的默认工具集。
- 配置者先通过名称或前缀模式包含一组工具，再从中排除个别工具；同一工具同时命中时，exclude 生效。
- 现有使用 `tools: [read_file, grep]` 的配置无需修改，得到的工具集不变。

## Scope

In:

- `tools` 支持 `include`、`exclude` 两部分，并应用于 root agent 和 sub-agent。
- 现有列表写法继续作为 include 的简写。
- include 与 exclude 都支持精确工具名和现有的尾部 `*` 前缀模式。
- 显式对象写法中可以只声明 include 或 exclude。省略 include 时沿用原有默认：root agent 以全部已注册工具为基础，sub-agent 以空集为基础；省略 exclude 时不排除任何工具。
- 最终工具集为 include 的展开结果减去 exclude 的展开结果，并保持去重。

Out:

- 不改变 `task_output`、`task_kill` 等后台能力工具的自动注入机制；它们仍不可在 `tools` 中声明或排除。
- 不改变工具审批、权限模式或 shell allow/deny 规则。

## Constraints

- `tools` 字段或其中的 include 缺省时都保持现有默认：root agent 获得全部已注册工具，sub-agent 不获得工具。
- 精确工具名与前缀模式沿用现有校验：未知精确名称导致启动失败，未匹配任何工具的模式只记录警告。
- MCP 等运行时注册工具必须参与 include 与 exclude 的匹配。

## Success Criteria

- 列表形式与等价的 `include` 对象形式产生相同工具集，现有 agent 配置行为不变。
- root agent 使用 `tools: { exclude: [...] }` 时得到“全部工具减去排除项”的结果；sub-agent 使用同样写法时仍以原有空工具集为基础，因此结果为空。
- 同时配置 include 与 exclude 时，只有被 include 且未被 exclude 的工具可用，exclude 优先。
- 精确名称、前缀模式、重复及交叠项均按上述规则稳定解析。
- 自动注入的后台能力工具和现有权限控制行为不受影响。
