## Problem

`ReasoningEffort` 是固定的 6 值 enum（none/minimal/low/medium/high/xhigh），对应 OpenAI 标准档位。但不同 provider 实际支持的档位各异——有些支持 OpenAI 映射，有些不支持。当前的固定 enum 无法表达非标准档位，配置时只能从中选子集。

## Scope

In: `ReasoningEffort` 从 enum 变为任意字符串，贯穿 core → openai → agent → server → wire → 前端全链路。

Out: 档位映射到多个参数（temperature 等）；自定义显示 label（如果需要可以后续加）。

## Assumptions

- `"off"` 是"关闭思考"的特殊值。`Option<String>` 中外层 `None` 表示不设置（走 provider 默认），`Some("off")` 显式关闭。选 `"off"` 而非 `"none"`，因为语义更直白、跟 UI 显示一致、且不跟 Rust `Option::None` / TS `null` 撞概念。
- **`"off"` 必须显式出现在 `reasoning_efforts` 数组中才可用。** 不是所有 reasoning model 都支持关闭思考——不在数组中就不出现在 UI 里，`normalize_reasoning_effort` 也会拒绝它。
- TOML 配置中 `reasoning_efforts` 仍然是字符串数组，只是不再校验是否属于固定枚举。现有配置 `["low", "medium", "high"]` 无需改动。
- AGENT.md frontmatter 的 `reasoning_effort` 同样变为任意字符串，启动时仍校验是否属于该 model 的已配置档位。
- 不支持关闭思考的 model 不会出现 `"off"` 以外的问题——因为 `"off"` 不在它的 `reasoning_efforts` 里，前端不渲染 Off 选项，server 拒绝 `"off"` 请求。

## Alternatives Considered

**保留 enum + 增加一个 `Custom(String)` 变体。** 可以兼容现有代码中的 pattern match（`None`/`Minimal`/…），但引入了两种表示——标准值和自定义值——在比较、序列化、传递时都要分别处理，复杂度不值得。直接用 `String` 更简单统一。

## Components

- **`coda_core::llm`** — `ReasoningEffort` enum 删除，`ChatCompletionRequest.reasoning_effort` 变为 `Option<String>`。
- **`coda_openai`** — 不再使用 `async_openai` 的 `ReasoningEffort` enum 和 `req.reasoning_effort()` setter；改为序列化后直接往 JSON body 注入 `"reasoning_effort"` 字符串字段（已有先例：DeepSeek 的 `thinking` 字段）。DeepSeek 分支按 `"off"` vs 非 `"off"` 判断 thinking on/off。
- **`coda_agent`** — `ModelProfile.reasoning_effort` 从 `Option<ReasoningEffort>` 变为 `Option<String>`。
- **`coda_server::config`** — `ModelConfig.reasoning_efforts` 从 `Vec<ReasoningEffort>` 变为 `Vec<String>`；`parse_model_reasoning_efforts` 不再校验值是否属于固定集合，只要求是字符串数组。
- **`coda_server::agents`** — `AgentFile.reasoning_effort` 从 `Option<ReasoningEffort>` 变为 `Option<String>`。
- **`coda_server::wire`** — `ProviderInfoWire.reasoning_efforts` 从 `Vec<ReasoningEffort>` 变为 `Vec<String>`；snapshot/params 中的 `reasoning_effort` 同理。
- **`coda_server::hub`** — 所有 `Option<ReasoningEffort>` 签名变为 `Option<String>`。
- **`coda_server::bin::server`** — `ProviderHandle.reasoning_efforts` 变为 `Vec<String>`；`normalize_reasoning_effort` 简化——不再对 `"off"` 做特殊分支，统一检查 `configured.contains(&effort)` 即可（`"off"` 如果不在列表中自然被拒绝）。
- **Frontend** — `ReasoningEffort` 类型从 union literal 变为 `string`；`model-preferences.ts` 的 `isModelPref` 校验放宽为 `typeof string`；`model-selector.tsx` 不再硬编码 Off 选项，统一遍历 `reasoning_efforts` 渲染（`"off"` 显示为 "Off"）。

## Interfaces

