# Compaction — 设计方案

> 状态：v4.4 已实现。评审见 `docs/review/compaction.md`。
> v1→v2：移除自动 compaction，只保留显式 `/compact`。
> v2→v3：整体下沉到应用层（`coda_server`），运行层只剩一处改动；压缩记录改用 `thread_state` 边界标记。
> v3→v4：**拆分 `Message` / `RequestMessage`**，新增 `Message::Custom`；压缩边界改由摘要消息自身承担，**`thread_state` 标记整个去掉**。
> v4→v4.1（评审修订）：`compact` 显式接收 model binding；压缩纳入 entry 活跃门闩，并在提交时对 `message_count` 做 CAS；结果类型拆出「什么都没写」这一种。
> v4.1→v4.2（评审修订）：修正 CAS 的 root thread id（是 session id，不是字面量 `"root"`）；`compacting` 改挂 `EntryState` 并补上 `rewind` 门闩；`compacting` 进 snapshot。
> v4.2→v4.3（评审修订）：`CustomRole` 收窄为 `User | Assistant`（`Tool` 降级不出合法消息）；快照推送改为新增内部事件 `RelayEvent::Snapshot`，压缩开始与结束各推一次。
> v4.3→v4.4（评审修订）：`compact` RPC 响应不再携带消息实体，transcript 只由快照事件更新。
> v4.4→v4.5：`coda_web` 发送 `/compact` 时先乐观展示该行（`pendingCompact` 标记），结束快照按内容对账；响应仍只报告结果，失败/拒绝路径移除乐观副本。
> v4.5→v4.6：`CustomMessage` 新增 `visibility: Option<Vec<Visibility>>`（`None` = 不限视图；`Transcript` / `Model` 组合）。失败消息（`compaction_failed`）写 `Some(vec![Transcript])`，模型视图（`compaction::view` + `Message::visible_to_llm()`）据此滤掉它，只留在 transcript（UI 照常展示）；运行层与摘要输入共用同一条规则。

## Problem

一次会话的 thread 历史只增不减，迟早撑爆模型的 context window：请求要么被 provider 拒绝，要么把预算全花在陈旧的 tool 输出上。需要给用户一把闸——`/compact`——把旧历史压成一段摘要继续对话，且**不丢失可回看历史、不破坏 rewind/fork 语义**。

什么时候该压缩由用户判断（composer 上已有 context usage 环形指示器）。runtime 自动触发是后续的事。

## 前提：压缩改的是「视图」，不是历史

这是整个方案的立足点。**session 的原始 messages 一条都不删、不改**，压缩只改模型看到的那一份。代码上这两者本来就是分开的两个函数，而且各只有一个调用点：

| 函数 | 唯一调用点 | 喂给谁 |
| --- | --- | --- |
| `Agent::messages()` | `driver.rs:1224` | `ChatCompletionRequest` —— **只有模型看这个** |
| `Agent::history()` | `driver.rs:775` | `StoredCheckpoint.messages` —— 落库的原始历史 |

本设计**只改 `messages()`**，`history()` 一个字不动。于是往下游一路都保持全量：

```
history()（全量）→ messages 表（只追加）
                 → load_checkpoint → Session::resumed_messages
                 → hub live.snapshot → Snapshot.messages → 前端 transcript
```

压缩之前的对话在 transcript 里照常渲染。rewind 与 fork 读的同样是原始历史，因此也不需要改。

## 基础改动：拆开「历史消息」与「请求消息」

今天 `coda_core::llm::Message` 一个类型兼两职：线程历史的元素，和 `ChatCompletionRequest` 的载荷。这两者的成员**本来就不一样**，代码已经在为此打补丁——每一个非测试的 `Message::System` 落点都是在堵洞：

| 位置 | 现状 |
| --- | --- |
| `storage.rs:789` | `Err("cannot persist a system message: the system prompt is not history")` |
| `agent.rs:153` | 返回 `None`，逼得 `add_message_with_state` 带一个 `error!("dropping state recorded against a message with no id")` 分支 |
| `hub_tests/replay.rs:165` | `unreachable!("a system message reached persisted history")` |
| `tests/storage_pg.rs:159` | `unreachable!("system messages are not history")` |

真正有意义地构造/消费 `System` 的只有两处：`agent.rs:608` 组请求、`coda_openai/lib.rs:33` 渲染。

压缩需要一种「历史里有、但要先降级才能发给模型」的消息，正是让这条缝显形的契机。拆成两个类型：

```rust
/// 线程历史装的东西。没有 System —— system prompt 不是历史，组请求时才现加。
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolMessage),
    Custom(CustomMessage),
}

/// 发给 provider 的东西。没有 Custom —— 组请求前一律降级成其余三类之一。
pub enum RequestMessage {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolMessage),
}

pub struct CustomMessage {
    pub message_id: MessageId,
    /// 应用层语义，对 `coda_core` 以下完全不透明。UI 按它渲染。
    pub kind: String,
    /// 降级目标。`From<&Message> for RequestMessage` 只读这个。
    pub role: CustomRole,   // 只有 User | Assistant
    pub content: String,
    pub created_at: jiff::Timestamp,
}
```

**`CustomRole` 不包含 `Tool`。** 一条只有文本的 `CustomMessage` 降级不出合法的 `ToolMessage`——后者要配套的 tool-call `id`、工具名、`output`、`outcome`。就算硬凑一个出来，它也是一条前面没有对应 `tool_calls` 的**孤儿 tool result**，正是 Decision 11 存在的理由。枚举里留着这个取值就是留一个必然写错的口子。将来真需要投影成 tool 消息，那要的是一个能携带完整投影载荷的类型，不是这里加个变体。

