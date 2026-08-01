# 按轮次的取消与顶替（第二部分）

## Problem

这个系统的取消语义本来就是破的，只是一直被 fork 的兜底校验遮着。中止之后 sub-agent 还会继续开工并写库；新任务顶替在跑的轮次时，事件账目对不上、`unsettled_user_messages` 会永久残留（而它正是 fork 的拒绝条件之一）；挂起等审批的线程根本没有被中止叫醒的途径。

这些今天就在发生。之所以现在必须处理，是因为 [`persist-before-visible.md`](../implementation/persist-before-visible.md) 建立的那条「存档早于回话」的因果链，在这三处会被切断——而拆掉 fork 兜底校验（原需求 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md) 的验收标志）的前提，正是这条链在所有路径上都成立。

**依赖**：本文档建立在第一部分之上，收场协议直接复用它的 `TurnEnd` 和 `save_checkpoint -> Result`。第一部分必须先落地。

**状态**：尚未可批准。Open Questions 里有三个待决点——活跃轮次顺序的权威来源、`TurnId` 送到 hub 的契约、`pending_reply` 的存活判据——都要先做 spike 才谈得上定稿。

## Scope

**In**

- 取消从一次性控制广播改成按轮次的状态，且能主动驱动挂起线程。
- 收场协议：逐层递归等真回话，只有根发出能 settle 的信号。
- 顶替协议：新任务走同一套收场，就地合成收窄到「下游确认已死」。
- 信箱仲裁：`run_agent` 持 deferred FIFO，按当前轮次决定放行哪些信封。
- hub 的结算账目改成按 `TurnId` 关联，不再按序弹出。
- 重启恢复时把活跃轮次重建出来。
- 全部落地后，拆掉 fork 的重试与落库校验。

**Out**

- 第一部分已交付的那些（`TurnEnd`、`save_checkpoint -> Result`、`PersistFailed`、强制重同步、web 横幅）。
- 不追求分布式意义上的严格一致。

## Validation Findings

已在代码里逐条确认。

**取消是一次性广播，管不住排队的、树状的工作。** 派发是先发信封、后登记 `pending_replies`（[`driver.rs:935`](../../crates/coda_agent/src/runtime/driver.rs) / `:960`），中间有段窗口谁都不知道这份工作存在；空闲 agent 收到 `Abort` 直接 `continue` 忽略（[`driver.rs:96`](../../crates/coda_agent/src/runtime/driver.rs)），随后照样取出排队信封跑完一整轮；运行中的 agent 那条 select 分支把 `Abort` 消费掉了（[`driver.rs:137`](../../crates/coda_agent/src/runtime/driver.rs)），收尾后转回空闲等待，照样开跑队列里的第二个。同名 stateless sub-agent 的并发调用全排在同一条队列上（只有 stateful 的并发调用会被拒，[`driver.rs:876`](../../crates/coda_agent/src/runtime/driver.rs)），所以「一个在跑、一个排队」是常规情形。

**取消走不到挂起线程。** sub-agent 发完 `Suspended` 后 `suspended_thread = active_thread.take()`（[`driver.rs:181`](../../crates/coda_agent/src/runtime/driver.rs)），停在信封等待里——没有 Resume 信封就永远不会再进 `run()`。用户不审批直接发下一条消息（一个再正常不过的操作）时，它不检查标记、不收场、不回话。

**中止时主 agent 主动切断因果链。** 它自己把 `pending_replies` 全部写成 Aborted 的 ToolMessage（[`driver.rs:1068`](../../crates/coda_agent/src/runtime/driver.rs)）就宣布结束，而 sub-agent 们各自在收尾、各自写库。

**新任务同样切断它，而且这在正常路径上。** 提交任务的路径没有 `turn_running` 门禁（[`hub.rs:768`](../../app/coda_server/src/hub.rs)），:781 的注释还写明「轮次进行中提交新任务会顶掉待审批的调用」是有意为之。新 `Task` 和 `Reply` 进同一条 `envelope_rx`，`Task` 抢先时 [`driver.rs:502`](../../crates/coda_agent/src/runtime/driver.rs) 就地合成 Aborted 并直接开新一轮。

**中止不能就地由收到 Resume 的那个 agent 收场。** `PendingApproval` 带 `agent_name`（[`agent.rs:53`](../../crates/coda_agent/src/agent.rs)），审批可属于任何 sub-agent；而 `event_settles_turn` 只认根 agent 的 `Aborted`（[`hub.rs:313`](../../app/coda_server/src/hub.rs)）。sub-agent 改不了根线程的 `pending_replies`，发的事件也不会让 hub settle。

