# 按轮次的取消与顶替（第二部分）

## Problem

这个系统的取消语义本来就是破的，只是一直被 fork 的兜底校验遮着。中止之后 sub-agent 还会继续开工并写库；新任务顶替在跑的轮次时，事件账目对不上、`unsettled_user_messages` 会永久残留（而它正是 fork 的拒绝条件之一）；挂起等审批的线程根本没有被中止叫醒的途径。

这些今天就在发生。之所以现在必须处理，是因为 [`persist-before-visible.md`](persist-before-visible.md) 建立的那条「存档早于回话」的因果链，在这三处会被切断——而拆掉 fork 兜底校验（原需求 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md) 的验收标志）的前提，正是这条链在所有路径上都成立。

**依赖**：本文档建立在第一部分之上，收场协议直接复用它的 `TurnEnd` 和 `save_checkpoint -> Result`。第一部分必须先落地。

## Scope

**In**

- 取消从一次性控制广播改成按轮次的状态，且能主动驱动停着的线程。
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

已在代码里逐条确认。带「spike」标记的几条来自 `.scratchpad/turn-recovery/`，是实测而非读码推断。

**取消是一次性广播，管不住排队的、树状的工作。** 派发是先发信封、后登记 `pending_replies`（[`driver.rs:935`](../../crates/coda_agent/src/runtime/driver.rs) / `:960`），中间有段窗口谁都不知道这份工作存在；空闲 agent 收到 `Abort` 直接 `continue` 忽略（[`driver.rs:96`](../../crates/coda_agent/src/runtime/driver.rs)），随后照样取出排队信封跑完一整轮；运行中的 agent 那条 select 分支把 `Abort` 消费掉了（[`driver.rs:137`](../../crates/coda_agent/src/runtime/driver.rs)），收尾后转回空闲等待，照样开跑队列里的第二个。同名 stateless sub-agent 的并发调用全排在同一条队列上（只有 stateful 的并发调用会被拒，[`driver.rs:876`](../../crates/coda_agent/src/runtime/driver.rs)），所以「一个在跑、一个排队」是常规情形。

**取消走不到停着的线程，而「停着」有两种。** 一是发完 `Suspended` 后 `suspended_thread = active_thread.take()`（[`driver.rs:181`](../../crates/coda_agent/src/runtime/driver.rs)），停在信封等待里。二是**派发完 sub-agent 之后**：`handle_tool_execution` 见到 `pending_replies` 就 break，`run` 返回 `TurnOutcome::Completed`，`run_agent` 随即把 `active_thread` 清空——**根在等孩子回话期间是「空闲」的**（spike F2 实测：这种状态下优雅退出，`active_threads` 是空的）。两种都不检查取消标记、不收场、不回话。

**中止时主 agent 主动切断因果链。** 它自己把 `pending_replies` 全部写成 Aborted 的 ToolMessage（[`driver.rs:1068`](../../crates/coda_agent/src/runtime/driver.rs)）就宣布结束，而 sub-agent 们各自在收尾、各自写库。

**新任务同样切断它，而且这在正常路径上。** 提交任务的路径没有 `turn_running` 门禁（[`hub.rs:768`](../../app/coda_server/src/hub.rs)），:781 的注释还写明「轮次进行中提交新任务会顶掉待审批的调用」是有意为之。新 `Task` 和 `Reply` 进同一条 `envelope_rx`，`Task` 抢先时 [`driver.rs:502`](../../crates/coda_agent/src/runtime/driver.rs) 就地合成 Aborted 并直接开新一轮。

**中止不能就地由收到 Resume 的那个 agent 收场。** `PendingApproval` 带 `agent_name`（[`agent.rs:53`](../../crates/coda_agent/src/agent.rs)），审批可属于任何 sub-agent；而 `event_settles_turn` 只认根 agent 的 `Aborted`（[`hub.rs:313`](../../app/coda_server/src/hub.rs)）。sub-agent 改不了根线程的 `pending_replies`，发的事件也不会让 hub settle。

**hub 说「在跑」时 agent 未必已开跑。** `handle_resume` 只要 `session.resume()` 返回成功就把 `turn_running` 置真、把 pending approval 摘掉（[`hub.rs:925`](../../app/coda_server/src/hub.rs)），而那只表示信封进了 channel。

**活跃轮次可能同时有多个。** hub 允许任务排在运行中的轮次后面（[`hub.rs:1307`](../../app/coda_server/src/hub.rs)），`Session::send` 一发就进 channel。

