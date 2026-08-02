# 先存档，再对外说「做完了」（第一部分：落库顺序）

## Problem

一轮对外宣布结束时，它的内容（含 sub-agent）必须已经在数据库里。需求见 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md)。

**这份设计只交付需求的一部分。** 设计过程中发现，要让这条保证在中止和「新任务顶替」两条路径上也成立，得先把这个系统本身就已破损的取消语义重做一遍——那是个远大于本需求的子系统，单独立项于 [`turn-cancellation.md`](turn-cancellation.md)。本文档做的是它的地基，也是日常路径上收益最大的那一半。

**本文档兑现的**：一轮在**没有被中止、也没有被新任务顶替**的情况下结束时，该轮全部内容（含 sub-agent 线程）已经在数据库里；写不进去时明确报错，绝不伪装成成功。

**本文档不兑现的**（留给第二部分）：需求 Success Criteria 里「中止也算一种收尾」那一条，以及「拆掉 fork 的重试与落库校验」那一条。**fork 现有的补偿必须原样保留**——中止和顶替仍会让数据库落后于界面。

## Scope

**In**

- `AgentLoop::run` 结构调整：把「宣布这轮结束」的事件和「把结果回给调用方」的投递，都推到 checkpoint 写完之后。
- `save_checkpoint` 从「失败只记日志」改成失败可见，并让调用方据此放弃对外宣告。
- 写失败的出路：新事件 + hub 强制重同步 + web 会话级错误横幅。

**Out**

- 中止路径、顶替路径的因果链——全部归 [`turn-cancellation.md`](turn-cancellation.md)。
- **不拆 fork 的落库校验与客户端重试。** 拆它的前提是第二部分落地。
- 不动 hub 的结算账目（`fold_settled_turn` 的按序弹出）。改它是第二部分的事。
- 不改数据库结构，不改 `fork` / `rewind` 的对外行为。
- 一轮跑到一半不作保证（工具刚跑完等时刻）。

## Assumptions

- checkpoint 写入是增量的（`write_checkpoint` 只插 `stored_count..` 那一段），所以「写完再宣布」的额外开销是一次事务，不是全量重写。
- 数据库写失败是罕见的、通常伴随整体故障的场景。不为它设计精细的降级态。
- sub-agent 的回复必然经过 `reply_target`；没有别的把结果交回调用方的通道。**但调用方并不只被回复唤醒**——中止和新 `Task` 也能唤醒它，这正是本文档不覆盖的那两条路径。

## Validation Findings

**问：正常路径上，调用方会不会跑到 sub-agent 落库之前？** 会。sub-agent 在 [`driver.rs:759`](../../crates/coda_agent/src/runtime/driver.rs) 先投递 `Reply`，之后才在 `run` 末尾 `save_checkpoint`。调用方被 Reply 唤醒后可以跑完剩下半轮。**含义**：把投递挪到写之后，这条边就成立——一轮不被外力打断时，调用方除了这条 Reply 没有别的方式往下走。

**问：一轮只发一个结束信号吗？** 不是。主 agent 在生成中被中止时，先发 `Aborted(Generation)`（:708），随后落进 `Err` 分支又发一个 `Error("Aborted by user")`（:851）——两个都被 hub 判为 settle（:709 的 TODO 就是说这件事）。**含义**：`run` 收归成唯一出口后顺手解决它。

**问：写失败之后把这轮挂着不结束，会怎样？** 会把会话卡死。`turn_running` 永远为真，fork/rewind 拒绝（这是想要的），但 `maybe_release`（[`hub.rs:707`](../../app/coda_server/src/hub.rs)）也永远不放行，连断开所有客户端都清不掉，只能重启进程。**含义**：不能简单地「不宣布就完事」，得有出路——强制重同步。

**问：`PersistFailed` 事件发给客户端就够了吗？** 不够。web 的事件 reducer 是个没有 `default` 分支的 `switch`（[`session.ts:669`](../../app/coda_web/src/store/session.ts)），没写分支的新事件会被静默丢弃，且 [`protocol.ts`](../../app/coda_web/src/lib/protocol.ts) 的事件联合类型是封闭的。更关键的是，`force_resync` 会让客户端重连（[`server.rs:1467`](../../app/coda_server/src/bin/server.rs)），而快照**整个替换** `entries`（[`session.ts:1267`](../../app/coda_web/src/store/session.ts)）——所以哪怕渲染出来，错误行也会立刻被擦掉。好消息是 `applySnapshotToSession` 用的是 `...session` 展开（[`session.ts:1251`](../../app/coda_web/src/store/session.ts)），**只要错误状态不放在 `entries` 里就能活过重连**。**含义**：错误做成会话级横幅，放在 `entries` 之外。