于是「进 `coda_openai` 之前必须降级」不是纪律，是类型。**`coda_openai` 一行都不改**——还是今天那四个 arm，只是签名上的类型名换成 `RequestMessage`。

降级放在 `coda_core`，`impl From<&Message> for RequestMessage`，只有一条规则（按 `role` 分派），永远不认识任何 `kind`。`Agent::messages()` 变成：

```rust
pub async fn messages(&self) -> Vec<RequestMessage> {
    let history = self.state.lock().await;
    let mut messages = Vec::with_capacity(history.messages.len() + 1);
    messages.push(RequestMessage::System(SystemMessage(self.system_prompt.resolve())));
    messages.extend(
        compaction::view(&history.messages)
            .iter()
            .map(|e| RequestMessage::from(&e.message)),
    );
    messages
}
```

降级时复用 `CustomMessage` 自己的 `message_id`——请求向量用完即弃，`IntoOpenAIType` 根本不读 message_id。

**不要叫 `WireMessage`**：`app/coda_server/src/wire.rs` 已经是*客户端*协议的意思，拿它指 provider 格式会互相打架。

## 核心机制

一次成功的压缩往 root thread 追加**两条消息**：

```
messages:  … │ User("/compact 只保留架构决策") │ Custom{kind:"compaction", role:User}
                                                └── 边界就是它自己
```

视图组装是**一刀后缀切片**：

```rust
pub fn view(messages: &[HistoryEntry]) -> &[HistoryEntry] {
    let start = messages.iter().rposition(is_compaction).unwrap_or(0);
    &messages[start..]
}
```

`/compact` 那条 user 消息落在边界**之前**，因此对模型不可见——它只服务 transcript。用户追加的自定义指令由应用层回显进摘要正文，模型照样知道是什么要求塑造了这段摘要：

```
[本次压缩遵循的用户要求] 只保留架构决策

<摘要正文>
```

模型压缩后看到的是：

```
system
User(<摘要，由 Custom 降级而来>)
User(下一个任务)…
```

失败时写 `Custom{kind:"compaction_failed", role:Assistant}`，内容是失败原因，并带 `visibility = Some(vec![Transcript])`。它不是 compaction kind，**边界不动**。v4.6 起模型视图按 visibility 过滤：失败记录只进 transcript（前端照常展示），模型视图保持「失败等于什么都没发生」——否则每次后续 turn 和下一次摘要输入都要白付 token 看这条错误。下一次成功压缩的边界会把这条记录（连同 `/compact` 行）从 transcript 视图一起甩到后面，不需要任何清理逻辑。

**边界是一条消息，不是旁路状态。** 这是 v4 相对 v3 最重要的一点：rewind 按 seq 切消息、fork 复制消息前缀，两者都**天然**把边界切对，不需要 FK、不需要锚点、不需要绕开 `storage.rs:1101` 那条「只持久化本次追加消息上的 state」的过滤。子 agent 线程里没有这种消息，`rposition` 返回 `None` 走 `unwrap_or(0)` 得到全量历史，也不用判断线程身份。

## Scope

**In:**

- `coda_core`：拆 `Message` / `RequestMessage`，新增 `Message::Custom` 与 `From<&Message> for RequestMessage`。
- `coda_agent`：`compaction::view()` 纯函数 + `Agent::messages()` 调用它并降级。
- `coda_server`：`SessionOpener::compact()`（读视图 → 摊平 → 调一次 LLM → 一个事务写回）、`SessionCommand::Compact`、`compact` RPC、内置 `compaction-prompt.md`；`EntryState.compacting` 门闩；`message_row_identity` 增加 `"custom"` role；`SnapshotPayload` / `Snapshot` 增加 `compacting` 字段；新增内部事件 `RelayEvent::Snapshot`。
- `coda_web`：`HistoryMessage` 增加 `Custom` 分支、去掉 `System` 分支；`Snapshot` 类型跟进 `compacting`；composer 识别 `/compact ` 前缀，改调 `compact` RPC 而非 `task`，进行中状态由快照驱动；发送时先乐观展示 `/compact` 行（`pendingCompact` 标记，非 turn、不置 `running`），结束快照按内容对账、失败/拒绝时移除。

**Out:**

- **自动 compaction。** 需要 driver 主循环的阈值检查与一个不结束 turn 的失败路径，与本设计的应用层形态是两条路子。留待后续。
- **`compact` 作为工具。** 见 Alternatives 4。不冲突，但不在此范围。
- **不删任何历史消息。**
- 保留尾部（summary 之后再原样保留若干旧 turn）。边界之后本来就没东西。
- sub-agent thread 的压缩。`/compact` 只作用于 root thread。
- 压缩过程的中断（abort）。靠请求超时兜底，见 Risk 4。
- **`thread_state`。** v3 用它承载边界标记，v4 完全不碰。
- **无 schema migration。** `messages.role` 是裸 `text not null`（`migrations/20260725000000_sessions/up.sql:59`），没有 CHECK 约束，加一个 `"custom"` 取值不需要迁移。

## Assumptions

- 破坏性变更可接受（`AGENTS.md`）：`Message` 的序列化形状与 `HistoryMessage` 的 TS 联合类型都会变。
- `/compact` 只在 session idle 时受理（与 rewind 同一把闸：`turn_running` 为假且无 pending approval）。
- 压缩失败是低概率事件，不值得为它设计独立的恢复流程——写一条说明就够。
- 单进程 hub、单活跃 turn。

## Validation Findings