**hub 的结算账目是按序弹出的，不是按轮次关联的。** `fold_settled_turn` 每次 settle 从 `unsettled_user_messages` 弹一条。`Suspended` 本身就是 settle（[`hub.rs:312`](../../app/coda_server/src/hub.rs)），已经把旧轮那条弹掉了；挂起期间提交新任务后，再为旧轮补发 `Aborted`，弹掉的就是**新任务**那条，并把 `turn_running` 错误清零。**含义**：光给旧轮补一个结束事件不够，账目本身得改成按 `TurnId` 关联。

**顶替协议在现有边界里写不出来。** `run_agent` 从 `envelope_rx` 取出信封就交给 `AgentLoop::run`，取的那一刻已经消费掉了，而 `run` 够不着 receiver。而且 `TurnOutcome::Completed` 同时覆盖「真结束」和「停下来等剩余回话」两种情况，`run_agent` 无从判断何时重放 FIFO。

**[spike] 活着的 sub-agent 可能一行 checkpoint 都没有。** 硬崩在孩子生成中途时，存储里只有父那一行，孩子什么都没写过。所以「扫 checkpoint 找 `reply_target` 匹配的行」找不出 producer——活着的孩子恰恰可能还没落过库。

**[spike] 但孩子的 thread_id 能从父自己的状态算出来。** `uuid5(parent_thread_id, "{parent_message_id}:{call_id}")`（stateless）或 `uuid5(parent_thread_id, agent_name)`（stateful），两个输入都在父的 `ToolExecutionState` 里。审批场景实测印证：孩子记下的 `derivation_key` 正是父的 `parent_message_id` 拼上 `call_id`。

**[spike] 一个 `turn_id` 覆盖整棵子树。** `EnvelopeBody::ToolCall` 带着 `turn_id`，`opening_user_message` 原样传给孩子，所以一轮里所有 agent 的历史条目共享同一个值。hub 按 `TurnId` 结算因此拿得到一致的 key，不用自己拼父子关系。

**[spike] 重启恢复推得出轮次顺序——比原先估计的乐观。** 原先担心的几条事实都还在：`AgentRuntimeSnapshot` 把信封按 agent 名分桶存在 `HashMap` 里，跨 agent 入队顺序没保留；`TurnId` 底下是 `Uuid::new_v4()`（[`llm.rs:19`](../../crates/coda_core/src/llm.rs)），自身不含顺序；`Reply` 不带 `turn_id`；restart-resume 的 Resume 信封由 `run_agent` 内部构造（[`driver.rs:54`](../../crates/coda_agent/src/runtime/driver.rs)），`init_envelopes` 更是在 spawn **之前**就塞进 channel（[`runtime.rs:279`](../../crates/coda_agent/src/runtime.rs)），两条都绕过 `Session`。但这些都不致命：**新轮次只从根 agent 的信箱进**——sub-agent 收到的 `ToolCall`/`Reply` 都带着调用方的 `turn_id`，从不开新轮。跨 agent 的顺序丢了也无所谓，因为只有一条排队次序，就是根的那条。实测：屏障后连发的两条 Task 在 `drained_envelopes["coda"]` 里保序，`Task { message_id }` 直接给出 `TurnId`。

**[spike] runtime snapshot 不是一次性消费。** `bootstrap` 只从内存里那份拷贝上 `remove`，存储里那行原封不动，要等下一次优雅退出整体覆盖才清掉。中间再崩一次，同一条用户任务会被回放第二遍。**含义**：恢复出来的登记必须收敛（按 key 幂等），不能「snapshot 里有什么就当什么活着」。

## Alternatives Considered

**另造一套持久化屏障 vs 把因果边接回来。** 屏障那条路走过两版，都废弃了：先是数「有几个 agent 在跑」——派发在登记之前，已入队未开工的 sub-agent 根本不在计数里，而且计数只反映「人走了」不反映「存没存进去」；改成按 `TurnId` 登记执行义务、注销带成败之后，又发现它管不了 sub-agent 的审批恢复。屏障要盖住这些就得再加一条沿调用树往上传的取消链，而一旦有了那条链，屏障本身就多余了。所以选**把因果边接回来**。代价是中止不再「立刻」，且等待随调用树深度累加。

**活跃轮次：新持久化一份有序列表 vs 从根线程推导。** spike 之前列过三个候选（写进 runtime snapshot、从 `messages.seq` 推、承认重启后不需要完整顺序）。实测之后前后两个都不必要了：既然新轮次只从根信箱进，一份「根的当前轮 + 根信箱里没读的 Task」就是完整且有序的活跃轮次，重启后两半都能从已有数据恢复。**不新增任何持久化字段**，也就没有「崩溃时它和实际入队状态怎么收敛」这个新问题。