## Alternatives Considered

**待投递的结果放哪儿：`ResumePoint` vs `AgentLoopState`。** 放 `ResumePoint` 意味着它跟着 checkpoint 落库，崩溃后理论上能补投。放弃它，理由有二：一是 fork 的 busy 判定是 `resume_point != Generation`，一个挂着未清理待投递的线程会被判成忙，而 stateless sub-agent 的线程投完就再不会被驱动、永远清不掉，整个会话就永远 fork 不了；二是那份持久义务在硬崩溃后根本没人读——重驱动线程要靠 runtime snapshot 里的 `active_thread`，而硬崩溃时那份快照压根没写。选 `AgentLoopState`：义务只活在内存里，风险窗口从「隔着一次数据库往返」缩到「写返回到 channel send」这一瞬。

**写失败后：留着会话降级运行 vs 强制重同步。** 留着的好处是数据库恢复后，下一次成功的 checkpoint 会把这轮的尾巴一并补写（历史是追加的）。但它要引入一个新的「活着但不可信」状态，还得让 fork / rewind / 导出各自认得它。选强制重同步：写不进去就等于内存里的东西是假的，直接走 hub 已有的 `force_resync`——和事件流 lag 时同一条路，客户端回到数据库里真实存在的状态。

## Components

- **`persist_and_announce`（新）** — 「先存档再对外」这条规矩的唯一实现处。`run` 里所有 checkpoint 写入都经它：写成功才把欠外面的东西兑现出去，写失败就报一次错、停轮。规矩因此是结构性的，加新的写入点也漏不掉。
- **`TurnEnd`（新）** — 描述这轮怎么收的场：要发哪个事件、要不要回话回给谁。由各个 handler 产出，交给 `persist_and_announce` 兑现。可以为空。
- **hub 转发器（改）** — 认得 `PersistFailed`，走已有的强制重同步。
- **web 持久化错误横幅（新）** — 会话级状态，放在 `entries` 之外，让失败原因活过强制重同步带来的那次重连。

## Interfaces

```rust
/// 一轮结束时欠外面的东西。handler 只描述，不执行——
/// 兑现它的唯一地方是 `run`，且只在 checkpoint 写成功之后。
struct TurnEnd {
    /// 宣布这轮结束的事件（LLMEnd / Aborted / Error / Suspended）。
    /// `None` 表示这次退出不对外宣告（例如收到意料外的信封）。
    event: Option<AgentEvent>,
    /// 把结果交回调用方。只有 sub-agent 有；根 agent 恒为 `None`。
    reply: Option<Envelope>,
}
```

```rust
/// 写入本线程的 checkpoint，写成功了再把欠外面的东西兑现出去。
///
/// `run` 里**所有**的 checkpoint 写入都走这一个口子——今天有三处
/// （[`driver.rs:293`] 根 `Task` 入场存用户消息、`:298` `handle_envelope`
/// 提前返回、`:369` 收场），将来加第四处也一样。规矩因此是结构性的，
/// 不是靠逐个调用点记得遵守。
///
/// `owed` 可以是空的（入场和提前返回那两处就没有东西要宣告），
/// 空值是合法输入，不是需要另开分支的特例。
///
/// `suspended_at` 跟着传：它是 `run` 的局部状态（进 `PendingApproval` 时才
/// 赋值），而 `StoredCheckpoint` 要它，从 `self` 和 `resume_point` 还原不出来。
///
/// 返回 `false` 表示写失败、这一轮就此打住。
async fn persist_and_announce(
    &self,
    resume_point: ResumePoint,
    suspended_at: jiff::Timestamp,
    owed: TurnEnd,
) -> bool;
```

**它的两条分支就是本设计的全部不变量：**

```text
Ok  → emit owed.event → send owed.reply
Err → emit PersistFailed（恰好一次） → 停轮
```

`Ok` 那条里 **Reply 必须最后**：先投递的话调用方会被立刻唤醒，抢先发出自己的结束事件，本线程的结束事件就迟到落进下一轮的事件日志，被当成下一轮的内容重放。测试要在这两步之间刻意让出调度，确认上游的结束事件不会超车。

`Err` 那条里 **停轮是彻底的**：不再调 LLM、不再跑工具、不发结束事件、不投递结果。入场那次失败就等于这一轮压根不开始——用户的提示词都没存下来，继续往下跑只会在一个开头都不存在的轮次上堆内容，还白烧 token。「恰好一次」是要求：同一次失败不能既在写入点报一遍、又在收场路径上再报一遍。