| 问题 | 方法 | 结果 | 设计含义 |
| --- | --- | --- | --- |
| 加一个 `Message` 变体要动几处？ | 全仓 `Message::` 扫描 | 穷尽 match 只有四处：`llm.rs:473` 定义、`agent.rs:149` `message_id_of`、`coda_openai/lib.rs:32` `IntoOpenAIType`、`storage.rs:785` `message_row_identity` | 编译器会替你列全。其余（`session_preview` 的 let-else、`storage.rs:1075` 的 origin、`driver.rs:841` 的 `interrupted_calls`）都有 `_` 兜底 |
| 请求侧的 `Message` 引用有几处？ | 同上 | 四处：`llm.rs:506` `ChatCompletionRequest.messages`、`agent.rs:605`、`coda_openai/lib.rs:371` 与 `:401`（`inject_deepseek_reasoning`）。其余三十余处全是历史侧 | 拆类型的成本集中在这四处，历史侧名字不变 |
| `LLMProvider` 有几个实现？ | `impl LLMProvider` 扫描 | 一个真实现（`OpenAICompatible`）+ 两个测试替身 | 降级规则只有一个下游要伺候 |
| `role` 列有约束吗？ | `migrations/20260725000000_sessions/up.sql:59` | `role text not null`，无 CHECK | 新增 `"custom"` 无需 migration |
| 前端不认识新变体会怎样？ | `session.ts:392` `historyToEntries` | `if ("X" in message)` 链，末尾 `return []` | 静默不渲染。编译与测试都不报错，**必须自己想起来加分支** |
| 摘要会不会污染 context 表盘？ | `session.ts:469` `historyUsage` | 只认 `"Assistant" in message` | `Custom` 自动跳过。v3 里「摘要那条 `usage` 必须为 `None`」这条靠约定维持的不变量，在 v4 成了类型层面的事实 |
| 应用层拿得到 provider 吗？ | `bin/server.rs:86`、`:358` | `AppState.providers` 持有 `Arc<OpenAICompatible>` 与 `context_window`/`max_completion_tokens`，`ModelProfile` 本就是 server 造好交给 runtime 的 | **压缩不需要进运行层** |
| 改了存储，跑着的 runtime 会读到旧历史吗？ | `driver.rs:504` | `AgentLoop::run` **每一轮开头**都 `load_checkpoint` + `restore_history` | 不必像 rewind 那样停掉再重开 Session |
| rewind 为什么要 shutdown？ | `hub.rs:889` 注释 | 因为它随后要 `open` 第二个 runtime，两者不能重叠 | 压缩不 rebuild，所以不需要 shutdown |
| `/` 菜单能不能直接用？ | `composer-mentions.ts:12` | `/` 选中的是 skill，结果是插进草稿的**文本**，会原样发给模型 | `/compact` 必须在发送前被拦截成命令 |
| rewind 持锁多久？ | `hub.rs:871` "Runs entirely under the entry lock" | 全程持锁，但里面没有网络往返 | 压缩有 10–30s 的 LLM 调用，**不能照抄**，见 Decision 5 |
| 放锁期间 entry 会不会消失？ | `hub.rs:757` `maybe_release` | 条件是 `attached.is_none() && !turn_running`。压缩期间 `turn_running` 为假，**客户端一断线就释放** | generation 校验不够用 —— entry 都没了就没有 generation 可比。必须把压缩接进门闩并在存储层兜底，见 Decision 6 |
| `message_count` 的粒度？ | `schema.rs:49`、`storage.rs:1054` | 在 `thread_checkpoints` 上，per-thread；`write_checkpoint` 拿它当 append 起点 | CAS 要按 thread 定位，于是引出下一行 |
| root thread 的 id 是什么？ | `storage.rs:690` 与 `:984` 注释、`session.rs:488`、`rewind_to` 的查询（`storage.rs:1320`/`1362`/`1459`） | **root thread 的 `thread_id` 就是 session id**，没有字面量 `"root"` 这种东西 | CAS 条件写 `thread_id = $session_id`。写成 `'root'` 会让每次更新影响 0 行、被误判为 `Stale`，压缩永远做不成 |
| 标志放 `LiveState` 上行不行？ | `hub.rs:768` `make_live`；`handle_rewind`、`set_model` 都调它 | 两条路径都**重建一个全新的 `LiveState`**，挂在上面的字段会被整体丢弃 | `compacting` 必须挂 `EntryState`，与 `PermissionModeCell` 同理（它正是因为「hangs off the entry, not the phase」才扛得住 `SetModel` 重建）|
| `rewind` 的门是什么？ | `hub.rs:885` | 只有 `turn_running \|\| !pending_approvals.is_empty()` | 它同样改写 root 历史并重建 runtime，必须加进门闩，见 Decision 6 |
| 客户端怎么知道正在压缩？ | `wire.rs:427` `Snapshot.turn_running`；`session.ts:2050`/`2183` | 快照只有 `turn_running`，压缩期间为假 | 接管/重连的客户端会把会话当 idle 并允许发送，直到被 `NotIdle` 拒。`compacting` 要进快照，见 Decision 8 |
| hub 能主动推快照吗？ | `hub.rs:81` `enum RelayEvent`；`bin/server.rs:1666` | **不能。** `RelayEvent` 只有 `Event` / `Evicted` / `Closed`；那条 `snapshot` 通知是连接层在收到 `Closed` 后**重新 attach 一次**才发的，hub 手里没有 `send_notify` | 需要新增内部事件 `RelayEvent::Snapshot(Box<SnapshotPayload>)`，由连接层映射到已有的 `snapshot` wire 通知。**wire 类型不新增，内部事件类型要新增**，见 Decision 8 |
| hub 怎么拿到当前 model binding？ | `hub.rs:484` `LiveState.provider_id` / `reasoning_effort`；`hub.rs:186` `SessionOpener::open` | live state 持有，且 `open` 是**显式传参**，不让 opener 自己去存储里读 | `compact` 照此办理，见 Decision 7 |
| session 被删除时的写入竞争？ | `thread_checkpoints` / `messages` 对 `sessions` 的复合外键 + `commit_compaction` 的首条 CAS | 级联删除先带走 checkpoint，CAS 影响 0 行并返回 `Stale`；FK 仍是最后防线 | 删除不会被压缩写活。把压缩接进门闩是为了省掉白花的 LLM 调用与费解的报错，不是正确性要求 |