**存活判据：按轮次 vs 按 call vs 按 thread。** 按轮次太粗（一轮活着不证明每个 `call_id` 都有 producer）；按 call 要另建一份可恢复的工作清单。最后落在 **thread**：每个待回复调用恰好对应一个孩子线程（stateless 每次调用一个独立线程；stateful 一个线程复用，而并发调用在 [`driver.rs:943`](../../crates/coda_agent/src/runtime/driver.rs) 就被拒了，同一时刻至多一个未决调用），而孩子线程 id 由上面的推导免费得到。于是不用新建清单，thread 粒度的在途登记就是它。

## Load-Bearing Decisions

1. **取消是按轮次的状态，不是一次性广播；标记由 runtime 在广播前装好，且既拉又推。** 「谁消费了控制消息」这个竞态因此整个消失。取消走到一个线程有两条路：它即将开工（取信封时查标记），或它停着且没有信封会来——**停着有两种**，发完 `Suspended` 停在 `suspended_thread`，以及派发完 sub-agent 后空闲、手里攥着 `pending_replies`（见 Validation Findings 第二条），两种都要能被推动。各 agent 一律不自行推断该取消哪一轮：stateless agent 的 `Agent` 实例跨线程复用，空闲时 `current_turn()` 留着的是上一轮。

2. **取消沿调用树往上收，每一层都等自己的孩子，只有根 settle。** 已派发的 sub-agent 调用**不得就地合成结果**——某个中间节点一旦替孩子写了结论，链就在那里断了。就地合成只留给一种情形：**该调用对应的孩子线程，此刻在本进程里没有一份未了的差事**。「未了」是从派发那一刻算到它把回话发出去为止——**不能**只算「信封还没被取走」：三层树 `root → A → B` 里，A 早就取走了自己的信封、此刻正停着等 B，按后一种算法 A 会被判死，根就会替 A 写结论，而这恰恰是本决策要挡的那次断链。孩子线程 id 从父自己的 `ToolExecutionState` 推导，不查存储——因为活着的孩子可能还没落过库。

3. **新任务顶替走同一套收场协议，并为此把信箱仲裁上提到 `run_agent`。** 用户可见语义不变（新消息照旧顶掉在跑的工作），变的是「顶掉」从就地写结论变成等真回话。`AgentLoop::run` 要能拒收信封、`TurnOutcome` 要区分「真结束」和「等回话」、`run_agent` 要持 deferred FIFO——这些不是实现细节，没有它们协议无处落地。

4. **活跃轮次由根线程推导，不新增持久化。** 有序活跃轮次 = 根的当前轮（若未结束）++ 根信箱里还没读的 `Task`（按序）。「已开始但未结束的轮次」在根上至多一个——这一条今天靠的是就地合成（旧轮当场被判死），改完之后靠的是决策 3 的 deferred FIFO（新 `Task` 被扣住直到旧轮收场），两种规矩下都成立，但**它是协议维持出来的，不是白来的**。重启后当前轮由根 checkpoint 恢复出的历史末条目给出，排队的那些由回放信封的 `message_id` 给出。队首永远是根的当前轮，中止取消的正是它。

5. **`TurnId` 进事件通道，hub 的结算账目按它索引。** 事件从 `(agent_name, ThreadId, AgentEvent)` 变成带 `TurnId` 的四元组；子树共享同一个 `turn_id`，所以 hub 拿到的 key 天然一致。`unsettled_user_messages` 从 FIFO 改成按 `TurnId` 索引，`fold_settled_turn` 按 key 删（同一轮 settle 两次——`Suspended` 一次、结束再一次——第二次自然是 no-op），`turn_running` 由活跃轮次集合推导而非维护布尔值。不改这个，给旧轮补结束事件反而会吃掉新任务。

6. **等不到回复按持久化失败处理，绝不放行成功的结束信号。** 上限只设在根一处；中间节点等不到孩子就一直等，根超时后走 `PersistFailed` + 强制重同步，drain 把底下那些一并收走。

7. **全部落地并集成之后，才拆 fork 的补偿。** 那是原需求的验收标志，也是这两份文档合起来是否成立的唯一硬证据。

## Implementation Roadmap

前提：第一部分已落地。

- [x] [spike] 在 `.scratchpad/turn-recovery/` 造「两轮活着 → 重启 → 立刻中止」的场景，回答三个待决点
      结论见 `.scratchpad/turn-recovery/FINDINGS.md`，已回填进 Validation Findings、Alternatives Considered 与决策 1/2/4/5