```rust
/// 写入本线程的 checkpoint。
/// `Ok` 只证明**本线程**的内容已落库，不代表整轮。
/// 只由 `persist_and_announce` 调用，不对外直接使用。
async fn save_checkpoint(&self, ..) -> Result<(), String>;
```

```rust
/// 新事件：这轮的内容没能写进数据库。
/// **不是**结束信号——收到它的人不能认为这轮完成了。
AgentEvent::PersistFailed(String)
```

**两层保证，别混为一谈。** `save_checkpoint(Ok)` 是线程级事实。整轮的保证是拼出来的：一个线程只有存了档才回话，调用方只被已落库的回话唤醒，逐层递归，根 settle 那一刻整棵树都已落库。

**这条链有三个断点，本文档只堵住「什么都没发生」的那种情况**：中止、新任务顶替、以及对已派发调用的就地合成，都会切断它。它们归 [`turn-cancellation.md`](turn-cancellation.md)。这也正是 fork 的补偿必须留着的原因。

## Data Model

不新增持久化实体。两处新的进程内状态：

- `AgentLoop` 内的 `TurnEnd`：单个 task 独占，生命周期不超过一次 `run`。
- web 会话状态上的持久化错误横幅：放在 `entries` **之外**。`applySnapshotToSession` 是 `...session` 展开，所以重连快照不会冲掉它。

没有跨 agent 的共享可变状态——本文档不引入任何一处。

## Load-Bearing Decisions

1. **待投递的结果只活在内存里（`AgentLoopState`），不落库。** 换来的是不碰 `ResumePoint`、不碰 fork 的 busy 判定、不用写两次。代价是「写完了、还没投出去」这一瞬崩溃，那一轮作废——由用户的下一条消息通过既有逃生口（`ToolExecution` 收到 `Task` 的分支）清理。
2. **结束信号收敛成每轮一个。** `run` 是唯一发出口，一次 `run` 最多一个。顺带修掉中止时 `Aborted` + `Error` 双发。
3. **所有 checkpoint 写入统一走 `persist_and_announce`，写失败一律 fail-fast、恰好报一次。** 关键在「统一」：规矩若按调用点逐个约定，就永远要问「覆盖全了没有」——今天有三处，将来加第四处就会漏。收进一个 helper 之后，空 `TurnEnd` 成为合法输入而不是空档，规矩变成结构性的。失败即刻停轮：不调 LLM、不跑工具、不发结束事件、不投递结果。这和决策 4 一致：写不进去就说明内存里的东西是假的，继续跑只是在假的基础上堆更多假的。
4. **写失败 ⇒ 强制重同步；错误另走一条不被快照覆盖的通道。** 不引入「活着但不可信」的中间态。错误以会话级横幅呈现，落在 `entries` 之外：`PersistFailed` 时置上，用户手动关闭、或该会话之后有一轮正常结束时清除；普通快照不清除它。
5. **fork 的补偿原样保留。** 拆它是需求的验收标志，但那要等第二部分。这份文档单独上线时，`ForkError::Lagging`、`ForkOutcome::Retryable`、`retryWhileNotReady` 一个都不动。

## Risks / Open Questions

- **别把这份文档当成需求已完成。** 它兑现的是「不被打断的一轮」，中止和顶替仍会让数据库落后于界面。验收要按上面「本文档兑现的」那一句去验，不要按需求的 Success Criteria 整体去验。
- **审批挂起要不要一起管？** `Suspended` 在 hub 眼里也是 settle 信号。fork 和 rewind 都被 `pending_approvals` 挡着，所以现在没有暴露的读者。设计上把它一并纳入（`run` 统一「先写后宣告」比开个例外简单），代价是审批弹窗晚一次数据库写。如果实测觉得这个延迟碍事，可以退回去只管另外三个。
- **`PersistFailed` 会不会太吵。** 数据库短暂抖动就把会话踢去重同步，用户会看到一次「跳回上一个已存状态」。要观察实际频率；如果太吵，可以考虑加一次有界重试，但那会给关键路径加延迟，先不做。

## Implementation Roadmap

- [x] [风险验证] 给 `driver_tests/fixtures.rs` 的 `TestStorage` 加两个开关：卡住写入、让写入失败。写一个带 sub-agent 的用例，卡住 sub-agent 的写，断言调用方的结束事件没有提前出现
      Purpose: 验穿「存档早于回话」这条边——本文档的全部价值都在它上面
      Verification: 卡住时无结束事件；放开后才出现，且顺序正确