## Alternatives Considered

**1. 不拆类型，`Message` 直接加 `Custom`，在 `coda_openai` 里渲染它。**
最小改动。**否决**：`coda_openai` 的 arm 若要按 `kind` 分派，应用语义就漏进了最底层的 provider crate；若不按 `kind` 分派而固定成一种渲染，那这个 arm 与上层的降级逻辑就是同一条规则的两份实现，迟早分叉。拆类型后 `coda_openai` 反而**一行都不用改**。

**2. 不拆类型，在 `Agent::messages()` 里降级，`coda_openai` 留一个够不着的 arm。**
**否决**：类型仍然允许 `Custom` 走到 provider adapter，「降级过了」是纪律不是保证。写 `unreachable!()` 是在类型系统检查不到的地方立字据。

**3. 新增 `Message::Compaction` 专用变体。**（v1 方案）
**否决**：为一个用途开一个变体，第二个用途来了就得开第二个。`Custom { kind }` 是同样的表达力、一个变体的成本，且 `kind` 对 `coda_core` 以下不透明。

**4. 摘要落成普通 assistant message，边界写进 `thread_state`。**（v3 方案）
不需要动 `Message`，是 v4 之前的选择。**否决**：边界成了旁路——`compaction::view()` 要吃两个参数，还要和 `storage.rs:1101` 那条「只写锚在本次新增消息上的条目」的规则保持同步，写在别处的标记会被**静默丢弃**。而且摘要作为 assistant 消息带 `usage` 就会让 context 表盘反向跳高，只能靠约定置 `None`。改成消息之后，边界只受一条已经被 rewind/fork 实现好的规则约束，`usage` 问题自动消失。

**5. 把 `compact` 做成一个工具，由模型自己调用、自己写摘要。**
很有吸引力：省掉一整次全量 context 请求（模型此刻上下文里就是完整对话），sub-agent 免费获得同样能力，`/compact` 甚至可以只是一个 skill。**本轮不做**，原因是确定性：`/compact` 会从「按下必然执行」退化成「对模型的请求」，可能不调、可能摘要写得差。另外模型若把 `compact` 和别的工具放进同一批调用，并发结算顺序会造出边界之后的孤儿 tool result。这条路没被封死：工具版写同一种 `Custom{kind:"compaction"}` 消息即可，将来要加是增量。

**6. 压缩请求原样重放对话历史（而不是摊平成文本）。**
**否决**：那样就得携带完整 tool definitions（否则历史里的 `tool_calls` 在部分 provider 上非法）、要处理 `reasoning_continuation` 被截断、还要保证不出现孤儿 tool result。摊平成一段文本后这三个问题**全部消失**，而且压缩请求本来就不必是一段合法对话——它是「总结这份记录」这一个任务。代价：图片内容摊不平（丢弃或单独附上）。

## Components

1. **`coda_core::llm`（改）** — 拆 `Message` / `RequestMessage`；新增 `CustomMessage`、`CustomRole`、`From<&Message> for RequestMessage`。
2. **`coda_agent::compaction`（新模块）** — `view(&[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry>`：`rposition` 找最新 `Custom{kind:"compaction"}`，返回从那里开始的后缀，并滤掉 `Message::visible_to_llm()` 为假的记录（v4.6：`visibility` 不含 `Model` 的 custom 消息，目前只有失败记录）；没有则返回全部。纯函数，无 I/O。**压缩规则的唯一实现**：运行层拼请求要用它，应用层取「上次压缩之后的消息」当摘要输入也要用它，两处各写一遍迟早分叉。`coda_server` 本就依赖 `coda_agent`。
3. **`Agent::messages()`（改）** — `system + view(history).map(RequestMessage::from)`。运行层的全部改动。
4. **`PgSessionStorage::commit_compaction()`（新）** — 一个事务，带 `expected_message_count` 参数：以 `WHERE thread_id = $session_id AND message_count = $expected` 为条件更新，不匹配即回滚成 `Stale`；插入两条 message 行（`role` 分别为 `"user"` / `"custom"`）；`message_count += 2`；`touch`。
   **`thread_id` 就是 session id** —— root thread 的 id 不是字面量 `"root"`（`storage.rs:984`）。`rewind_to` 已经是这么写的（`messages::thread_id.eq(&self.session_id)`），照抄即可；写错会让 CAS 每次影响 0 行，压缩永远 `Stale`。