**hub 说「在跑」时 agent 未必已开跑。** `handle_resume` 只要 `session.resume()` 返回成功就把 `turn_running` 置真、把 pending approval 摘掉（[`hub.rs:925`](../../app/coda_server/src/hub.rs)），而那只表示信封进了 channel。

**活跃轮次可能同时有多个。** hub 允许任务排在运行中的轮次后面（[`hub.rs:1307`](../../app/coda_server/src/hub.rs)），`Session::send` 一发就进 channel。

**hub 的结算账目是按序弹出的，不是按轮次关联的。** `fold_settled_turn` 每次 settle 从 `unsettled_user_messages` 弹一条。`Suspended` 本身就是 settle（[`hub.rs:312`](../../app/coda_server/src/hub.rs)），已经把旧轮那条弹掉了；挂起期间提交新任务后，再为旧轮补发 `Aborted`，弹掉的就是**新任务**那条，并把 `turn_running` 错误清零。**含义**：光给旧轮补一个结束事件不够，账目本身得改成按 `TurnId` 关联。

**顶替协议在现有边界里写不出来。** `run_agent` 从 `envelope_rx` 取出信封就交给 `AgentLoop::run`，取的那一刻已经消费掉了，而 `run` 够不着 receiver。而且 `TurnOutcome::Completed` 同时覆盖「真结束」和「停下来等剩余回话」两种情况，`run_agent` 无从判断何时重放 FIFO。

**重启恢复推不出轮次顺序。** `AgentRuntimeSnapshot` 把信封按 agent 名分桶存在 `HashMap` 里，跨 agent 的入队顺序本来就没保留；`TurnId` 底下是 `Uuid::new_v4()`（[`llm.rs:19`](../../crates/coda_core/src/llm.rs)），自身不含顺序；`Reply` 不带 `turn_id`。而 restart-resume 的 Resume 信封由 `run_agent` 内部构造（[`driver.rs:54`](../../crates/coda_agent/src/runtime/driver.rs)），`init_envelopes` 更是在 spawn **之前**就塞进 channel（[`runtime.rs:279`](../../crates/coda_agent/src/runtime.rs)）——两条都绕过 `Session`。

## Alternatives Considered

**另造一套持久化屏障 vs 把因果边接回来。** 屏障那条路走过两版，都废弃了：先是数「有几个 agent 在跑」——派发在登记之前，已入队未开工的 sub-agent 根本不在计数里，而且计数只反映「人走了」不反映「存没存进去」；改成按 `TurnId` 登记执行义务、注销带成败之后，又发现它管不了 sub-agent 的审批恢复。屏障要盖住这些就得再加一条沿调用树往上传的取消链，而一旦有了那条链，屏障本身就多余了。所以选**把因果边接回来**。代价是中止不再「立刻」，且等待随调用树深度累加。

## Load-Bearing Decisions

1. **取消是按轮次的状态，不是一次性广播；标记由 runtime 在广播前装好，且既拉又推。** 「谁消费了控制消息」这个竞态因此整个消失。取消走到一个线程有两条路：它即将开工（取信封时查标记），或它停着且没有信封会来（取消主动推它——目前只有「已发 `Suspended` 停在 `suspended_thread`」这一种）。各 agent 一律不自行推断该取消哪一轮：stateless agent 的 `Agent` 实例跨线程复用，空闲时 `current_turn()` 留着的是上一轮。
2. **取消沿调用树往上收，每一层都等自己的孩子，只有根 settle。** 已派发的 sub-agent 调用**不得就地合成结果**——某个中间节点一旦替孩子写了结论，链就在那里断了。就地合成只留给「下游确认已死」；**判据本身是待决项**（见 Open Questions 三）：按轮次判太粗，得下沉到「这个 `call_id` 在本进程有没有活着的 producer」。
3. **新任务顶替走同一套收场协议，并为此把信箱仲裁上提到 `run_agent`。** 用户可见语义不变（新消息照旧顶掉在跑的工作），变的是「顶掉」从就地写结论变成等真回话。`AgentLoop::run` 要能拒收信封、`TurnOutcome` 要区分「真结束」和「等回话」、`run_agent` 要持 deferred FIFO——这些不是实现细节，没有它们协议无处落地。
4. **hub 的结算账目改成按 `TurnId` 关联。** `unsettled_user_messages` 从 FIFO 改成按轮次索引，`fold_settled_turn` 只弹自己那一条，`turn_running` 由活跃轮次集合推导而非维护布尔值。不改这个，给旧轮补结束事件反而会吃掉新任务。**前置条件是把 `TurnId` 送到 hub**——现有事件边界上没有它，契约待定（见 Open Questions 二）。
5. **等不到回复按持久化失败处理，绝不放行成功的结束信号。** 上限只设在根一处；中间节点等不到孩子就一直等，根超时后走 `PersistFailed` + 强制重同步，drain 把底下那些一并收走。
6. **全部落地并集成之后，才拆 fork 的补偿。** 那是原需求的验收标志，也是这两份文档合起来是否成立的唯一硬证据。