```rust
// coda_core::llm — 变更后
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Outer `None` = leave provider default; Some("off") = explicitly off.
    pub reasoning_effort: Option<String>,
}
```

```rust
// coda_openai — 注入逻辑（替代 map_effort + req.reasoning_effort()）
// 序列化 req 为 JSON body 之后：
if let Some(effort) = &reasoning_effort {
    let should_set = match kind {
        ProviderKind::Generic => true,
        ProviderKind::Deepseek => effort != "off",
    };
    if should_set {
        body["reasoning_effort"] = serde_json::Value::String(effort.clone());
    }
}
```

```typescript
// protocol.ts — 变更后
export type ReasoningEffort = string;
// ProviderInfo.reasoning_efforts: string[]
```

```tsx
// model-selector.tsx — 变更后
// 不再硬编码 <SelectItem value="off">Off</SelectItem>。
// 统一遍历 efforts，"off" 显示为 "Off"，其他首字母大写。
function effortLabel(effort: string): string {
  if (effort === "off") return "Off";
  return effort.charAt(0).toUpperCase() + effort.slice(1);
}
```

## Data Model

无新实体。`ReasoningEffort` 从 enum 退化为 `String`/`string`，类型约束从编译期转到运行期（config 声明了合法值集合，`normalize_reasoning_effort` 在运行时校验）。

## Load-Bearing Decisions

1. **`"off"` 是显式配置的、有语义的特殊值。** `"off"` 统一表示"关闭思考"，但必须出现在 model 的 `reasoning_efforts` 数组中才生效。不支持关闭思考的 model 不列 `"off"`，UI 就不会渲染 Off 选项，server 也会拒绝客户端发来的 `"off"`。这是唯一一个有语义的字符串——其他一律透传给 provider API。

2. **不通过 `async_openai` 的类型化 API 设置 reasoning_effort。** 已有的 `byot` + JSON 注入模式完美适配任意字符串，不需要绕道 `OpenAIReasoningEffort` enum。删除 `map_effort` 函数。

3. **TOML 格式不变。** `reasoning_efforts = ["low", "medium", "high"]` 继续工作。只是解析端不再校验值是否属于固定集合。

## Risks / Open Questions

- **低风险：编译期类型安全降低。** `Option<ReasoningEffort>` 变为 `Option<String>` 后，拼写错误不会被编译器抓到。运行时校验（config 解析 + `normalize_reasoning_effort`）是唯一防线，但这跟 model id、provider id 等已有字符串字段的处理方式一致。

## Implementation Roadmap

- [x] [core] 删除 `coda_core::llm::ReasoningEffort` enum，`ChatCompletionRequest.reasoning_effort` 改为 `Option<String>`
  - Purpose: 解除固定枚举限制，允许任意字符串
  - Verification: `cargo check -p coda_core`（其他 crate 此时会报错，预期内）

- [x] [openai] 删除 `map_effort` 函数和 `OpenAIReasoningEffort` import，改为 JSON body 注入；DeepSeek 分支按 `"off"` 判断
  - Purpose: 透传任意字符串到 provider API
  - Verification: `cargo check -p coda_openai`

- [x] [agent] `ModelProfile.reasoning_effort` 改为 `Option<String>`
  - Purpose: agent 层适配
  - Verification: `cargo check -p coda_agent`

- [x] [server] config/agents/wire/hub/server.rs 全部从 enum 切到 `String`：`ModelConfig.reasoning_efforts` → `Vec<String>`，`parse_model_reasoning_efforts` 去掉 match 校验，`ProviderHandle`/`ProviderInfoWire`/snapshot params/hub 签名全部适配
  - Purpose: 服务端全链路贯通
  - Verification: `cargo clippy && cargo test`

- [x] [frontend] `ReasoningEffort` 类型改为 `string`，`model-preferences.ts` 校验放宽，`model-selector.tsx` 用首字母大写显示
  - Purpose: 前端适配
  - Verification: `pnpm --filter coda-web lint && pnpm --filter coda-web test`

- [x] [example] 更新 `coda-server.example.toml` 注释说明可以使用任意字符串
  - Purpose: 文档
  - Verification: 示例可正常解析