- [x] [核心] `AgentRuntime` 记有序活跃轮次（**只有 `Task` 开轮**——`ToolCall` 带的是调用方的 `turn_id`，`Reply`/`Resume` 更不开），`send_message` 投递前登记、失败回滚；关闭点是「根宣布了一个结束事件」，挂起和「停下来等回话」都不算；`request_abort` 先把队首放进取消集合、再广播
      Purpose: 让取消标记有可靠来源且装得够早
      Verification: 队首被标记而后面的轮次不被标记（没 bootstrap 任何 agent，广播谁都收不到，证明标记是 runtime 自己记的账而不是路过的 agent 装的）；`send_message` 失败不留幽灵登记；答完的轮次离开列表；等 sub-agent 回话期间轮次仍在列表里；`Resume` 不开第二轮。三处变异验证（去掉回滚、标记全部而非队首、放宽关闭条件）各自打红对应用例

- [x] [核心] `bootstrap` 在 spawn 任何 agent task **之前**重建登记，登记的是「本进程真会继续跑的工作」：`active_threads` 里停着的线程（查其 checkpoint 的当前轮）加上待回放的信封；按 `TurnId` 幂等，所以 snapshot 被重放第二遍也只算一轮。次序是在飞的工作在前、排队的 `Task` 在后
      Purpose: 重开会话后立刻中止时，取消面对的不能是空列表；重复回放也不能变成两轮
      Verification: 关闭重开、恢复 sub-agent 审批后，断言那一轮回到列表里且中止能标到它；连续两次重启（中间不优雅退出）拿到同一份列表

- [x] [核心] 收场协议：取消既拉又推（拉——线程每轮循环都查自己那一轮的标记；推——空闲 agent 收到 Abort 时检查 `suspended_thread`）；已取消的线程把没派出去的调用记 Aborted → 已派出去的等真回话 → 存档 → 发事件 → 回话；根等 `pending_replies` 收齐再发 `Aborted`
      Purpose: 把中止接回因果边，并让它逐层可传递
      Verification: 三层树 `root → A → B` 卡住 B 的写，根在放行前不发结束事件、放行后才发且不走 `PersistFailed`；**sub-agent 等审批时不发 Resume 直接中止**，断言被推着收场并自己回话；排队的 `Task` 属于另一轮，中止后不被标记且照常开跑。两处变异验证（根改回就地合成、去掉推）各自打红对应用例

- [ ] [核心] 信箱仲裁：`TurnOutcome` 区分「真结束」与「等回话」，加 `Deferred(Envelope)`；`run_agent` 加 deferred FIFO，旧轮收场前只喂 `Reply`，收场后按原序重投
      Purpose: 给顶替协议落脚点
      Verification: 被扣下的 `Task` 在旧轮收场后按原序重投，期间的 `Reply` 照常处理

- [ ] [核心] 顶替协议：新 `Task` / `ToolCall` 到达正等回话的线程时走收场协议再开新工作；同时立起按 `ThreadId` 的差事账（派发时记、回话发出时销——不是取走信封时销，见决策 2），就地合成收窄到「该调用的孩子线程没有未了的差事」，孩子 thread_id 由父的 `ToolExecutionState` 推导
      Purpose: 堵掉正常路径上的断链点，同时保留崩溃恢复的清理路径
      Verification: 卡住 sub-agent 的写并提交下一条 `Task`，新一轮的结束事件不提前出现；挂起不审批直接发新 `Task` 也能正常收场；崩溃恢复场景（孩子无 checkpoint 也无信封）仍走就地合成不挂住；两个 pending reply 一活一死时只合成死的那个

- [ ] [集成] `TurnId` 进事件通道；hub 结算改成按它关联：`unsettled_user_messages` 按轮次索引，`fold_settled_turn` 按 key 删，`turn_running` 按活跃轮次判定
      Purpose: 让旧轮补发的结束事件不再吃掉新任务
      Verification: 「挂起 → 提交新任务 → 旧轮补发 Aborted」后，新任务那条仍在、`turn_running` 未被错误清零；同一轮 settle 两次是幂等的；顶替后 `unsettled_user_messages` 不残留，fork 不被永久拒绝

- [ ] [核心] 根等待回复的上限：超时按持久化失败处理，发 `PersistFailed` 走重同步，绝不发 `Aborted`
      Purpose: 卡死的 sub-agent 不能钉住界面，更不能换来假的成功信号
      Verification: sub-agent 永不回话时，根既不 settle 也不卡住