5. **`SessionOpener::compact()`（新 trait 方法）** — 与既有的 `rewind` / `fork` 同形。读 checkpoint（同时记下 `message_count` 当 CAS 基线）→ `compaction::view` → 摊平成文本 → 用调用方传入的 model binding 发一次 LLM 请求 → 调用 4 写回 → 返回 `Compacted`。provider 知识留在 `bin/server.rs` 的实现里，`SessionHub` 不碰。
6. **`SessionHub`（改）** — `SessionCommand::Compact { instructions }`；闲置检查；`EntryState.compacting` 门闩（**挂 entry 不挂 phase**，五处判断，见 Decision 6）；从 live state 取 `provider_id` / `reasoning_effort` 传下去；放锁做 LLM 调用；回来后更新 `live.snapshot`、给已附着的客户端推一次快照、补一次 `maybe_release`。
7. **`compact` RPC（新）** — 参数 `{ workspace_id, session_id, instructions }`。响应**只报告结果，不带消息实体**：`"applied"`（边界已推进）/ `"recorded"`（摘要失败，记录已写，边界未动）/ `"stale"` / `"storage_error"`（都是一条没写、可原样重试）。transcript 一律由快照事件更新，见 Decision 9。
8. **`compacting` 上线（新字段 + 新内部事件）** — `hub.rs:92` 的 `SnapshotPayload`、`wire.rs:415` 的 `Snapshot`、`protocol.ts` 的对应类型各加一个 `compacting: bool`，`session.ts` 读它的地方（`2050`/`2183`）跟着传。另加内部事件 `RelayEvent::Snapshot(Box<SnapshotPayload>)`，hub 在压缩**开始**与**结束**各推一次；连接层把它转成已有的 `snapshot` wire 通知。**这个变体不终止流**——`Evicted` / `Closed` 都是终止的，连接层处理它时不能动 `streams` / `selections` / `reattached`。
9. **`compaction-prompt.md`（新）** — 内置摘要 system prompt，说明如何对待用户追加的自定义要求。
10. **`coda_web`（改）** — `protocol.ts` 的 `HistoryMessage` 加 `{ Custom: CustomMessage }`、去掉 `{ System: string }`；`historyToEntries` 加对应分支；composer 在发送前识别 `/compact ` 前缀，改调 `compact`；**composer 的进行中状态由快照里的 `compacting` 驱动**（而非只在本地记一次），这样中途接管的客户端也能正确禁用发送。v4.5 修订：**发送时乐观展示 `/compact` 行**——`compact` 响应不带消息实体，结束快照按内容（`/compact` / `/compact {instructions}`）对账，移除乐观副本、保留落库行；`empty` / `abandoned` / RPC 错误路径直接移除；乐观副本带 `pendingCompact` 标记，不计入 pending task（不置 `running`，避免出现无意义的 abort 按钮）。

## Interfaces

```rust
// coda_agent::compaction —— 压缩规则的唯一实现
pub const COMPACTION_KIND: &str = "compaction";
pub const COMPACTION_FAILED_KIND: &str = "compaction_failed";

/// 发给模型的那份历史：从最新压缩边界开始的后缀。
///
/// 边界是最后一条 `Custom { kind == COMPACTION_KIND }` 消息，**含它自己**。
/// 没有这样的消息时返回完整历史。
pub fn view(messages: &[HistoryEntry]) -> &[HistoryEntry];
```

```rust
// coda_server::hub —— 与 rewind / fork 同形
pub trait SessionOpener {
    /// 生成一次压缩摘要并落盘，返回写入 root thread 的两条消息。
    ///
    /// 调用方必须先确认 session 处于闲置、已置上 `compacting` 门闩，且
    /// **不得持有 entry 锁** —— 里面有一次完整的 LLM 往返。
    ///
    /// model binding 由调用方从 live state 显式传入，与 `open` 同规矩：
    /// opener 不去存储里猜当前选的是哪个模型。
    ///
    /// 内部在读 checkpoint 拼视图时记下 root 的 `message_count`，提交时以它
    /// 做 CAS。期间 root 被追加过就整体放弃（`Stale`），一条消息都不写。
    fn compact<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<&'a str>,
        instructions: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Compacted, CompactError>> + Send + 'a>>;
}

/// 两种**都写了东西**的结果。区分它们是为了让 UI 知道边界有没有动。
///
/// 带着消息实体是给 **hub 内部**用的：它要更新 `live.snapshot`，并据此拼出
/// 推给客户端的 `SnapshotPayload`。**`compact` RPC 的响应不转发这两条消息**
/// —— 见 Decision 9。
pub enum Compacted {
    /// 摘要生成并落库，边界已推进。
    Applied { command: Message, summary: Message },
    /// 摘要生成失败，失败记录已落库；边界不变，视图照旧。
    Recorded { command: Message, failure: Message },
}

/// 一条消息都没写。客户端可以原样重试，transcript 不需要回滚。
pub enum CompactError {
    /// 有 turn 在跑、有待批准的工具调用，或已有一次压缩在进行。
    NotIdle,
    /// CAS 失败：读视图之后 root thread 被追加过（entry 曾被释放、新 runtime
    /// 写入了新消息），或 session 已被删除。摘要已经过期，丢弃。
    Stale,
    Storage(String),
}
```

`Applied` / `Recorded` 的区分不能省。LLM 失败会留下一条可见的说明，而 `Stale` / `Storage` 什么都没留下——UI 对前者要展示记录，对后者要提示「压缩未执行，可重试」。用一个 `applied: bool` 表达不了这三种。

**信任边界在 `compact` RPC 的参数解析处。** `instructions` 是唯一的用户输入，只作为一段文本进入摘要请求（`substitute` 是单趟的，`agent.rs:340`，注入不了占位符），以及作为 user message 的正文落库、回显进摘要正文。校验只有长度上限（建议 4 KiB），超出直接拒。

## Data Model

- **`/compact` 命令消息** 是 `messages` 表的普通行，`role = "user"`。只服务 transcript，模型看不到。
- **摘要消息** 是 `messages` 表的行，`role = "custom"`，payload 里 `kind = "compaction"`、`role = User`。**它既是内容也是边界**，只存一份。
- **失败消息** 同样 `role = "custom"`，`kind = "compaction_failed"`、`role = Assistant`，`visibility = Some(vec![Transcript])`。它落在边界判定之外，但模型视图按 visibility 把它过滤掉（v4.6 修订：诚实记录归 transcript，模型看到它只会浪费 token 并污染下一次摘要输入）。
- **`usage` 不存在**：`CustomMessage` 没有这个字段，压缩请求的 token 消耗不进入前端的「当前上下文占用」估算。
- **turn 归属**：两条消息共享一个 turn id，取那条 user 消息的 id（沿用「turn 由开启它的 user message 命名」的既有规则）。
- **不变量：**
  1. 至多一条 `compaction` 消息是「生效的」——最后那条；更早的只是历史痕迹。
  2. 视图永远是真实历史的一个后缀，因此永远是合法 provider 历史（不需要额外校验）。
  3. 写入是全有或全无：两条消息、`message_count` 推进、CAS 判定在同一个事务里。CAS 不过就一条不写。