- [x] [核心] 引入 `TurnEnd` 与 `persist_and_announce`，把 `run` 里三处 checkpoint 写入（`:293` / `:298` / `:369`）全部收编，并把 `LLMEnd`(终局) / `Aborted` / `Error` / `Suspended` 的发出和 sub-agent 的 `Reply` 投递从各 handler 收归它，按「写 → 发本线程结束事件 → 投递 Reply」兑现
      Purpose: 把「先存档再对外」落成一处结构性规矩，顺带把每轮结束信号收敛成一个
      Verification: 上一步的用例转绿；新增用例断言 sub-agent 的结束事件严格早于其 `Reply` 触发的下游事件；`abort.rs` / `approval.rs` / `subagent_origin.rs` 不回归；中止时不再同时出现 `Aborted` 和 `Error`；代码里 `save_checkpoint` 除 helper 外无其他调用点

- [x] [核心] `save_checkpoint` 改为返回 `Result`；helper 在 `Err` 时发恰好一次 `AgentEvent::PersistFailed` 并立刻停轮（不调 LLM、不跑工具、不发结束事件、不投递结果）
      Purpose: 让写失败无法伪装成成功；收进 helper 之后，「还没有 `TurnEnd`」不再是一种需要特判的状态
      Verification: 四个用例，三个写入点各一条加一条重复性断言——(a) `:293` 入场写用户消息失败，断言这一轮压根没调 LLM、没跑工具；(b) `:298` `handle_envelope` 提前返回时写失败；(c) `:369` 收场写失败，断言既无结束事件也无 `Reply`；(d) 三条路径都断言 `PersistFailed` 恰好一次，不重复

- [x] [集成] `WireEvent::PersistFailed` + `event_settles_turn` 返回 false；hub 转发器收到它先转给客户端、再走 `force_resync`
      Purpose: 给写失败一条不卡死会话的出路
      Verification: hub 用例断言会话被 drain 并从持久状态重建，客户端先收到错误事件

- [x] [集成] web：`protocol.ts` 加联合成员、reducer 加分支，失败原因写进会话级横幅字段（在 `entries` 之外，靠 `applySnapshotToSession` 的 `...session` 展开活过重连）；用户手动关闭或该会话之后有一轮正常结束时清除
      Purpose: 让用户真的看见错误，而不是被紧随其后的重连快照擦掉
      Verification: 前端集成测试走完 `PersistFailed → Closed → snapshot`，断言横幅在重连后仍在；另一个用例断言下一轮正常结束后横幅消失

## Deviations from Design

- **第一步和第二步是一起验收的。** 第一步的 Verification（「卡住时无结束事件；放开后才出现」）描述的是修好之后的行为，所以第一步交付时那个用例是**红**的——红本身就是风险验证的结论：卡住 `explore` 的写之后，根 agent 在 0.00s 内就结束了整轮，竞态确定性复现，不靠时序碰运气。第二步落地后转绿。
- **`AgentLoopState::Done` 里的 `TurnEnd` 装了箱。** 不装的话这个枚举涨到 648 字节，clippy 的 `large_enum_variant` 会报——在 async fn 里它还会撑大 future。设计里没提，属于实现细节。
- **一个既有 hub 用例的前提被本次改动消掉了，屏障改用另一个窗口守。** `a_rewind_waits_out_a_sub_agent_that_replied_before_it_saved` 断言的是「根轮次 settle 时 sub-agent 还没存完」，而这正是本次要消除的窗口。已改名为 `a_rewind_cannot_race_a_sub_agents_checkpoint_write` 并把断言反过来：settle 时那份 checkpoint 必须已经在库里。它原来还兼职守着 `handle_rewind` 开头那次 shutdown，反转之后守不住了——所以另加了 `a_rewind_waits_out_a_sub_agent_a_superseded_turn_left_behind`：**顶替**（用户在 sub-agent 写到一半时发下一条消息）仍会让根轮次不等它就 settle，那道屏障眼下正是靠这个窗口在挡事。已实测去掉 `shutdown` 后新用例会挂。这个窗口本身归 [`turn-cancellation.md`](turn-cancellation.md) 关掉。
- **前端 `turnComplete` 顺带补齐了 `!aborted`。** 服务端的 `event_settles_turn` 一直排除 aborted 的 `LLMEnd`，前端没有——中止生成时那条部分消息会被当成一轮正常结束。原本只影响 `running`，但横幅要靠「下一轮正常结束」来清除，不修就会被中止误清。
- **前端横幅没有在浏览器里跑过。** 触发它需要一个会拒绝写入的数据库，preview 里造不出来。验证靠 reducer 单测（5 条，覆盖不结束轮次、活过重连、下一轮正常结束时清除、sub-agent 结束不算、中止不算）加 `tsc --noEmit` 和 oxlint。