- [ ] [清理] 删掉 `ForkError::Lagging`、`ForkSource` 的 `root_messages`、`ForkOutcome::Retryable`、`rpc::FORK_NOT_READY` 和 web 的 `retryWhileNotReady`
      Purpose: 原需求的验收标志——补偿能拆掉，说明两份文档合起来真的成立
      Verification: `cargo test`、`pnpm --filter coda-web test` 全绿；fork 在「刚 settle 就 fork」「刚中止就 fork」「顶替后立刻 fork」三个时序下都一次成功

- [ ] [清理] 更新 `hub.rs` 里 `LiveState::snapshot` 那段关于「checkpoint 落在 settle 之后」的注释，以及 web `retryWhileNotReady` 附近描述该竞态的文档
      Purpose: 别让注释继续描述一个已经不存在的次序
      Verification: 通读改动处，无残留描述旧次序的说明

## Deviations from Design

- **差事账从第一步挪到了顶替那一步，而且记法改了。** 原来写的是「`send_message` 投递前加、`run_agent` 取走后减」，实现时发现这个减法对三层树是错的（详见改写后的决策 2）：中间节点取走信封后正停着等孙子，会被判成死的。正确的记法是从派发算到回话发出。既然唯一的读者是顶替那一步的就地合成判据，就跟它一起做——第一步先立只有它自己要用的那本轮次账。
- **没有加 `is_cancelled` / `active_turns` 访问器。** 它们的第一个生产调用方在收场协议那一步，现在加进来就是死代码，而这个仓库没有 `#[allow(dead_code)]` 的先例。测试直接读 `AgentRuntime.turns`（`driver_tests` 是 `runtime` 的后代模块，看得见私有字段），等有真正调用方时再补正经 API。
- **恢复的判据是「谁真会继续跑」，不是「根 checkpoint 是否停在轮次中途」。** 后者看着直接，实际不可靠：中止收场之后根的 `resume_point` 同样是 `Generation`、历史末尾同样是一条 ToolMessage，和「刚答完一轮」分不开。而把一个已经结束的轮次登记回去比漏登记更糟——它会永远占着队首，把后面每一次中止都吃掉。改成只登记 `active_threads` 里停着的线程和待回放的信封；根自己那一轮不会漏，因为 spike F3 已证明子树共享同一个 `turn_id`，任何一个活着的孩子都会把它带回来。
- **停着等 `pending_replies` 的线程不需要推，被回话叫醒就够了。** 决策 1 列了两种「停着」，实现下来只有等审批那种要主动推：等回话的那个，孩子收场后一定会回话把它叫醒，而它醒来第一件事就是查标记。真正没人叫的只有等审批的——用户用「停止」代替了「批准」，那条 Resume 永远不会来。
- **`EnvelopeBody::Reply` 多了一个 `aborted`。** 孩子被中止时回的是 `Err("Aborted by user")`，而父亲原来只会按派发时记下的 `outcome`（`Auto`/`Approved`）写 ToolMessage——结果一次中止在界面上看起来像一次工具失败。只有回话的那一方知道自己是没做完还是做失败了，所以让它带上；父亲据此把这次调用记成 `Aborted`。
- **`Aborted` 事件里的 call id 改成从历史里读，不再沿途累加。** 收场是分几次完成的（每收到一份回话进行一轮），累加只能拿到最后一次那批。改成扫本轮历史里 outcome 为 `Aborted` 的工具消息——顺带也更准，因为孩子真回话说自己被中止时也会算进去。
- **`Shutdown::Abort` 与用户中止分开了两个入口。** 两者原先都走 `request_abort`。加上「推」之后这变得危险：hub 换模型时会对旧 runtime 调 `Shutdown::abort()`，而那时新 runtime 已经从库里读走了同一个待审批状态——旧的一推，就会把它当场收场写掉。现在 `cancel_in_flight` 只广播、不标记任何轮次，`request_abort` 才标记；停着的线程只在自己那一轮真被标记时才被推动。
- **轮次的关闭点是设计里没写的。** 第一步只说了怎么开轮，没说怎么关；不关的话队首永远是第一轮，「取消队首」立刻失效。落点选在「根宣布了一个结束事件且已落库」——挂起虽然也发事件但轮次只是停着，停下来等 sub-agent 回话则什么都不宣布，两种都不关。这条正是决策 4「至多一个已开始未结束的轮次」在代码里的具体形态。

## 顺带发现，不在本文档范围内

spike 撞见一个既有缺陷，和取消语义无关，但值得单独修：**runtime snapshot 会被重复回放**。`bootstrap` 只清内存拷贝，存储里那行要等下一次优雅退出才被覆盖，中间再崩一次，同一条用户任务就会被执行两遍。本文档的登记按 key 幂等，能挡住「一条任务变成两轮」，但挡不住任务本身被重放。