拆类型顺带把几处手工巡逻变成不可能：`message_id_of` 变成全函数（`-> MessageId`，`add_message_with_state` 里的 `if let Some(anchor)` 与 `error!` 分支一并删除）；`message_row_identity` 变成不可失败（`-> (MessageId, &'static str)`，少一个 `SaveError::Rejected` 调用者）；两个 `unreachable!()` 自行消失；`protocol.ts` 去掉 `{ System: string }`，`historyToEntries` 开头的 `if ("System" in message) return []` 一并删除。

## Load-Bearing Decisions

1. **压缩只追加，永不改写或删除历史。** 代价：库里存着模型再也看不到的消息。收益：`write_checkpoint` 的 append-only 断言不用碰；rewind / fork / `message_count` 水位线全部原样可用；transcript 完整可回看；rewind 到压缩之前天然就是「撤销压缩」。

2. **边界是一条消息，不是旁路状态。** 消息顺序这条规则，rewind 与 fork 已经实现好了，边界白拿正确的生命周期。而 v3 的 `thread_state` 方案要额外与 `storage.rs:1101` 的锚点规则保持同步，写错位置会被静默丢弃。**这条是 v3→v4 的全部理由。**

3. **`RequestMessage` 装不下 `Custom`，让「降级」成为类型保证而非纪律。** 代价：`coda_core` 多一个枚举、四处请求侧引用改名。收益：`coda_openai` 零改动；降级规则只有一份实现且只认 `role` 不认 `kind`；顺带消掉四处对 `System` 的手工兜底。

4. **压缩在应用层执行，运行层只负责切片。** 依据是两条实测结论：provider 句柄在 `AppState` 里（`bin/server.rs:86`），driver 每轮重载 checkpoint（`driver.rs:504`）所以不存在状态不同步。**代价是自动压缩将来要另走一条路**——它必须在 driver 里判断，且失败不能结束 turn。这是明知的取舍，不是遗漏。

5. **不能握着 entry 锁做 LLM 调用。** `handle_rewind` 全程持锁（`hub.rs:871`）是因为它里面没有网络往返。压缩要等 10–30 秒，持锁会把该 session 的 attach、abort、其他命令全堵住。所以：置 `compacting` 标志 → 放锁 → 发请求 → 重新拿锁 → 写回。

6. **放锁的代价由「门闩 + CAS」两层承担，缺一不可。** 这是评审补上的缺口，也是本设计唯一会**静默丢数据**的地方。

   `maybe_release` 只看 `attached.is_none() && !turn_running`（`hub.rs:757`），压缩期间 `turn_running` 为假，所以**客户端断线就会释放 entry**。entry 一旦消失，就没有 generation 可校验；随后新 attach 打开新 runtime、用户发新 turn、消息落库，旧的压缩请求回来把边界追加在这些新消息**之后**——视图把它们整段切掉。这比压缩失败严重得多。

   - **门闩挂在 `EntryState` 上，不挂 `LiveState`。** 这是评审第二轮补的，也是个真坑：`handle_rewind` 与 `set_model` 都走 `make_live`(`hub.rs:768`) **重建一个全新的 `LiveState`**，挂在上面的字段会被整体丢弃——entry 随即显示为 idle，`maybe_release` 可以在压缩进行中释放它，第二次 `/compact` 也会被放行，门闩形同虚设。`PermissionModeCell` 正是因为「hangs off the entry, not the phase」才扛得住 `SetModel` 重建，照抄。挂到 entry 上之后五个检查点拿的都是同一个 `state`，反而更简单。
   - **五个检查点**：`maybe_release`(757)、`task`(811)、`set_model`(1067)、`fork` 的 `ForkGate`、`rewind`(885)。**`rewind` 不能漏**——它同样改写 root 历史并重建 runtime。选择让它**拒绝**（返回 `NotIdle`）而不是取消压缩：取消要把 cancellation token 一路穿进 opener 再 await，为一个罕见竞态加一套机制不划算，用户等 ≤10 分钟重试即可。
   - 压缩完成后必须补一次 `maybe_release`，否则断线的 entry 会滞留到超时——与 turn settle 那条路径（`hub.rs:1698`）同一个处理。
   - **不要拿 `turn_running` 兼职**：`Shutdown::graceful_unbounded` 与 `Shutdown::abort` 都是冲着 runtime 里的 turn 去的，压缩根本不在 runtime 里，复用会让优雅关闭去等一个不存在的 turn。
   - **CAS**：门闩挡不住进程退出、驱逐、删除这些路径，所以真正的保证在存储层。`commit_compaction` 以拼视图时读到的 `message_count` 为条件更新（`WHERE thread_id = $session_id AND message_count = $expected`），不匹配就整体回滚成 `Stale`。语义正好是「我读完之后 root 有没有被人追加过」。**`thread_id` 是 session id**，不是字面量 `"root"`（`storage.rs:984`）。
   - 删除路径**已有兜底**：级联删除会先带走 root checkpoint，事务开头的 CAS 因而影响 0 行并返回 `Stale`；`messages` 对 `sessions` 的复合外键还是最后防线。压缩写不活已删除的会话，门闩覆盖它只是为了省掉一次白花的 LLM 调用和一个费解的报错。