## Open Questions

三个待决点，都得在动代码之前解掉。

### 一、活跃轮次顺序的权威来源

中止要取消「队首那一轮」，就得知道顺序；而重启后现有快照推不出来（见 Validation Findings 最后一条）。候选：

- **把有序活跃轮次持久化进 runtime snapshot。** `runtime_snapshots.snapshot` 是 jsonb，加字段不算改表结构。最直接，但要想清楚崩溃时它和实际入队状态的偏差怎么收敛。
- **从根线程的 `messages.seq` 推。** 根线程的消息本来就有序且带 `turn_id`。问题是排队未开工的轮次还没有消息行（用户消息要等根取出信封才写），推不全。
- **承认重启后不需要完整顺序。** 硬崩溃本来就把在跑的工作全带走了，重启后能跑的只有被回放的那些。要论证「这种情况下队首唯一」是否恒成立。

### 二、`TurnId` 送到 hub 的契约

「hub 按 `TurnId` 结算」目前只是个目标，还不可实现：事件通道上只有 `agent_name` + `thread_id`（[`runtime.rs:214`](../../crates/coda_agent/src/runtime.rs)），而根线程的 `thread_id` 就是 session_id、跨轮不变，hub 手上没有任何能区分轮次的东西。要定的有：

- `TurnId` 怎么从 driver 走到 hub——进 `SessionEvent` 本体，还是包一层内部的 tagged event？
- `EventLog` 怎么保留它，`fold_settled_turn` 又怎么用它定位该弹哪一条。
- 同一轮**多次 settle** 的幂等语义：`Suspended` 已经是一次 settle，之后这一轮还会再 settle 一次（`Resume` 后正常结束，或被顶替后补发 `Aborted`）。第二次不能再弹用户消息，更不能弹到别轮头上。
- `turn_running` 最好由活跃 `TurnId` 集合推导，而不是继续维护一个布尔值——布尔值天然表达不了「旧轮在收场、新轮已排队」。

### 三、`pending_reply` 的存活判据

决策 2 里那个判据——「该轮在活跃列表里 ⇒ 下游还活着」——**粒度错了**。轮次活跃只证明这一轮**至少有一项**工作被恢复，证明不了**每一个** `pending_replies.call_id` 都有对应的 producer。部分恢复时会出现一半调用还会回话、另一半的 producer 已经永久没了，按轮次统一等待就会等一个永远不来的回话，最后误走 `PersistFailed`——而这恰好打掉第一部分所依赖的那条清理路径（崩在「存完未投递」之后，用户下一条消息能正常清理残局）。

所以判据得下沉到 call/thread 粒度：**这个具体的 `call_id` 在本进程里有没有一个活着的 producer？** 有就等，没有就地合成。这可能意味着「有序活跃轮次」要升级成一份按 call/thread 记录的可恢复工作清单。

### spike 要回答的

造一个「两轮活着 → 重启 → 立刻中止」的场景，同时验：

- 三个候选方案各自能不能推出正确的队首；
- 每个 `pending call` 能否对应到本进程里的具体 producer；
- 没有 producer 的调用能不能安全地就地合成；
- runtime snapshot 是一次性消费，还是可能在后续重启里被再次回放（这决定了清单的收敛方式）；
- 「有序活跃轮次」是否必须升级成按 call/thread 的工作清单。

结论回填本文档之后，才谈得上批准。

## Implementation Roadmap

前提：第一部分已落地。

- [ ] [spike] 在 `.scratchpad/turn-recovery/` 造「两轮活着 → 重启 → 立刻中止」的场景，回答 Open Questions 里的全部三条：队首顺序、`TurnId` 到 hub 的契约、以及每个 `pending call` 能否对应到本进程的具体 producer
      Purpose: 解掉三个未决点，它们共同决定后面所有步骤的数据模型
      Verification: 结论回填本文档并重新评审；特别要给出「有序活跃轮次」是否需要升级成按 call/thread 的可恢复工作清单
      注意: 回填时**不能只改 Open Questions**。下面几步里写死的「有序活跃轮次集合」和决策 2 里「按轮次判断能否就地合成」都是待决点的占位说法，spike 有结论后必须一并改写，否则文档会自相矛盾

