## Problem

让 root agent 和 sub-agent 的 `tools` 配置支持 include/exclude 集合运算，同时保留现有默认值与列表写法。需求见 [agent-tool-include-exclude](../requirement/agent-tool-include-exclude.md)。

## Scope

In: YAML 数据模型、工具名称/模式展开、include/exclude 集合运算、工具注册重名校验、启动期错误传播和测试。

Out: runtime 工具注入、审批策略、shell allow/deny，以及 MCP 的发现、连接和工具执行逻辑；MCP 工具进入 registry 时的重名校验属于本方案范围。

## Validation Findings

- `app/coda_server/src/agents.rs` 同时拥有 frontmatter 解析、`ToolRegistry` 和 `resolve_tools`，可以在一个模块内封装全部新语义。
- root 当前用 `Option<Vec<String>>` 区分“字段缺省”和“显式空列表”；sub-agent 当前用空列表表达缺省。新模型必须为两者都保留“include 是否出现”的信息。
- `ToolRegistry` 已统一覆盖内置工具与启动时注册的 MCP/`ask_user` 工具，并为前缀模式提供确定性排序。
- `task_output`、`task_kill` 在 `AgentTeam::build` 中另行注入，不经过配置解析，因此天然保持现有行为。

## Alternatives Considered

- 仅接受新的对象写法 `tools: { include, exclude }`：模型最简单，但会无必要地破坏所有现有列表配置，因此不采用。
- 在原列表中使用 `!shell` 一类否定项：写法短，但会把包含、排除和匹配语法揉进字符串，并产生顺序是否影响结果的歧义，因此不采用。
- 先构造 `ToolSpec` 再过滤：能复用部分现有代码，但会构造必然被排除的 spec，也让集合规则依赖对象层；选择先解析名称、完成集合运算，最后统一构造 spec。
- 在枚举默认工具集时才检查 registry 重名：改动更局部，但显式 include 路径未必枚举全部名称，可能继续隐藏冲突；选择在 `insert` 时建立一次性唯一性约束。

## Components

- `ToolSelection`：表示兼容的列表简写或带 `include`/`exclude` 的对象配置，并保留 include 缺省与显式空列表的区别。
- `ToolRegistry`：在注册边界保证内置与预构建工具名称全局唯一，再按稳定顺序枚举、解析全部可声明工具。
- `resolve_tools`：根据 agent 类型提供的默认工具集计算最终名称集合，通过校验后构造 `ToolSpec`。

## Interfaces

```rust
#[derive(Deserialize)]
#[serde(untagged)]
pub enum ToolSelection {
    Include(Vec<String>),
    Rules(ToolRules),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRules {
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
}
```

列表变体是原配置的兼容入口；对象变体允许任一字段缺省。`ToolRules` 是不可信 YAML 进入系统的边界，拒绝未知键，避免拼错 `include` 后意外回退到默认工具集。

```rust
fn resolve_tools(
    registry: &ToolRegistry,
    agent: &str,
    selection: Option<&ToolSelection>,
    default: DefaultToolSet,
) -> Result<Vec<Box<dyn ToolSpec>>, LoadError>;
```

调用方明确传入 `All`（root）或 `Empty`（sub-agent）。函数返回已去重、已排除且顺序稳定的 spec；未知精确名称返回 `LoadError::UnknownTool`，空匹配模式只告警。

```rust
pub enum ToolRegistryError {
    DuplicateToolName(String),
}

pub fn insert(
    &mut self,
    tool: Box<dyn ToolObject>,
) -> Result<(), ToolRegistryError>;
```

这是 MCP/`ask_user` 等外部构造工具进入 registry 的信任边界。若名称与内置工具或已注册的预构建工具重复，立即返回重复名称错误；成功后，名称枚举与 `resolve` 均可依赖 registry 的唯一性约束。

## Data Model