7. **model binding 由 hub 显式传给 opener。** 与 `open`(`hub.rs:186`) 同规矩：权威值在 `LiveState.provider_id` / `reasoning_effort`（`hub.rs:484`），opener 不去存储里猜。让 opener 自己读会在 `SetModel` 之后拿到不一致的值。

8. **`compacting` 必须对客户端可见，开始与结束都要推。** 压缩期间 attach 照常受理，而快照里只有 `turn_running`（`wire.rs:427`），它此刻为假——接管或重连的客户端会把会话当 idle，允许用户输入，然后才被 `NotIdle` 拒掉。所以 `SnapshotPayload` / `Snapshot` 加 `compacting: bool`，composer 的进行中状态由它驱动。

   **推送要新增一个内部事件，不能复用现有那条路。** 我上一版说「复用既有 unsolicited `snapshot` 推送、不新增事件类型」是错的：那条通知是连接层在收到 `RelayEvent::Closed` 后**重新 attach 一次**才发出的（`bin/server.rs:1666`），而 `RelayEvent` 只有 `Event` / `Evicted` / `Closed` 三个变体（`hub.rs:81`），hub 手里根本没有 `send_notify`；借 `Closed` 更不行，那会把整个会话拆掉重连，还会撞上一次性的 `reattached` 守卫。

   正确做法是加 `RelayEvent::Snapshot(Box<SnapshotPayload>)`，连接层把它转成已有的 `snapshot` wire 通知——**wire 类型不变，内部事件类型要加一个**。注意它**不终止流**，而现有两个非 `Event` 变体都是终止的，连接层处理它时不能动 `streams` / `selections` / `reattached`。

   **开始也要推一次**：否则状态有两个来源——发起者靠本地记「我发了 RPC」，其他客户端靠快照字段，composer 要维护两套逻辑。开始推一次之后只剩一个来源。结束那次同时带上新写入的两条消息，中途接管的客户端两件事一次收齐。

9. **transcript 只有一个来源：快照事件。`compact` RPC 的响应只报告结果。** 压缩结束时，发起的那个客户端会同时收到快照事件和 RPC 响应。两边都往 transcript 里塞 `command` / `summary` 就会渲染出两遍——而且响应和推送的先后并不确定，谁先到都可能。

   前端的 `applySnapshot` 是**整体替换**（同一个 reducer 服务 `open_session` 结果与推送，`session.ts:2171`），本身幂等且自足；风险全在 RPC 响应这一侧。所以让响应根本不携带消息实体，而不是要求客户端按 `message_id` 去重——后者把正确性押在两处代码保持一致上，前者从形状上就不可能错。响应只回 `applied` / `recorded` / `stale` / `storage_error`，用来出提示和决定要不要提供重试。

   `SessionOpener::compact` 仍然返回那两条消息：hub 要用它们更新 `live.snapshot` 并拼出推送的 `SnapshotPayload`。**不转发**这一步发生在 RPC 层。

   **每一种终止都要推一次快照**，包括 `Stale` / `Storage` 这类一条没写的——否则 `compacting` 在客户端那边清不掉。

10. **成功与失败写入同样的结构，只差 `kind`——但仅限「写进去了」这一类结果。** LLM 失败时：用户输入的自定义指令不会凭空消失、transcript 里不存在「用户说了话没人应答」、重试无需清理（失败那对消息落在新边界之前，**自动从视图里掉出去**）。而 `Stale` / `Storage` 是另一类：一条消息都没写，`/compact` 那条也没有。结果类型必须把这两类分开，否则 UI 会去展示一条并不存在的记录。

11. **压缩请求以原始持久化 messages 为数据源，但先在服务端摊平成专用纯文本 transcript，不把 messages 原样重放给 provider，也不复用前端的 transcript/UI 模型。** 一次性消掉三个 provider 兼容性问题（tool definitions、reasoning continuation、孤儿 tool result）。代价是图片摊不平，只能记为附件占位。

## Risks / Open Questions

1. **最大风险：单条巨型 tool 输出把窗口冲爆，`/compact` 自己也发不出去。** 压缩请求要携带完整的当前视图，视图超窗时它同样超窗，压缩救不了自己。没有自动压缩意味着没有任何东西替用户盯水位，composer 上那个 context usage 指示器是唯一预警。兜底方案（摘要输入超窗时从最旧的 turn 开始丢弃直到装得下）留到需要时再做。正交的真正修法是给 tool 输出加截断上限——单独立项。

2. **摘要质量决定一切，而它不可测。** 用户按 `/compact` 时常常正做到一半，摘要必须写清「正在做什么、下一步是什么、哪些文件改过」。`compaction-prompt.md` 要明确要求覆盖未完成的工作。先验证：拿一个真实长会话跑一次，人读摘要判断够不够接着干。

3. **摘要降级成 user 消息后，视图开头可能出现连续两条 user 消息。** OpenAI 兼容接口允许，但 `kind = "deepseek"` 那条路径要实测。若某 provider 不接受，把 `CustomRole` 改成 `Assistant` 即可——这正是这个字段存在的意义，改一处配置而非改结构。spike 里一并验。

4. **没有中断路径。** 压缩发出后无法 abort，靠请求超时兜底，否则 `compacting` 标志会把 session 卡住。超时值当前为 600s（10 分钟）——长 transcript 的摘要确实可能耗时，放宽后代价只是 session 被占住更久，不会产生错误结果。

5. **压缩期间 session 被释放或驱逐。** 已由 Decision 6 的门闩 + CAS 覆盖，剩下的是**用户可见的行为**：断线重连后压缩可能报 `Stale` 而什么都没发生，用户得重按一次。可以接受（压缩本来就是显式操作），但 UI 的提示要说清「没执行」而不是「失败了」。回来时 entry 已被换掉的情况另外校验 generation，仅用于决定要不要更新快照——写回本身由 CAS 保证，不依赖 entry 还在。

