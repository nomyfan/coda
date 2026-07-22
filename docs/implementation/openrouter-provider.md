## Problem

让静态配置的 OpenRouter 模型在 Coda 现有 Chat Completions Agent 循环中可靠处理推理、工具续接、usage 和流内错误。

需求见 [`../requirement/openrouter-provider.md`](../requirement/openrouter-provider.md)。

## Scope

In: `kind = "openrouter"`、文本与图片输入、可选的每模型输出预算、OpenRouter reasoning 请求与响应、跨轮 reasoning 续接、流内错误、会话模型绑定、静态模型示例与回归测试。

Out: 模型自动发现、Responses/Messages API、OpenRouter 托管工具与高级路由、xAI/Moonshot/智谱直连、非文本输出，以及会话中途切换模型/provider 后的混合历史兼容。reasoning effort 仍可在同一模型声明的档位内切换。

## Assumptions

- 用户把 base URL 配为 Chat Completions 根路径（通常是 `https://openrouter.ai/api/v1`），Coda 不自动改写区域或路由域名。
- OpenRouter 负责消化上游模型差异；Coda 不按 `x-ai/*`、`moonshotai/*`、`z-ai/*` 写模型名分支。
- `reasoning_efforts` 是部署者维护的静态能力声明：包含 `off` 才允许关闭；`off` 在 OpenRouter 请求中映射为 `none`。空列表统一表示“不提供 reasoning 控制”，既可用于非推理模型，也可用于始终推理但没有可调档位的模型；adapter 省略 reasoning 参数并依赖模型默认行为。
- Coda 不读取 OpenRouter 模型目录，也不检测静态 capability 随远端模型变化产生的漂移；部署者负责让配置与当前模型能力一致。
- 本期只支持文本输出。配置进来的模型不会产生图片、文件、音频或视频输出，这是部署前提，不是 Coda 能从静态配置验证的保证。
- session 绑定的 provider/model 在其持久化生命周期内保持可用；静态 sub-agent model override 不会在保留旧 session 的同时改指向另一模型。绑定模型缺失时恢复明确失败，不回退到默认模型。
- 流中错误前已经推送给 UI 的 partial content/reasoning 只是临时展示，不写入 assistant 历史和 checkpoint；重试从包含当前 user message、但不包含失败 partial assistant 的最后一份完整历史开始。
- 实施第一步必须具备可调用 Grok 4.5、Kimi K3 和 GLM 5.2 的 OpenRouter 凭据与足够额度；拿不到三个模型的真实 SSE 时暂停核心实现，人工构造 fixture 不能替代这项风险验证。
- 不读取旧 session 是可接受的，因此可以扩展消息持久化结构而不做迁移。

## Validation Findings