- `Frontmatter.tools` 与 `RootFrontmatter.tools` 均改为 `Option<ToolSelection>`；`None` 表示整个字段缺省。
- `ToolSelection::Include` 等价于对象形式的显式 `include`，包括 `tools: []` 表示显式空集。
- `ToolSelection::Rules` 中 `include: None` 使用调用方传入的原有默认集，`include: Some([])` 使用显式空集；`exclude` 缺省为空列表。
- 所有集合均只包含 `ToolRegistry` 中可声明的工具，不包含自动注入的后台工具。

## Load-Bearing Decisions

- 最终集合固定为 `base - excluded`：base 是显式 include，或 include 缺省时的 agent 原有默认集；exclude 与 include 同时命中时始终优先，不采用顺序敏感规则。
- include/exclude 都相对于整个 registry 校验和展开，而不是相对于当前 base。已注册但不在 base 中的排除项合法且无效果；未知精确名称仍报错。
- 保持现有顺序：显式 include 按配置顺序，单个模式的匹配按名称排序；默认集保持内置工具声明顺序后接预构建工具名称顺序；排除只删除，不重排。
- `ToolRegistry` 在 `insert` 时拒绝所有名称冲突，包括 prebuilt/builtin 和 prebuilt/prebuilt；名称级集合去重只处理同一配置内的重复或模式交叠，不能掩盖 registry 冲突。代价是原先“后注册 prebuilt 覆盖同名 prebuilt”的隐式行为改为启动失败，但错误比静默选错工具更安全。
- 兼容列表和对象两种 YAML 形状只存在于解析边界；后续逻辑统一处理 `ToolSelection`，不把兼容分支扩散到 runtime。

## Risks / Open Questions

- `serde(untagged)` 的错误信息可能比单一结构笼统；应通过解析测试确保非法标量、未知对象键和字段类型错误都会启动失败。
- include 缺省与 `include: []` 很容易在重构中混淆；应先用 root/sub-agent 的矩阵测试锁定语义。
- MCP 工具名称可能因前缀拼接或截断而冲突；registry 注册测试和 workspace 构建错误传播测试需确保冲突清晰暴露，而不是覆盖或去重。

## Implementation Roadmap

- [ ] [配置边界] 引入 `ToolSelection`/`ToolRules` 并接入两类 frontmatter
  - Purpose: 保留列表兼容性以及缺省/显式空集的区别
  - Verification: YAML 解析测试覆盖列表、对象、仅 exclude、空 include 和非法键
- [ ] [核心逻辑] 将模式展开改为名称级解析，并实现默认集、include、exclude 的集合运算
  - Purpose: 在构造工具前集中完成校验、去重、优先级和稳定排序
  - Verification: 单元测试覆盖精确名称、前缀、重叠、未命中模式及已注册但不在 base 的排除项
- [ ] [registry 约束] 让 `ToolRegistry::insert` 拒绝与 builtin 或既有 prebuilt 重名的工具，并向 workspace 启动路径传播错误
  - Purpose: 保证名称级运算不会静默隐藏或替换不同的工具实现
  - Verification: 回归测试分别覆盖 prebuilt/builtin、prebuilt/prebuilt 冲突以及正常注册
- [ ] [集成] root 以 `All`、sub-agent 以 `Empty` 调用统一解析器，并更新模块说明
  - Purpose: 接通真实 agent team 构建路径且不影响后台工具注入
  - Verification: team 测试覆盖两类 agent 的字段缺省、仅 exclude 及 include+exclude
- [ ] [配置文档] 更新 `AGENTS.md` 的 Agent Configuration 说明和示例
  - Purpose: 让配置者能看懂列表简写、对象写法、root/sub-agent 默认集差异、exclude 优先级和后台工具例外
  - Verification: 文档示例覆盖仅 include、仅 exclude、include+exclude，并与解析测试中的语义一致
- [ ] [回归] 执行项目规定的 Rust 检查
  - Purpose: 确认格式、lint、常规测试和 feature-gated storage targets 均可构建
  - Verification: `cargo fmt --check`、`cargo clippy`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`