- [ ] [核心] `AgentRuntime` 加活跃轮次的有序唯一集合与 `enter_turn`（入队前登记、返回 guard、`Drop` 只回滚本次新增项、成功后 `commit`；`Resume` 靠新增的 `PendingApproval::turn_id`）；`request_abort` 先原子地把队首放进取消集合、再广播
      Purpose: 让取消标记有可靠来源且装得够早
      Verification: 强制「排队信封所在 agent 先处理 Abort、根后处理」的调度顺序仍能查到标记；排在后面的轮次不被取消；`send_message` 失败不留幽灵轮次；挂起期间 `Resume` 重复登记幂等

- [ ] [核心] `bootstrap` 按 spike 的结论重建活跃轮次，必须在 agent task 能看见信封**之前**完成
      Purpose: 重开会话后立刻中止时，`cancel_active_turn` 面对的不能是空列表
      Verification: 关闭重开、自动恢复 sub-agent 审批后立刻中止，断言取消生效

- [ ] [核心] 收场协议：取消既拉又推（空闲 agent 检查自己的 `suspended_thread`）；已取消的线程记 Aborted → 等齐已派发的下游真回话 → 存档 → 发事件 → 回话；根等 `pending_replies` 收齐再发 `Aborted`
      Purpose: 把中止接回因果边，并让它逐层可传递
      Verification: 三层树 `root → A → B` 卡住 B 的写、根不提前发事件；根自己的审批 `Resume → Abort`；sub-agent 的审批 `Resume → Abort`；**sub-agent 等审批时不发 Resume 直接中止**，断言被推着收场、不走 `PersistFailed`；同 agent 一跑一排队；排队的 `Task` 属于另一轮、中止后仍会跑

- [ ] [核心] 信箱仲裁：`TurnOutcome` 区分「真结束」与「等回话」，加 `Deferred(Envelope)`；`run_agent` 加 deferred FIFO，旧轮收场前只喂 `Reply`，收场后按原序重投
      Purpose: 给顶替协议落脚点
      Verification: 被扣下的 `Task` 在旧轮收场后按原序重投，期间的 `Reply` 照常处理

- [ ] [核心] 顶替协议：新 `Task` / `ToolCall` 到达正等回话的线程时走收场协议再开新工作；就地合成收窄到「该轮不在本进程活跃列表里」
      Purpose: 堵掉正常路径上的断链点，同时保留崩溃恢复的清理路径
      Verification: 卡住 sub-agent 的写并提交下一条 `Task`，新一轮的结束事件不提前出现；挂起不审批直接发新 `Task` 也能正常收场；崩溃恢复场景仍走就地合成不挂住

- [ ] [集成] hub 结算改成按 `TurnId` 关联：`unsettled_user_messages` 按轮次索引，`fold_settled_turn` 只弹自己那条，`turn_running` 按轮次判定
      Purpose: 让旧轮补发的结束事件不再吃掉新任务
      Verification: 「挂起 → 提交新任务 → 旧轮补发 Aborted」后，新任务那条仍在、`turn_running` 未被错误清零；顶替后 `unsettled_user_messages` 不残留，fork 不被永久拒绝

- [ ] [核心] 根等待回复的上限：超时按持久化失败处理，发 `PersistFailed` 走重同步，绝不发 `Aborted`
      Purpose: 卡死的 sub-agent 不能钉住界面，更不能换来假的成功信号
      Verification: sub-agent 永不回话时，根既不 settle 也不卡住

- [ ] [清理] 删掉 `ForkError::Lagging`、`ForkSource` 的 `root_messages`、`ForkOutcome::Retryable`、`rpc::FORK_NOT_READY` 和 web 的 `retryWhileNotReady`
      Purpose: 原需求的验收标志——补偿能拆掉，说明两份文档合起来真的成立
      Verification: `cargo test`、`pnpm --filter coda-web test` 全绿；fork 在「刚 settle 就 fork」「刚中止就 fork」「顶替后立刻 fork」三个时序下都一次成功

- [ ] [清理] 更新 `hub.rs` 里 `LiveState::snapshot` 那段关于「checkpoint 落在 settle 之后」的注释，以及 web `retryWhileNotReady` 附近描述该竞态的文档
      Purpose: 别让注释继续描述一个已经不存在的次序
      Verification: 通读改动处，无残留描述旧次序的说明