- 问题：`reasoning_details` 流式对象是否需要按 `index`/`id` 做字段归并。方法：2026-07-22 使用同一工具 schema，分别调用 `x-ai/grok-4.5`（low）、`moonshotai/kimi-k3`（low）和 `z-ai/glm-5.2`（high），保存真实 SSE，并把完整 detail 序列原序放入 assistant 工具调用消息后提交 continuation。结果：三次首轮和三次 continuation 均为 HTTP 200 且无流内错误；同一 index 会对应多个完整的增量 detail 对象，Grok 还在 summary chunks 后给出独立 encrypted block。原序回传的 26、100、11 个对象分别被三家接受。影响：确认只按到达顺序追加对象，不按 index/id 合并、去重或改写；OpenRouter `delta.reasoning` 归一化到内部 `reasoning_content`，原始 details 单独持久化。
- 问题：三个样本模型当前如何声明 reasoning capability。方法：2026-07-22 查询已认证的 OpenRouter `/api/v1/models` 目录。结果：Grok 4.5 `mandatory=true`，efforts 为 high/medium/low；Kimi K3 `mandatory=false`、默认开启，efforts 为 max/high/low；GLM 5.2 `mandatory=false`，efforts 为 xhigh/high。影响：样本配置仅给 Grok 省略 `off`，Kimi/GLM 增加 `off`；纠正需求调研阶段“Kimi 始终推理”的过时假设。运行时仍不读取目录。
- 问题：是否需要替换 HTTP 客户端。方法：检查本地 `async-openai 0.40.2` 的 BYOT 实现。结果：请求可以发送任意 JSON，SSE data 可以反序列化为自定义响应类型，且库已处理注释事件和 `[DONE]`。影响：继续复用现有客户端，只扩展 wire codec。
- 问题：OpenRouter 数据当前会怎样。方法：对照 `ReasoningStreamResponse` 与 OpenRouter schema。结果：Serde 会忽略 `reasoning`、`reasoning_details` 和顶层 `error`；流内错误带空 delta 时最终成为空成功消息。影响：响应超集和终止校验必须在 provider adapter 内完成。
- 问题：OpenRouter 非 2xx 错误能否直接沿用 SDK 的错误类型。方法：通过 Rust adapter 在线测试 Kimi K3 时捕获真实 429。结果：OpenRouter 返回数值型 `error.code`，而 `async-openai 0.40.2` 的通用错误类型期望字符串，导致错误先落入 `JSONDeserialize`。影响：OpenRouter 方言会从 SDK 保留的原始响应体把数值 `error.code` 归一化为 `status_code`，同时恢复 `metadata.error_type` 和 `message`；真实限流不再退化成不透明的 JSON 反序列化错误。
- 问题：原有 `StreamingError` 是否应承载 DeepSeek 等 provider 的 HTTP/API 错误。方法：检查 `async-openai 0.40.2` 的 HTTP 状态检查和 SSE 解码边界。结果：非 2xx 响应在创建 stream 时表现为 `ApiError`，或在错误体不符合 SDK schema 时表现为 `JSONDeserialize`；stream item 上的 `JSONDeserialize` 则是 HTTP 200 后的 SSE payload 解码失败。影响：所有方言的非 2xx/API 拒绝统一归为 `ProviderError`，流中 JSON 解码失败归为 `InvalidResponse`；原 variant 重命名为 `TransportError`，只承载网络传输、SSE framing 和其他 SDK 基础设施错误。
- 问题：结构化推理放在哪里。方法：检查 checkpoint 路径。结果：`StoredCheckpoint` 直接持久化 `Vec<Message>`。影响：把 continuation 挂在 `AssistantMessage` 上即可自然覆盖工具轮次、重启恢复和子 Agent，无需另建存储表。
- 问题：重启后如何保证仍使用原模型。方法：检查 session metadata、`open_session`、dashboard model preference 与 `set_model`。结果：metadata 当前不保存模型，历史会话会用 workspace 最近选择或默认模型重开，空闲 session 也允许保留历史切换模型。影响：“用户不主动切换”不足以保证 continuation 安全；必须由服务端持久化并执行模型绑定。
- 问题：metadata 新字段能否由现有写入路径安全保留。方法：检查 `initialize_session`、`rename_session` 和原子写盘实现。结果：当前 rename 虽先读取 metadata，随后仍用只含 `name` 的新对象覆盖整个文件；并发写入只靠同一把锁串行，尚无统一的 read-modify-write 契约。影响：binding、name 和 effort 必须属于同一个聚合实体，所有 mutation 在同一锁下读取、修改并原子替换完整实体。
- 问题：是否继续维护 metadata 的自定义原子写实现。方法：对比现有“同目录临时文件 → `sync_all` → rename”与 [`atomic-write-file 0.3`](https://github.com/andreacorbellini/rust-atomic-write-file)。结果：两者高层流程相同，但 crate 在 Unix 使用 directory fd 与 `openat`/`renameat`，并有 Linux kernel panic crash tests；其 MSRV 1.85 兼容项目固定的 Rust 1.95。代价是同步 API 需要 `spawn_blocking`，且会与现有 `nix 0.31` 并存一个 `nix 0.30`。影响：采用该 crate 作为底层原子替换原语，把文件系统细节交给专用实现；metadata 聚合锁、read-modify-write 和 runtime 提交顺序仍由 Coda 负责。
- 问题：dashboard 是否需要新的 capability 系统。方法：检查 model catalog 与 selector。结果：现有 UI 已支持任意 effort 列表、默认 effort 和图片 gating。影响：只需服务端继续输出静态声明，不新增前端协议字段。
- 基线：`cargo test -p coda_openai` 当前 8 个测试全部通过。

## Alternatives Considered

- 独立实现 `OpenRouter: LLMProvider`：vendor 边界直观，但会复制消息/tool/usage 编解码；由于 `LLMProvider::stream` 返回 `impl Stream`，server 还需引入新的枚举或对象安全封装。选择保留一个深的 OpenAI-compatible adapter，用方言策略集中差异。
- 把 reasoning request key、response key、replay key 全部做成用户可配 capability matrix：能覆盖更多网关，但把协议组合合法性推给部署者，启动校验和支持成本更高。选择公开稳定的 `ProviderKind`，wire 差异留在 crate 内。
- 只持久化 reasoning 字符串：改动小，但会丢失签名、加密块和顺序，无法满足工具续接。选择“可见文本 + 带格式标签的原始 continuation”。
- 把完整 SSE chunk 原样写入 session：保真但存储膨胀，并把流式分片格式泄漏到 core。选择在 adapter 内合并为一份可回传的完整 `reasoning_details` 数组。
- 按 `index`/`id` 对 detail 做字段级归并：可能适配某些分片形态，但缺少真实 SSE 证据，而且可能改变带签名或加密块的原始序列。选择先按流到达顺序和 chunk 内数组顺序保留 detail 对象；若三模型真实样本证明存在对象内部字段分片，先修订本设计并定义经验证的最小组装规则，再进入核心实现。
- 依赖前端每次重开历史 session 时提交原始模型：无需新增持久化，但浏览器偏好、默认模型和非 dashboard 客户端都可能破坏约定。选择把 provider/model 和最新 reasoning effort 写入 session metadata，由服务端作为恢复时的权威来源。
- 继续维护基于 `tokio::fs`、UUID 临时文件和 rename 的自定义原子写：没有新依赖且保持全异步，但要由项目持续维护异常清理、目录变化和平台文件系统细节。选择引入 `atomic-write-file 0.3` 的默认 features，并在 blocking pool 中调用；不启用 Linux-only `unnamed-tmpfile`，避免依赖 `/proc` 且保持 Linux/macOS 行为接近。
- 继续让缺省 `max_completion_tokens` 回退 10,000：配置兼容但仍暗中改变 provider 行为。选择可选字段；存在时显式发送，不存在时完全省略并采用 provider 默认值。
- 强制每个模型填写 `max_completion_tokens`：预算最明确，但对没有调优需求的模型增加样板配置，并无谓破坏所有现有配置。选择非必填，同时在示例中为高推理模型演示显式预算。

## Components

- `coda_core::llm`：定义 provider-neutral 的可见 reasoning、不可见 continuation 和结构化 provider error。
- `coda_openai::OpenAICompatible`：继续实现唯一的 OpenAI-compatible `LLMProvider`，隐藏消息编码、SSE 解码和 completion 归并。
- `coda_openai::ProviderDialect`：集中 `generic`、`deepseek`、`openrouter` 的请求注入、reasoning 提取/回传、usage 与错误解释；调用方不接触字段名差异。
- `coda_openai::CompletionAccumulator`：按流顺序归并文本、并行工具参数和 usage，并有序收集 reasoning details，完成时产出一个合法 `AssistantMessage`。
- `coda_server` provider catalog：验证静态 TOML，构造共享 provider client，并把模型的输出预算带入 `ModelProfile`。
- `coda_server` session metadata：在 session 首次打开时持久化 provider、model 和 reasoning effort；恢复时解析绑定模型，并拒绝已有绑定改指向另一 provider/model。完整聚合实体通过 `atomic-write-file` 原子替换。
- `coda_web` model selector：draft session 可选择 provider/model；session 打开后禁用模型切换，但在空闲时继续允许调整该模型支持的 reasoning effort。

## Interfaces

```rust
pub enum ProviderKind { Generic, Deepseek, OpenRouter }

pub struct ReasoningContinuation { /* format-tagged, immutable JSON payload */ }
impl ReasoningContinuation {
    pub fn try_new(format: impl Into<String>, payload: Value) -> Result<Self, String>;
    pub fn payload_for(&self, format: &str) -> Option<&Value>;
}
```

`ReasoningContinuation` 是持久化边界：构造和反序列化时验证非空 format 与 payload 基本形状；具体方言在回传前再验证自己拥有的 schema。未知 format 被安全保留但不会发送给不认识它的 provider。

```rust
pub struct AssistantMessage {
    // existing fields...
    pub reasoning_content: Option<String>,
    pub reasoning_continuation: Option<ReasoningContinuation>,
}

pub struct ProviderError {
    /// Static provider id from `[[providers]].id`.
    pub provider_id: String,
    /// Rejected request's HTTP status, or the equivalent in-band status.
    pub status_code: Option<u16>,
    /// OpenRouter's stable classification from `error.metadata.error_type`.
    pub error_type: Option<String>,
    pub message: String,
}
```

`reasoning_content` 只服务于事件/UI；`reasoning_continuation` 只服务于协议续接。`StreamError` 新增 `InvalidRequest` 区分本地请求编码失败，并新增携带 `ProviderError` 的分支；响应解析错误继续使用 `InvalidResponse`，agent runtime 仍可通过 `Display` 输出用户可读错误。

```rust
impl ProviderDialect {
    fn encode_request(&self, request: ChatCompletionRequest) -> Result<Value, StreamError>;
    fn reduce_chunk(
        &self,
        chunk: CompatibleStreamResponse,
        completion: &mut CompletionAccumulator,
    ) -> Result<Vec<LLMStreamEvent>, StreamError>;
}
```

这是外部 API 响应的信任边界：校验 error envelope、reasoning details、tool-call chunk 和 usage；下游只接收规范化事件或明确错误。

OpenRouter 方言在该边界完成 wire-to-core 映射：`delta.reasoning` 优先、`delta.reasoning_content` 作为兼容别名，二者都归一化为 `AssistantMessage.reasoning_content`；`delta.reasoning_details` 进入 `reasoning_continuation`。回传工具轮次时，有 continuation 就发送 `message.reasoning_details`，否则把内部明文 reasoning 发为 `message.reasoning`。请求侧的 `reasoning: { effort }` 只控制新一轮推理强度，与这两个响应字段分开处理。

```rust
pub struct ModelConfig {
    // existing fields...
    pub max_completion_tokens: Option<u32>,
}
```

配置解析边界在字段存在时要求该值大于 0 且不超过 `context_window`；通过 `ProviderHandle` 原样进入 `ModelProfile`。这只是静态 sanity check，运行时可用输出仍取决于 context window 减去实际 prompt token 数。`None` 使请求省略该参数，删除 server 中的固定 `Some(10_000)`。

```rust
pub struct SessionModelBinding {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
}

pub struct SessionMetadata {
    pub name: Option<String>,
    pub binding: SessionModelBinding,
}

pub struct InitializedSession {
    pub metadata: SessionMetadata,
    pub created: bool,
}

impl WorkspaceStorage {
    /// Return the existing aggregate, or atomically create it with the requested binding.
    pub async fn initialize_session(
        &self,
        session_id: &str,
        requested_binding: SessionModelBinding,
    ) -> Result<InitializedSession, SessionMetadataError>;

    /// Change only the name while preserving the binding.
    pub async fn rename_session(
        &self,
        session_id: &str,
        name: Option<&str>,
    ) -> Result<Option<String>, SessionMetadataError>;

    /// Change only effort when the durable provider/model matches the caller's expectation.
    pub async fn update_reasoning_effort(
        &self,
        session_id: &str,
        expected_provider_id: &str,
        expected_model_id: &str,
        effort: Option<&str>,
    ) -> Result<SessionModelBinding, SessionMetadataError>;
}
```

`SessionMetadata` 是 name、provider、model 和 reasoning effort 的唯一聚合实体，对 `generic`、`deepseek` 和 `openrouter` 采用同一规则。三个 mutation 都使用现有的同一把 metadata 锁，在锁内读取完整实体、只修改自己拥有的字段，再原子替换完整实体；不得从局部字段重新构造 metadata。底层写入使用 `atomic-write-file 0.3` 的默认 features，序列化后把 `AtomicWriteFile::open`、`write_all` 和 `commit` 整体放进 `tokio::task::spawn_blocking`，避免 `sync_all` 阻塞 Tokio worker。不开启 `unnamed-tmpfile`。该 crate 只拥有单文件替换，不拥有聚合并发或内存状态事务。首次 `open_session` 从客户端的 `{provider_id}:{model_id}` selection key 解析、校验并原子创建；后续打开忽略浏览器的 workspace model preference，以持久化 binding 构造 runtime，并在 snapshot 中返回权威选择。绑定模型不再存在或 effort 已不受支持时返回明确错误，不静默选择默认值。缺失 binding 但已有历史属于不支持的旧持久化格式。

现有 `set_model` RPC 保留，但语义收紧：session 空闲时，相同 provider/model 可以更新并持久化 `reasoning_effort`；provider/model 不同则对已绑定 session 返回 model-locked 错误；运行中的 session 继续拒绝任何调整。effort 更新的提交顺序固定为：(1) 在不改变 live state 的情况下构造 replacement runtime；(2) 调用 `update_reasoning_effort` 原子写入完整 metadata；(3) 在 hub 已串行化的 session command 内无失败地替换 live runtime；(4) 关闭旧 runtime 并向客户端确认。replacement 构造失败或写盘失败时丢弃 replacement，live selection 与原 metadata 都保持不变；一旦写盘成功，后续内存替换不得包含可失败操作。这样不需要新增只转发 effort 的 RPC。

## Data Model

- 一个 `AssistantMessage` 最多持有一份 `ReasoningContinuation`，格式为 `openrouter.reasoning_details.v1` 时 payload 是一份可原样回传的有序 JSON 数组。
- OpenRouter accumulator 按 SSE chunk 到达顺序、再按每个 `reasoning_details` 数组的元素顺序追加对象；不按 `index`/`id` 重排、去重或跨对象合并。真实 SSE 风险验证若发现对象内部字段分片，必须先更新这里的契约，不能由实现者临时推导归并语义。
- OpenRouter 响应的可见 reasoning 优先取 `delta.reasoning`，其次 `delta.reasoning_content`，最后从可见 detail 中提取；同一 chunk 只选择一个来源，防止 UI 重复。
- 只有带 `tool_calls` 的历史 assistant 消息需要回传 reasoning：有 OpenRouter continuation 时发送 `reasoning_details`，否则发送明文 `reasoning`；普通历史回复不回传，控制输入成本。
- continuation 的格式标签用于阻止错误方言回传，但不承担跨 provider 历史兼容；session metadata 和服务端校验保证历史不会切换 provider/model。没有共享可变状态，accumulator 只属于单次 stream。
- session metadata 是持久化聚合根，拥有 name 和一份模型绑定。provider/model 一经首次打开不可变，reasoning effort 是同一绑定内可变且持久化的偏好；rename 和 effort 更新不得丢失彼此字段。

## Load-Bearing Decisions

- **一个 adapter，多种方言。** 保留统一消息/tool codec，把协议差异收口到 `ProviderDialect`，接受其内部逻辑比单一 OpenAI 实现更复杂，换取 server 与 agent 完全不感知 OpenRouter。
- **continuation 与展示文本分离。** UI 不解析签名或加密块，协议回传也不从展示字符串重建；代价是每条 reasoning assistant 消息多保存一份小型结构化状态。
- **reasoning details 只做有序收集。** 默认把每个 detail 对象视为不可修改的协议块，接受遇到未验证分片形态时明确失败并返回设计阶段，而不是用启发式 key merge 冒险破坏签名序列。
- **OpenRouter 使用规范化 `reasoning` 对象。** effort 发送为 `reasoning: { effort }`，`off` 映射 `none`；不继续依赖兼容别名 `reasoning_effort`，以便 max/xhigh 及后续 OpenRouter 扩展共用一个边界。
- **服务端绑定会话模型。** 首次打开后 provider/model 不可变；reasoning effort 可在空闲时切换并持久化。选择牺牲已有历史上的模型切换能力，换取工具 continuation 和重启恢复具有可执行的不变量，而不是依赖前端约定。
- **metadata 作为单一原子聚合更新。** name、binding 和 effort 共用一个 read-modify-write 边界与一把锁，所有写入都通过 `atomic-write-file` 替换完整实体。effort 切换先证明新 runtime 可构造，再落盘，最后提交无失败的 live swap，接受同步文件原语需要 `spawn_blocking`、并增加一个专用依赖，换取不再自行维护关键 metadata 的文件系统原子替换细节。该选择不扩大承诺为所有文件系统上的断电持久性。
- **静态配置是受信任的能力声明。** 不调用远端目录校正配置；启动只验证字段形状和内部一致性，不承诺 provider 会拒绝错误 capability，也不检测远端漂移。
- **空输出不是成功。** usage-only 尾块只更新统计，不算模型输出；整个流若没有内容、reasoning 或工具调用，`finish()` 返回 `InvalidResponse`。这会暴露冷启动/上游异常，而不是制造空 assistant 消息。

## Risks / Open Questions

- 最大风险是 OpenRouter 不同上游对流式 `reasoning_details` 的分片方式仍有差异。核心实现前先捕获三个样本模型的真实 reasoning + tool-call SSE，确认“按到达顺序追加对象”足够；若需要字段级组装，回到本设计明确规则后再继续。付费在线调用不进入自动测试，脱敏后的响应固定为 fixture。
- OpenRouter 未来可能新增 detail 类型。归并器应保留未知对象和未知字段；只有无法证明顺序/标识一致时才失败。
- 如果未来重新开放模型/provider 切换，应单独设计历史分段、上下文重置或 continuation 转换；本期服务端直接阻止产生这种混合历史。
- 静态 capability 可能随 OpenRouter 模型目录变化而失真，且不支持的参数可能被忽略。样本配置需在实现时对照当前官方目录，并以手工 smoke test 验证；运行时不提供漂移检测。
- OpenRouter 或上游可能返回非文本输出；当前 core 和配置没有输出 modality 契约。本期依赖部署前提，只把这种响应作为不支持的外部输入处理。
- 未配置 `max_completion_tokens` 时，实际输出上限由 OpenRouter 和上游模型决定，可能带来更高延迟或费用；example config 应为高推理样本展示建议值，但不替用户暗设默认。

## Implementation Roadmap

- [x] [risk validation] 使用具备三个样本模型访问权限和足够额度的 OpenRouter 凭据，捕获 Grok 4.5、Kimi K3、GLM 5.2 的真实 reasoning + tool-call SSE，确认 detail 是否可按到达顺序直接追加，并用原始序列完成真实 continuation
      Purpose: 在改动核心模型前锁定最不确定的 wire 行为；如果需要字段级组装，先修订并重新批准本设计
      Verification: 三份真实样本均能重建并原样回传 continuation；Grok 26 个 details（含 encrypted block）、Kimi 100 个、GLM 11 个，六次请求均为 HTTP 200 且无流内 error
- [x] [provider fixtures] 将三份真实响应脱敏后固化为 fixture，并补齐 reasoning 字符串、三种 details、并行工具、usage 和 HTTP 200 流内 error
      Purpose: 把已验证的 wire 形态转成离线回归边界
      Verification: 三份脱敏 fixture 保留 summary/text/encrypted、分片工具参数和 usage；独立测试覆盖交错并行工具与 HTTP 200 error envelope
- [x] [core model] 增加 `ReasoningContinuation`、`ProviderError` 和对应序列化/反序列化测试
      Purpose: 建立展示、协议续接、持久化之间唯一的数据契约
      Verification: `cargo test -p coda_core` 通过；message JSON round-trip 保留 continuation，空 format、空/标量 payload 被拒绝，provider error 保留 provider/status/type/message
- [x] [provider adapter] 将现有 `OpenAI` 整理为 `OpenAICompatible` + `ProviderDialect`，实现 OpenRouter request/replay/stream/error，并保持 generic/deepseek fixture 不变
      Purpose: 把所有兼容方言封装在一个可独立测试的深模块中
      Verification: `cargo test -p coda_openai` 22 项通过，覆盖三种方言、三份真实形态、details 原序回传、单次/交错并行工具、请求/响应错误分类、各方言 HTTP/API 错误、流中解码错误、SSE 传输错误、OpenRouter 流内错误和空流失败
- [x] [configuration] 支持 `kind = "openrouter"` 和可选 `max_completion_tokens`，更新 example config 与三样本静态配置示例，并对照实现时的 OpenRouter 官方目录人工确认 capability
      Purpose: 让部署者可显式控制输出预算，同时保留 provider 默认行为
      Verification: `cargo test -p coda_server config::tests` 42 项通过；覆盖字段缺省、合法值、零/负/非整数/超 context，OpenRouter kind、effort/default 既有校验保持通过；server 不再暗设 10,000
- [x] [session binding] 引入 `atomic-write-file 0.3` 默认 features，通过 `spawn_blocking` 写完整 metadata 聚合；在首次打开时持久化 provider/model/effort，恢复时采用绑定模型，并收紧 `set_model` 为“模型锁定、effort 可变”
      Purpose: 让同模型 continuation 与重启恢复成为服务端不变量，不依赖浏览器偏好
      Verification: server 测试覆盖首次绑定、重开时保留原绑定、跨模型拒绝、同模型 effort 更新和运行中拒绝；storage/hub 测试覆盖 rename 保留 binding、effort 更新保留 name、原子替换失败保留旧文件，以及写盘失败不改变 live selection
- [x] [server integration] 从静态配置构造 profile，删除固定 10,000；验证 session 保存/恢复后仍回传 continuation，流中错误不持久化 partial assistant
      Purpose: 接通真实 Agent 生命周期并固定失败重试语义
      Verification: agent 集成测试完成“模型 → 工具审批 → 重启恢复 → 模型”，断言第二次请求仍含原始 details；server checkpoint JSON round-trip 保留 continuation；错误后 checkpoint 只保留 user message、不含已展示的 partial assistant
- [x] [dashboard regression] 打开的 session 禁用 provider/model 下拉框但保留空闲时的 effort 选择；为图片请求编码、仅 `max`、不可关闭和含 `off` 的静态模型补充测试
      Purpose: 证明 UI 执行模型绑定，同时准确表达输入模态和三类 reasoning 能力
      Verification: OpenRouter request fixture 覆盖 Kimi K3“图片 → 工具 → continuation”；`pnpm --filter coda-web lint`、`pnpm --filter coda-web test`（12 项）与 `pnpm --filter coda-web typecheck` 通过
- [x] [final verification] 运行全仓 Rust 检查，并复用风险验证凭据手工 smoke test 三个 OpenRouter 模型
      Purpose: 捕获方言重构回归并验证真实 SSE 差异
      Verification: `cargo clippy`、`cargo test` 通过；ignored Rust smoke test 通过 `OpenAICompatible` 分别完成 Grok 4.5、Kimi K3、GLM 5.2 的工具调用与 continuation replay（六次真实请求）

## Deviations from Design

- 没有新增独立的 `ProviderDialect` 类型；`ProviderKind` 直接拥有私有的 request encode 与 response reduce 方法。方言边界和行为不变，但当前只有三个无状态分支，复用现有 enum 比再加一层策略类型更直接。