6. **前端必须显式处理 `Custom`，否则静默消失。** `historyToEntries` 是 `in` 链加 `return []` 兜底，漏了不报错。好处是漏了也只是不显示、不会渲染错。

7. **未定：摘要用哪个 model。** 当前方案用 session 自己的 model binding。用更便宜的 model 能省钱，但要引入「压缩专用 model」这层配置。等有成本数据再说。

## Implementation Roadmap

- [ ] **[风险验证] 摘要请求形态 spike**
      在 `.scratchpad/compaction-spike/` 里，把一段真实的长对话（含 tool 调用往返）摊平成文本，用真实 provider 发一次 `System(压缩 prompt) + User(transcript + 自定义要求)`；再发一次 `System + User(摘要) + User(下一个任务)` 验证连续 user 消息。
      Purpose：验证 Decision 11 与 Risk 2、Risk 3。
      Verification：两次请求都返回 200；人工阅读摘要，判断「正在做什么 / 下一步 / 改过哪些文件」是否都在。

- [x] **[基础] 拆 `Message` / `RequestMessage` + `Message::Custom`**
      纯类型改动，先不涉及压缩逻辑：`coda_core` 定义与降级、`agent.rs:605` 组请求、`coda_openai` 换签名、`message_id_of` 与 `message_row_identity` 简化、`protocol.ts` 与 `historyToEntries` 跟进。
      Purpose：把「降级必须发生在进 provider 之前」变成编译期保证，并让后续压缩改动只剩逻辑。
      Verification：`cargo clippy` + `cargo test` 全绿；`pnpm --filter coda-web lint` + `test` 全绿；`storage_pg`（`--features pg-tests`）写入并读回一条 `role = "custom"` 的消息。
      **状态**：全部通过。`storage_pg` 那条是 `a_custom_message_round_trips_under_its_own_role`，顺带断言 `role = "custom"` 不被 `role = 'user'` 的过滤命中——rewind 目标与 turn 边界都靠那个过滤。

- [x] **[核心逻辑] `coda_agent::compaction::view` + `Agent::messages()`**
      Purpose：把压缩规则做成一份可单测的纯逻辑，并接进唯一的视图组装点。
      Verification：单测覆盖 —— 无 compaction 消息时视图 == 全量；有一条时从它切起且它本身保留；有多条时取最后一条；`compaction_failed` 不构成边界；sub-agent 线程（无此类消息）得到全量。
      **状态**：5 条 `coda_agent::compaction` 单测通过；默认无边界路径也由既有 session/runtime 测试覆盖。

- [ ] **[存储] `commit_compaction` 事务 + `message_count` CAS**
      Purpose：确认两条消息 + `message_count` 推进是原子的（推进**不能漏**——漏了下一次 `save_checkpoint` 会按旧 count 算 seq、与新行撞主键，整个 checkpoint 写失败），且陈旧提交被拒绝。
      Verification：`storage_pg` 测试 —— **正常路径必须真的写进去**（这条同时钉住 `thread_id` 用的是 session id：写成 `"root"` 会让 CAS 影响 0 行，表现为永远 `Stale`）；写入后 `load_checkpoint` 的消息数与 `message_count` 一致；随后再跑一次正常 checkpoint 保存不冲突；**读基线之后先追加一条消息再提交，必须返回 `Stale` 且一条都没写**；session 已删除时 CAS 返回 `Stale` 且不复活任何行；rewind 到压缩之前时两条消息一起消失且视图恢复全量。
      **状态**：实现与 3 条 compaction PostgreSQL 测试已完成，并通过 `--all-features` 编译；当前环境没有 `DATABASE_URL`，尚未实际执行数据库断言。

- [x] **[集成] `SessionOpener::compact` + hub 门闩 + RPC**
      含闲置检查、`EntryState.compacting` 门闩五处、model binding 显式传参、放锁、generation 校验、快照更新与推送、完成后补 `maybe_release`。
      Purpose：打通链路，确认长时间的 LLM 调用不会堵住 session，也不会让 entry 在脚下消失。
      Verification：`hub_tests` —— 压缩期间 `task` / `set_model` / `fork` / **`rewind`** 全部返回 `NotIdle` 且 attach 不被阻塞；**压缩期间 detach 不释放 entry，压缩完成后才释放**；**压缩开始收到一次 `compacting == true` 的快照事件，结束收到一次带新消息且 `compacting == false` 的，且这两次都不终止事件流**；压缩期间 attach 拿到的快照 `compacting == true`；LLM 失败路径写入两条消息但视图不变；`Stale` 路径一条不写且 transcript 无残留；非闲置时 `compact` 什么都不改。
      **状态**：新增 5 条 `hub::tests::compaction` 集成测试，以上门闩、takeover、快照、释放、失败与 stale 路径全部通过。

- [x] **[集成] web：`/compact ` 前缀识别 + `Custom` 渲染**
      Purpose：把能力交到用户手上。
      Verification：`pnpm --filter coda-web test` 覆盖 —— `/compact`、`/compact 只保留架构决策` 走命令路径；`/compact` 出现在句中或作为普通文本的一部分时**不**走命令路径；`Custom{kind:"compaction"}` 渲染成可辨识的分隔条目而非普通气泡。
      **状态**：命令解析、RPC 路由、快照 busy 状态和 `Custom` 分隔条目的 4 条测试通过；完整 web suite 为 96 条通过。

- [x] **[收尾] `cargo clippy` + `cargo test` + `pnpm --filter coda-web lint` + `pnpm --filter coda-web test`**
      Verification：全绿。
      **状态**：默认 workspace 全绿；`cargo clippy --workspace --all-targets --all-features` 也通过。带 `pg-tests` 的运行仍受上一项所述数据库环境限制。
