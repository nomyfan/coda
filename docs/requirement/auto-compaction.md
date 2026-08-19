## Problem

目前 coda 只支持用户通过 `/compact` 手动触发上下文压缩，而且现有实现（`hub.rs` 的 `handle_compact`）强制要求 session 处于 idle 状态（没有正在执行的 turn）才允许压缩。长会话、尤其是包含大量工具调用的 agentic turn，会在**一次 turn 执行过程中**不断累积 token 用量，很可能在 turn 结束前就超过模型的 `context_window`，导致后续 LLM 请求出错或质量下降；而用户通常不会在合适的时机主动敲 `/compact`。需要系统在 token 用量接近/超过阈值时自动触发压缩，且不必等到 turn 结束才检测。

## Scenarios

1. **长 agentic turn 中途触发**：用户发起一次任务，agent 在同一个 turn 内做多轮工具调用（shell/grep/read_file 等），每轮都累积 usage。某一轮工具调用全部完成、下一次 LLM 请求发起前，服务端发现累计 token 用量超过该 model 配置的阈值，自动触发一次压缩。压缩的范围是"上一次压缩 cutoff 到当前 turn 的上一个 turn结束位置"——**不包含当前 turn 自己已经产生的消息**（因为这些消息是 agent 正在使用的活跃上下文，压缩掉会打断它对当前任务的推理）。压缩完成后追加的 "Context compacted" 记录消息，因为是在当前 turn 执行过程中插入的，物理上会出现在当前 turn 消息序列的中间，但它标记的 cutoff 仍然是"上一个 turn 结束时"，不是它自己的物理位置——因此后续判断"哪些消息还在有效上下文里"时，需要按这条记录消息携带的 cutoff（而不是它本身在消息列表中的物理位置）来切，否则会把当前 turn 里标记之前已经产生的消息误当作"已压缩"丢掉。用户不会看到中断，只会在之后查看 transcript 时看到这条记录，展开可看到摘要内容。
2. **普通多轮对话中触发**：session 经过多轮用户消息-助手回复往复，usage 缓慢上涨；某一轮 turn 即将/正在发起 LLM 请求时超过阈值，自动压缩后继续，行为与场景 1 一致，只是发生在 turn 之间而非 turn 内部。
3. **未配置阈值**：某个 model 在 `coda-server.toml` 里没有显式配置自动压缩阈值，系统按 `context_window` 的 80% 作为默认阈值判断。
4. **配置了具体阈值**：某个 model 显式配置了一个绝对 token 数阈值，系统按该值判断，不再套用 80% 比例。

## Scope

**In**
- 每个 model 可在 `coda-server.toml` 中配置一个可选的自动压缩 token 阈值（绝对 token 数，与 `context_window` 放在一起），未配置时默认按 `context_window` 的 80% 计算。
- 在 turn 执行过程中（而不仅是 turn 之间的 idle 状态）检测最新 `CompletionUsage.total_tokens` 是否超过阈值，一旦超过，自动触发一次压缩；压缩完成后 turn 继续执行剩余步骤。
- 复用现有压缩执行逻辑（`compaction.rs` 的摘要请求构造 + summary 消息追加、边界移动机制），不新建第二套压缩流程。
- 自动压缩静默执行，不打断/不询问用户；压缩完成后在 transcript 留下与手动压缩一致的 "Context compacted" 记录，用户可随时展开查看摘要内容。
- 自动压缩失败时的行为与手动压缩一致：transcript 记录失败，不移动压缩边界，turn 继续正常执行。失败的这一次检测不会立刻重试；但边界既然没有因为失败而移动，同一 turn 内如果后面还有检测点（例如又发生了一轮工具调用）、且那时 usage 仍超阈值，会按正常流程再次尝试压缩——这不是需要避免的重试循环，而是每个检测点各自独立判断的自然结果，最终受 turn 剩余步数限制。

**Out**
- 是否允许在 workspace/session 级别关闭自动压缩（本次不做开关，默认始终开启；开关需求留给后续迭代评估）。
- 自动压缩摘要内容/prompt 与手动压缩共用，不额外定制。
- 压缩后仍超阈值的场景（例如单条工具输出本身就极大）——本次不解决进一步降级策略。压缩失败后是否在同一 turn 的后续检测点重试，不额外加开关或退避策略，按"边界未移动、下一个检测点独立判断"的自然结果处理。
- root 线程之外的 sub-agent 线程各自独立的阈值判断——本次范围与现有手动 `/compact` 一致，只覆盖 root 线程。

## Constraints

- 现有 `handle_compact`（`hub.rs`）通过 idle 判定门控压缩，这与"turn 执行中触发"的要求冲突；设计阶段需要给出新的触发路径，不能直接套用 idle-gated 的 `SessionCommand::Compact`，但应尽量复用其压缩执行与落盘逻辑（`compaction.rs`），避免产生两套机制。
- 工具调用可能并行执行（`KeyedLock` 相关文档），自动压缩的触发点应选在一轮工具调用全部完成、下一次 LLM 请求发起之前，不应打断正在执行中的工具调用。
- 阈值判断依赖已有的 `CompletionUsage`（`crates/coda_core/src/llm.rs`）与 `ModelConfig.context_window`（`app/coda_server/src/config.rs`），不引入新的 token 计数机制。
- 压缩范围需要按 turn 切分，不能只按压缩标记的物理位置切。现有数据模型已经具备这个能力，不需要新概念：`messages.turn_id`（`app/coda_server/src/schema.rs`）在 schema 和内存态（`HistoryEntry`，`crates/coda_agent/src/agent.rs`）里都已存在，配合 root 线程的 `seq` 列（`retained_turns`，`app/coda_server/src/storage.rs` 已有同样按 `seq` 切 turn 边界的先例）就能表达"当前 turn 的消息" vs "之前所有 turn 的消息"。但现有 `message_view::model_view`（`crates/coda_agent/src/message_view.rs`）是"取最后一条压缩标记之后的物理位置"，语义上假定标记的物理位置就是 cutoff；自动压缩需要把这两者解耦——压缩标记记录的 cutoff（上一个 turn 结束时的 seq）与它自己被追加时的物理位置可以不一致，`model_view` 的取值逻辑要按记录的 cutoff 而不是标记的物理位置来切，否则会把当前 turn 里标记之前已产生的消息误判为"已压缩"。

## Success Criteria

- model 未配置阈值时，系统按 `context_window` 的 80% 判定；配置了具体阈值时按该值判定。
- 一次包含多轮工具调用的 turn 中，只要中途累计 usage 超过阈值，压缩会在下一次 LLM 请求前自动完成，turn 能继续正常完成，无需用户手动介入。
- 自动压缩不会打断正在执行中的工具调用。
- 自动压缩产生的摘要范围严格止于"上一个 turn 结束位置"，不包含当前进行中 turn 已经产生的消息；压缩标记消息即使物理上出现在当前 turn 序列中间，也不会导致当前 turn 自己的消息被误判为已压缩。
- 自动压缩完成后，transcript 中出现与手动压缩一致的 "Context compacted" 记录，可展开查看摘要。
- 自动压缩失败时不影响 turn 正常执行，失败记录出现在 transcript 中，压缩边界不移动。
- 同一次检测点内最多触发一次压缩尝试，不会在一次检测里反复重试导致死循环；一次成功的压缩会把边界前移，使后续检测点在没有新内容时判定为"无需压缩"。一次失败的压缩不移动边界，因此同一 turn 内更靠后的检测点若 usage 仍超阈值会再次独立尝试，直到成功、或 turn 结束、或没有更多可压缩的新内容。
