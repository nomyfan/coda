## Problem

让用户能选中一条历史 user message、改写它，然后把会话回退到那一点重新执行——旧的分支永久丢弃。

需求：`../requirement/session-rewind.md`
依赖（均已落地）：`message-model-upgrade`（`message_id` / `turn_id` / `origin`）、`storage-migration-pg`（消息按行存储、`message_count` 增量水位）

## Scope

**In:**

- 服务端 `rewind` 请求：停运行时 → 按 turn 截断持久化状态 → 重建运行时 → 以编辑后的内容提交新 task
- `PgSessionStorage::rewind_to`：单事务完成定位、前置断言、跨线程按 `turn_id` 删除、`message_count` 重置、空线程清理、运行时快照清空，并回传截断后的 root 线程历史
- hub 侧的闲置前置条件与运行时重建；截断失败时把会话恢复原状
- 前端：把被编辑的消息**放回 composer**（复用其文本 + 图片附件能力），transcript 标出"提交后将被丢弃"的区段，提交即确认

**Out:**

- 不做分支 / 撤销：被丢弃的消息不可恢复
- 不支持 rewind sub-agent 线程内部的消息（UI 也不会呈现这类消息）
- 不支持运行中 / 待审批时 rewind
- 不支持编辑 assistant / tool message
- **不保证崩溃原子**：截断已提交而新 turn 未起是一个可能的中间结果，设计让它可一键恢复而不是消灭它（见 Interfaces 的"失败语义"）
- 不改 `SessionStorage` trait，`coda_agent` 零改动

## Assumptions

- **rewind 目标恒为 root 线程的 user message。** transcript 里只可能出现 root 线程的 user 消息（snapshot 只带 root 历史，事件流不携带 user 消息），所以 UI 不会给出别的目标。其余情况一律拒绝，不做兼容处理。
- **同一时刻只有一个客户端 attach**（latest-wins），所以 rewind 的结果只需回给发起方，不需要广播。
- **发起 rewind 时会话必然是 live 且已 attach 的**：UI 只能从已打开的会话进入编辑态；非 live 直接答 `SESSION_NOT_LIVE`。
- **任一线程内，turn 是按序成块出现的**（截断删的恒为每条线程的尾部）。理由：每个 agent 串行处理 envelope（`driver.rs:94-119`），channel 是 FIFO，root 也不可能在 turn B 之后再发出 turn A 的调用。这条**不靠假设兜底**——事务内有一条 `max(seq) + 1 == count` 的断言，不成立就整体回滚（见 Interfaces）。
- **会话 idle 时，丢弃任何还留在队列里的 envelope 都是安全的。** 依据**不是时序**——一次 abort 的迟到 `Reply` 完全可能晚于后续若干个 turn 才被父线程取走（被取消的子 agent 照样会从 `handle_generation` 的错误分支把 `Reply` 发出去，`driver.rs:823-845`）。依据是事务里那条断言：**每条线程都停在 `Generation`**。逐类看：`Reply` 只对停在 `ToolExecution` 且 `pending_replies` 里有对应项的线程有意义；`ToolCall` 只由正处于 `handle_tool_execution` 的父线程发出；`Resume` 的收件线程必在 `PendingApproval`——三者都被该断言排除。`Task` 只由 hub 在 entry 锁内发出，rewind 期间锁在我们手上，而一条未结算的 task 会让 `turn_running` 为真、根本进不了 rewind。于是没有任何线程在等待队列里的任何一条 envelope，丢弃它们不损失任何人在等的东西。
- 编辑后的图片由前端原样回传（data-URI / HTTPS URL），走与 `task` 完全相同的 `sanitize_task_images` 与"文本或图片至少有一个"校验。
- 会话内线程数是个位数量级，所以"逐线程重算 `message_count`"用几条小语句而非一条相关子查询 UPDATE 是划算的。

## Validation Findings

1. **"root turn 已结算"不蕴含"没有 agent 还会写 checkpoint"（决定了整个方案形状）。** 子 agent 在 `handle_generation` 里**先**把 `Reply` envelope 发给父线程（`driver.rs:756-781`），**之后**才由 `AgentLoop::run` 收尾调用 `save_checkpoint`（`driver.rs:369`）。于是 root 完全可能收到回复、跑完剩余生成、结算整个 turn，而子线程那次 `save_checkpoint` 还没落库。abort 路径窗口更宽：`request_abort` 广播后每个 agent 各自 cancel、各自等 `TOOL_ABORT_GRACE`（最长 2s）再保存。若此时截断存储，这次迟到的写入会带着**截断前**的完整历史进入 `write_checkpoint`——由于 `message_count` 已被改小，它只会被当作"正常增长"，把刚删掉的尾部原样追加回去（`storage.rs:558-589` 的 append-only 断言只挡"变短"，挡不住"变长"）。
   **设计实现：** rewind 先 `Shutdown::graceful_unbounded()` 停掉运行时。这是系统里唯一能证明"没有 agent 任务还活着"的屏障，`hub.rs:556-573` 的注释已经明确它的语义；`force_resync` 正是靠它保证"丢弃内存视图后读到的持久化状态是完整的"。截断之后再 `open` 重建，和 `handle_set_model` 的重建路径同构。
2. **停机屏障竖起之后，迟到的 agent 间消息会被写进持久化的运行时快照。** `request_exit` 先设置 exit barrier，此后 `send_message` 不再投递到 inbox，而是把 envelope 塞进 `drained_envelopes` 并**立即持久化**（`runtime.rs:330-352`）。abort 之后的收尾正好落在这里：root 已发出 `Aborted`、hub 认定 idle，子 agent 还在取消并会发出一条 `Reply`——这条 Reply 会进快照。若截断不清它，重建时 `bootstrap` 会把它重新投递给 root（`runtime.rs:266-281`），并且 `has_resuming_agents` 会因它为真、`turn_running` 被错误置位。
   **设计实现：** rewind 事务里删掉 `runtime_snapshots` 行。顺序上它必须在停机**之后**（停机过程本身会写这一行），也必须在 `Generation` 断言**之后**——那条断言不是与它并列的一步，而是它的前提（见 Assumptions 的对应条目：正因为没有线程停在 `ToolExecution` / `PendingApproval`，队列里的 envelope 才没有任何人在等）。
3. **`thread_checkpoints.pending_approval` 列不足以做闲置判据。** 它只在 `PendingApproval` 且有待审批调用时为真（`storage.rs:494-502`），停在 `ToolExecution`（等子 agent 回复）的线程不会被标记。正确的谓词是 `resume_point = '"Generation"'::jsonb`——`StoredResumePoint::Generation` 是外部标签枚举的 unit variant，序列化就是 JSON 字符串 `"Generation"`，现有测试夹具已经这么写了（`tests/storage_pg.rs:191`）。
4. **持久化的运行时快照是"至少一次"语义，不能当 outbox 用。** `bootstrap` 是从**加载出来的那份局部快照**里 `remove` 条目的，`AgentRuntime::new` 的 `self.snapshot` 从空开始（`runtime.rs:233, 266-281`），而 DB 里那一行只在 agent 退出 / `wait_for_exit` 时才被覆写（`runtime.rs:361-386, 418-427`）。也就是说：被消费过的 envelope 在会话关闭之前一直留在 DB 里。这一条否掉了"把新 Task 写进快照做崩溃原子"的路子（见 Alternatives）。
5. **需求里"前端 `TranscriptEntry` 用合成 ID（`history:user:${index}`）"这条已经过期。** entry id 现在由 `message_id` 派生（`session.ts:1285` 的 `userEntryId`），`message-model-upgrade` 落地时改掉了。所以 rewind 不需要为"index 位移"重建 entries——本设计仍然整体重建 entries，但理由是另一个（让服务端成为截断后状态的唯一权威）。
6. **存活线程上残留的 `reply_target` 无害。** abort 路径 `Done(ResumePoint::Generation)` 不会 `take` 掉 `reply_target`，于是截断后可能留下一个指向已删除调用的 `reply_target`。但每次收到 `ToolCall` 都会整体覆盖它（`driver.rs:475 / 525 / 582`），永远不会被误用。不做处理。
7. **agent 的内存历史确实不用管**（需求原判断成立，且本设计给了更强的理由）：`AgentLoop::run` 每个 envelope 开头必 `load_checkpoint` + `restore_history`（`driver.rs:251-281`）。而在本设计里 agent 实例根本会被销毁重建，问题不存在。
8. **stateful / stateless 在截断里根本不是一个判据。** stateless 线程的 id 由 `(父 Assistant message_id, call_id)` 派生，一个线程恰好对应一次调用、于是恰好属于一个 turn；它自己再派生出去的整棵子树继承同一个 turn（`turn_id` 随 `ToolCall` envelope 传播）。所以一条 stateless 线程只有两种命运：整条进丢弃集合，或者一条都不碰——**永远不会被部分截断**。反过来，能带着剩余消息活下来的线程必然是在两个以上 turn 里被调用过的，那要求它整条祖先链都是 stateful，或者它就是 root。（推论：挂在 stateless 线程下面的 stateful 子 agent 同样是单 turn 的，因为父线程本身只活一个 turn。）于是真正的判据只有一个：**剩下几条消息**。
   附带一条能力限制，方向相同：存储层的行里本来也没记 stateful/stateless——`thread_checkpoints` 只有 `agent_name` 与 `derivation_key`，stateful 的 `derivation_key` 就是 agent 名，stateless 的是 `"{message_id}:{call_id}"`（`llm.rs:185-187`）——要按两类分开处理只能靠 `derivation_key == agent_name` 去推断一个没被记录的事实。

## Alternatives Considered

**怎么保证截断不被迟到的 checkpoint 覆盖 —— 停运行时 vs 存储层加 fencing token vs 什么都不做。**

- 选择：**先 `Shutdown::graceful_unbounded()` 停掉运行时，截断后再 `open` 重建。** `coda_agent` 零改动，复用两处已被验证的机制（graceful shutdown 作屏障；`handle_set_model` 的 generation 换代）。正确性论证只有一句话：`shutdown` 返回后没有任何 agent 任务存在。代价是一次运行时重建（与切模型同量级）+ 等落单的子线程收尾（已被 abort 的话上限就是 `TOOL_ABORT_GRACE`）。
- 放弃"存储层 fencing"（会话加 epoch，rewind 时 +1，每次 `save_checkpoint` 带上加载时的 epoch，不匹配就拒绝）：不用重建运行时，但要给 `StoredCheckpoint` 加字段、给 `thread_checkpoints` 加列、在 driver 里穿透 epoch，并且新增一种 driver 必须妥善处理的失败——为一个罕见的用户操作把改动铺到 `coda_agent` 里，不划算。
- 放弃"只做前缀校验"（保存时比对已存尾条 `message_id`）：**挡不住这个 bug**。截断删的是尾部，迟到的那份历史前 N 条与存活部分完全一致，前缀校验必然通过，然后把被丢弃的尾巴接回去。

**要不要做成崩溃原子的 —— 持久化 outbox vs 接受"可能丢这次提交"。**

- 选择：**接受**。理由不是省事，而是现有的 outbox 载体是至少一次语义，代价比它要解决的问题更重。设计转而保证"永不写坏"并让这次失败可一键恢复（见 Interfaces 的"失败语义"）。
- 放弃"把新 Task envelope 写进 `runtime_snapshots.drained_envelopes`、由 `open` 自动消费"：机制上确实可行（`bootstrap` 会把它塞进 agent inbox，`has_resuming_agents` 也会正确变真），但由 Finding 4，**它不会被消费掉**。于是"打开 → 任务跑完 → 崩溃"之后，下次打开会再投递一次同一个 Task，driver 用同一个 `message_id` 再追加一条 user 消息，撞上 `UNIQUE(workspace_id, session_id, message_id)`，`save_checkpoint` 从此每次都失败（且只被 `error!` 记一笔，`driver.rs:403`）——一个永久坏掉的会话。用 `active_threads` 让 driver 从历史直接续跑也一样，只是坏法换成"重开时凭空多生成一轮"。**这两种坏法都比"丢一次提交"严重。**
- 要把 outbox 做成恰好一次，消费方必须在记录自身效果的**同一个事务**里清掉它——也就是让 driver 的 `save_checkpoint` 与"清 outbox"合成一次写，那要动 `coda_agent` 与存储层的接口。为一个罕见操作的良性失败付这个代价不划算；若日后 fork 也需要同样的语义，届时一并做。

**截断逻辑放哪 —— `coda_server::storage` vs `coda_agent` 运行时操作。**

- 选择：整块放在 `PgSessionStorage::rewind_to`，一个事务。`SessionStorage` trait 不动，driver / `MemoryStorage` / 测试 stub 零改。
- 放弃"在 `coda_agent` 里做"（给运行时加一条控制消息，让每个 agent 截断自己的内存历史并重存 checkpoint）：这是不了解 driver 的人最容易走的路，但它要 (a) 新增一条广播控制消息与"所有 agent 都已应答"的屏障，(b) 让每条线程整体重写 checkpoint，正好打掉 PG 层赖以增量追加的 append-only 不变量，(c) 而在停机重建的方案里内存历史根本不存在。三重多余。

**截断谓词 —— 按 `turn_id` 集合 vs 沿 `origin` 递归上溯 vs 按时间戳。**

- 选择：`delete from messages where turn_id = any($discarded)`，其中 `$discarded` = root 线程 `seq >= 目标 seq` 的那些 `turn_id`。`message-model-upgrade` 引入 `turn_id` 就是为了这一条谓词。
- 放弃递归 CTE 沿 `origin` 上溯：能算出同样的集合，但要写一套递归查询，而 `turn_id` 就是为免掉它才存的。
- 放弃按 `created_at` 截断：跨线程的写入时间没有全序保证（Finding 1 描述的窗口正是反例），会误删或漏删。

**空线程怎么处置 —— 一律删 vs 需求原方案（stateless 删、stateful/root 留空行）。**

- 选择：**剩余消息数为 0 的 `thread_checkpoints` 行一律删除**，包括 root。理由分三层：
  1. **两类规则里的"类"根本不是判据**（Finding 8）：stateless 线程永远是全丢或全留，不存在被部分截断的可能；能带着剩余消息活下来的必然是多 turn 线程。所以唯一还需要拍板的问题只剩一个——**剩 0 条时留不留空行**。
  2. 剩 0 条时，对 stateless 是**必须删**（父 Assistant 消息已随之删除，再没有任何东西能算出这个 id，空行是永久垃圾）；对 stateful / root 是**删与留等价**（`load_checkpoint` 返回 `None` 与返回一个空 checkpoint 在 driver 里走同一段代码，`driver.rs:274-281` 显式把无 checkpoint 的情况清空成同一状态，而稳定派生保证下次调用仍是同一个 id）。于是"一律删"既必要又充分。
  3. 换来一条更强的不变量：**没有消息的线程不存在**。附带地，也不必把"stateful 还是 stateless"这个行里没记的事实推断出来（Finding 8 末段）。
- 代价：这是对需求"todos 不回滚"的一处**有意偏离**，范围是"整条线程被删时其 todos 随之消失"。对 root 而言这只发生在 rewind 到第一条消息时——那意味着整段对话都被丢弃，此时留着一份来自被抹掉对话的 todo 清单反而更奇怪。**需求文档已同步修改**以反映这条规则；若不接受，改回两类规则是 `rewind_to` 第 6 步里加一个 `derivation_key != agent_name` 判据的事。

**截断后前端怎么对齐 —— 服务端回传剩余历史 vs 前端本地截断 entries。**

- 选择：`rewind` 的结果带上截断后的 root 历史（`{ message_id, messages }`），前端按 snapshot 的同一条路径重建 entries / usage，**再补一条编辑后的 user entry**（事件流从不携带 user 消息，这条必须由客户端补，与 `startTurn` 的乐观追加同理）。hub 手里本来就有这份数据（截断事务顺带读出、直接成为新的 `live.snapshot`），服务端零额外成本。
- 放弃"只回 `message_id`、前端 `entries.slice(0, index)`"：payload 更小，但把"截断后长什么样"变成两侧各算一遍的东西；而且 `session.usage` 是按 assistant 消息累积的扁平列表，本地截断没法把它算对（context 用量指示器会一直偏高）。回传历史让 `historyUsage` 顺手把它重算对。
- 代价：一次 rewind 多传一份历史。但它**必然小于**客户端此刻已持有的历史，而每次 `open_session` / 重连都在传同样量级的数据。

**编辑入口 —— 放回主 composer vs 消息气泡内联编辑。**

- 选择：**放回主 composer**。composer 已经具备文本 + 图片（粘贴 / 拖拽 / 上传 / 逐张删除）+ 模型能力校验（`imagesBlockSend`）+ Enter 提交的全套能力，编辑态直接复用，图片附件问题一并消解。transcript 用视觉标记指出正在编辑哪一条、哪些内容将被丢弃；**提交按钮本身就是那道确认关卡**，不再加二次确认弹窗。它同时是失败恢复的落点——但那要靠上面那套编辑态模型撑住，**不是**复用 composer 就自动获得的：现有 `submit()` 提交后无条件清空本地 text/images，编辑模式必须显式跳过这一步。
- 放弃气泡内联编辑：要在气泡里重建一套简化的输入框，且带图片的消息要么退化成纯文本、要么再造一套附件 UI。

## Components

- **`PgSessionStorage::rewind_to`**（`coda_server::storage`）—— 单事务完成一次 rewind 的全部持久化改动，并回传截断后的 root 历史。是本设计里唯一知道"rewind 用 SQL 怎么写"的地方。
- **`SessionOpener::rewind`**（`coda_server::hub` 的 port）—— hub 通往持久化层的既有出口（它已承载 `load_messages` / `update_reasoning_effort`），新增一个方法把 `rewind_to` 接进来。
- **`SessionHub::handle_rewind`** —— 闲置校验 → 停运行时 → 截断 → 重建运行时 → 提交新 task，全程持 entry 锁，所以没有任何命令能插进中间。
- **`rewind` RPC**（wire 参数 / 结果 + dispatcher 分支）—— 客户端输入进入系统的边界。
- **`applyRewound`**（web store，纯 reducer）—— rewind 成功后的整段状态转换：截断后的历史 + 新 user entry + 用量重算。
- **web store 的编辑态**（`editing` + `beginEdit` / `cancelEdit` / `rewindTurn` / `reconcileEditing`）—— 一个小状态机：改的是哪一条（可为"已经不指向任何一条"）、改成了什么、请求是否在途。失败恢复完全由它承担。
- **Composer 编辑模式** —— 由 `editing` 播种初始文本与图片，顶部显示编辑横幅与取消。
- **Transcript 编辑标记** —— user 气泡上的编辑入口，以及编辑期间把待丢弃区段标灰。

## Interfaces

```rust
// coda_server::storage —— 一次 rewind 的全部持久化语义，一个事务，全有或全无。
impl PgSessionStorage {
    /// 丢弃 `target`（本会话 root 线程的一条 user 消息）以及它之后所有轮次在
    /// **任何**线程留下的消息，回传 root 线程剩下的对话。
    ///
    /// 拒绝并且不改动任何东西的情况：`target` 不是本会话 root 线程的 user 消息；
    /// 或者本会话还有线程没停在普通的生成边界上（调用方必须先把 turn 跑完 /
    /// 中止、把待审批答完）。调用方必须先停掉运行时——本方法不做这件事，也无从
    /// 校验，它只是这条前置条件的最后一道防线。
    pub async fn rewind_to(&self, target: MessageId) -> Result<Vec<Message>, RewindError>;
}

pub enum RewindError {
    /// 目标不存在，或不是本会话 root 线程的 user 消息。
    TargetNotFound,
    /// 该线程没停在生成边界上（`resume_point != Generation`）。
    ThreadBusy { thread_id: String },
    /// 截断后某条线程的 seq 不再是 [0, count) 的连续区间——"删的恒为尾部"
    /// 这条不变量破了。事务已回滚，什么都没变。
    HistoryNotContiguous { thread_id: String },
    Persistence(String),
}
```

事务内的步骤（顺序即依赖）：

1. 取目标 `seq`：`messages` 中 `(ws, sid, thread_id = sid, message_id = target, role = 'user')`；查不到即 `TargetNotFound`。
2. 闲置断言：`thread_checkpoints` 中 `(ws, sid)` 且 `resume_point <> '"Generation"'::jsonb` 的第一条 → `ThreadBusy`。
3. 待丢弃轮次：`select distinct turn_id from messages where (ws, sid) and thread_id = sid and seq >= $seq`。
4. `delete from messages where (ws, sid) and turn_id = any($turns)`。
5. 重算：`select thread_id, count(*), max(seq) from messages where (ws, sid) group by thread_id`。
6. 逐条遍历本会话的 `thread_checkpoints`：不在上一步结果里 → 删行；`max + 1 <> count` → `HistoryNotContiguous`（回滚）；否则 `message_count = count`。
7. `delete from runtime_snapshots where (ws, sid)`（Finding 2）。**必须排在第 2 步之后**——第 2 步的 `Generation` 断言正是"丢弃队列里的 envelope 不损失任何人在等的东西"的依据。
8. `touch(sessions.updated_at)`。
9. `select payload from messages where (ws, sid, thread_id = sid) order by seq` → 回传。

第 6 步的连续性断言是本设计对"删的恒为尾部"这条不变量的**唯一硬保证**：一旦哪天线程内的 turn 不再成块有序，这里立刻炸出来，而不是留下一段 seq 有洞、之后每次 `save_checkpoint` 都错位的历史。

```rust
// coda_server::hub —— 命令与结果。
SessionCommand::Rewind { target: MessageId, task: String, images: Vec<String> }

CommandOutcome::Rewound { message_id: MessageId, messages: Vec<Message> },
/// 有 turn 在跑，或还有待审批调用；也覆盖存储层的 `ThreadBusy`。
CommandOutcome::NotIdle,
CommandOutcome::RewindTargetNotFound,
/// 截断已提交，但新 turn 没能起来。运行时已被丢弃、`Closed` 已发出，
/// 客户端会重新 attach 并对账；答 `REWIND_FAILED` 并带上说明。
CommandOutcome::RewindNotStarted,
// 已有的 PersistenceFailed / OpenFailed 复用。

// hub 的 port 新增一项。
trait SessionOpener {
    /// 丢弃 `target` 及其之后的全部轮次，回传 root 线程剩下的对话。
    /// 调用方保证运行时已经停稳。
    fn rewind<'a>(&'a self, key: &'a SessionKey, target: MessageId)
        -> Pin<Box<dyn Future<Output = Result<Vec<Message>, RewindError>> + Send + 'a>>;
}
```

`handle_rewind` 的控制流（全程持 entry 锁）：

```
Live(live) 否则 -> Ignored（dispatcher 读作 SESSION_NOT_LIVE）
live.turn_running || !live.pending_approvals.is_empty() -> NotIdle
live.session.shutdown(Shutdown::graceful_unbounded()).await          // 屏障
match opener.rewind(key, target).await {
    // 截断未提交：事务什么都没改，把会话原样恢复
    Err(e)       => 重建运行时（沿用原来的 snapshot） -> 对应错误
                    重建也失败                        -> 兜底 -> OpenFailed
    // 截断已提交：从这里往后，任何失败都必须把客户端逼进重新 attach
    Ok(messages) => 重建运行时；新 LiveState.snapshot = messages；
                    照 handle_task 提交新 task
                      成功      -> Rewound { message_id, messages }
                      send 失败 -> 兜底 -> RewindNotStarted
                      重建失败  -> 兜底 -> OpenFailed
}
// 兜底（与 attach 的开失败路径一致）：关掉并丢弃 replacement runtime、
// 给 attach 方发 Closed、从 entries 移除。连接层已有"被 hub 关掉就重新 attach"的处理。
```

**截断提交之后的每一种失败都走同一条兜底**，而不是各自想办法把状态修回去。理由：进程崩溃那条路本来就只能靠"重新 attach + 对账"恢复，让 `send` 失败和重建失败复用同一条，恢复机制就只有一套、且被任何一条失败路径的测试覆盖到。反过来说，如果给 `send` 失败单独做一个"就地把截断后的历史推给客户端"的分支，就会有两套恢复逻辑，其中一套（崩溃那套）永远无法被这个分支验证。这条兜底也把原来那个隐患消掉了：`handle_task` 在 `send` 失败时只返回 `Ignored`（dispatcher 答 `SESSION_NOT_LIVE`），而此时截断已经提交——客户端会一直显示数据库里已经不存在的消息，重发则永远得到 `TargetNotFound`。不能指望"运行时反正会自己断"：那只在 `ChannelClosed`（agent 已死 → broadcast 关闭 → forwarder 走 `Closed`）时成立，`AgentNotFound` 时运行时活得好好的。

三点值得点名：**(a)** 停机与重建之间没有任何别的命令能插进来，因为整段在 entry 锁内；**(b)** 重建走的是 `make_live`，所以 `log` / `unsettled_user_messages` / `pending_approvals` 全部是新的，不需要逐个手工清理；**(c)** 老 forwarder 靠 generation 递增自行退休，与 `handle_set_model` 同一机制。

**失败语义（rewind 不是崩溃原子的，这是有意选择）。** 截断事务提交之后还有三个失败点：重建运行时失败、`Session::send` 失败、以及 `send` 成功但 driver 还没把新 user 消息写进 checkpoint 时进程崩溃（driver 是处理完 envelope 之后才 `save_checkpoint`，`driver.rs:283-296`）。任一处失败留下的状态都是**一致但缺了新 turn**：旧分支已删、编辑后的消息不存在。本设计保证的是**永不写坏**（要么两边都没动，要么历史干净地停在回退点），而不是"永不丢这次提交"。

丢失之所以可恢复，靠的是下面这个状态模型，**不是靠"内容碰巧还在输入框里"**——现有 composer 提交后会同步清空本地 text/images（`composer.tsx:155-160`），光保留 store 里的 `editing` 什么也恢复不了。三条规则缺一不可：

1. **提交前把用户当前输入写回 `editing`。** `rewindTurn(text, images)` 的入参就是 composer 手里的最新值，发请求前先落进 store。这样 `editing.text/images` 从"初始播种值"变成"权威草稿"，composer 因任何原因重挂载都能复原。
2. **编辑模式下 `submit()` 不清空本地状态。** 清空这件事交给成功路径：`editing = undefined` 让 composer 的 key 从 `edit:*` 变回 `new`，重挂载即为空。于是失败路径天然什么都不清。
3. **重连时按快照对账**（见下方 `reconcileEditing`）：截断到底提没提交，只有服务端知道；客户端靠"目标消息还在不在"这一个可判定的事实来决定下次提交走 `rewind` 还是走 `task`。而"必然会有一次重连"由服务端保证——截断提交之后的任何失败都会发 `Closed`（见上方控制流的兜底），不依赖连接碰巧断掉。

（截断**之前**的失败——`NotIdle` / `TargetNotFound`——什么都没改，目标消息仍在快照里，对账后编辑态原样保留，取消或重试皆可。）

**相邻的既有缺口，本设计不解决：** 若崩溃发生在新 user 消息已持久化、但生成尚未开始时，重开会话会得到一条"没有回复的 user 消息"——`active_threads` 为空，没有任何东西会去续跑它。这是任何 turn 中途崩溃都有的行为，不是 rewind 引入的；这里记一笔是因为 rewind 让它更容易被撞见。

```rust
// wire —— 客户端输入的信任边界。
struct RewindParams {
    workspace_id: String, session_id: String,
    message_id: MessageId,          // 客户端提供，由第 1 步校验"确为本会话 root 线程的 user 消息"
    task: String,                   // 与 task 走同一条 trim + 非空校验
    images: Vec<String>,            // 与 task 走同一个 sanitize_task_images
}
struct RewindAccepted { message_id: MessageId, messages: Vec<Message> }
```

**Trust boundary —— `rewind` 的 dispatcher 分支。** 客户端唯一能指定的身份是 `message_id`，它只作为绑定参数进入一条"必须是本会话 root 线程的 user 消息"的查询；不满足即 `REWIND_TARGET_NOT_FOUND`，不存在跨会话指认。新消息的 `message_id` 一律服务端铸造（沿用 `handle_task` 的既定不变量）。文本与图片走与 `task` 完全相同的清洗，下游不必重复校验。

新增错误码（沿用现有分块）：`SESSION_NOT_IDLE = -32006`（会话状态块）、`REWIND_TARGET_NOT_FOUND = -32014`（资源块）、`REWIND_FAILED = -32023`（操作失败块）。

```ts
// web store —— 编辑态承载三件事：改的是哪一条、改成了什么、请求是否在途。
type OpenedSession = {
  // …
  editing?: {
    /** rewind 目标。`null` 表示目标已经不在了（截断已提交），这就只是一份普通
     *  草稿——下一次提交走 `task` 而不是 `rewind`，transcript 也不再标丢弃区段。 */
    target: string | null;
    /** 权威草稿，不是初始播种值：每次提交前由 `rewindTurn` 写成当前输入。 */
    text: string;
    images: string[];
    /** 请求在途。期间禁用提交 / 取消 / 其它消息的编辑入口。 */
    submitting: boolean;
  };
};

beginEdit(messageId)      // 仅在 connected && !running && approvals===0 && !evicted && !editing?.submitting 时可用
cancelEdit()              // submitting 期间禁用
rewindTurn(text, images)  // editing 置位时 composer 的提交一律走这里（含 orphan 分支）

/** 重连 / 重新 attach 时对账编辑态。截断到底提没提交只有服务端知道，而客户端
 *  能判定的事实只有一个：目标消息还在不在快照里。由 `applySnapshot` 调用。 */
export function reconcileEditing(
  editing: OpenedSession["editing"],
  messages: HistoryMessage[],
): OpenedSession["editing"];

/** 待丢弃区段的起点；entries 是追加有序的，所以一个下标就够。 */
export function discardedFrom(entries: TranscriptEntry[], messageId: string): number | undefined;

/** rewind 成功后的整段状态转换。纯函数（与既有的 `reduceEvent` 同构），可直接单测。 */
export function applyRewound(
  session: OpenedSession,
  payload: { messages: HistoryMessage[]; messageId: string; text: string; images: string[] },
): OpenedSession;
```

`applyRewound` 的语义逐条（**这一段是必须写死的**：事件流从不携带 user 消息，只应用 `messages` 的话编辑后的消息根本不会出现在 transcript 里，随后的 assistant 输出会直接接在旧历史后面）：

- `entries` = `historyToEntries(messages)` **再追加一条** `{ id: userEntryId(messageId), messageId, kind: "user", content: text, images, startedAt: now }`——与 `startTurn` 的乐观追加同理，区别只是这里的 id 一开始就是服务端的，不需要事后 reconcile；
- `usage` = `historyUsage(messages)`（截断后重算，否则用量指示器会一直偏高）；
- `approvals` / `drafts` / `allowDrafts` / `pendingCallInfo` 全部清空；
- `running = true`；
- `editing = undefined`——**只在成功路径清**（并借此触发 composer 重挂载完成清空）；`rewindTurn` 的失败分支只把 `submitting` 置回 false，保留 `editing` 并追加一条 danger 活动记录；
- `firstUserMessage`：`messages` 为空时改成 `text` 并同步 catalog 标题（rewind 到第一条消息会改变会话列表标题）。

`TranscriptEntry` 增加 `messageId?: string`（只有 user entry 有）：`historyToEntries` 里直接取，乐观条目在 `adoptServerMessageId` 收到 ack 时补上。没有它就没有编辑入口——正好排除掉"turn 正在跑、消息还没 ack"的条目。

`rewindTurn` 拥有**两个分支各自完整的提交生命周期**——`editing` 的清除只可能发生在它这里，别处不会替它做（`applyRewound` 只覆盖 rewind 成功这一种情况，`startTurn` 根本不认识编辑态）：

```ts
// 两个分支共同的开头：写回权威草稿 + 上锁
if (editing.submitting) return;              // 在途期间的二次提交直接丢弃
editing = { ...editing, text, images, submitting: true };

if (editing.target === null) {
  // orphan：历史已经停在回退点，这就是一次普通 task
  const started = await startTurn(...);
  if (started) editing = undefined;          // -> key 变回 "new" -> 重挂载清空
  else editing.submitting = false;           // 草稿留着，可以再试
} else {
  const result = await rpc.request("rewind", ...);
  if (ok) applyRewound(...);                 // 其中包含 editing = undefined
  else editing.submitting = false;           // 只动这一个字段，理由见下
}
```

`editing = undefined` 是**唯一**的退出方式，两个分支都必须显式走到它——否则消息已经发出去、同一份文字却还留在输入框里，用户下一轮还能再提交一次同样的内容。

`reconcileEditing` 的语义（只有两个分支，理由见下）：

- `editing` 未置位 → 不变；
- `editing.target` 在 `messages` 里仍能找到同 `message_id` 的 user 消息 → **截断没提交**（请求根本没到、或在断言/定位阶段被拒），编辑态原样保留；
- 找不到 → **截断已提交**，把 `target` 置为 `null`：内容留着，但它不再指向任何历史消息，下一次提交是一个普通 `task`——而此时历史正好停在回退点，普通 task 得到的就是用户本来想要的结果；
- 任何情况下 `submitting` 都清掉（发出那次请求的连接已经没了）。

**失败分支只动 `submitting`，这一点是有意的**：截断提交后的失败会同时产生一个 RPC 错误答复和一条 `Closed` 推送（走兜底），两者到达顺序不定。因为失败分支不碰 `target`、而 `reconcileEditing` 不碰 `text/images`，这两次更新**可交换**——先答复后重连、还是先重连后答复，最终都落在"orphan 草稿、submitting 为假"。若哪天失败分支开始改 `target`，这条性质就没了。

**为什么没有第三个分支（"替代消息已经存在，说明只是响应丢了"）**：那条替代消息的 id 是服务端铸造并随响应回传的，而这个分支的前提正是响应丢了——客户端从来没拿到过那个 id，无从判定。不需要它也是安全的：如果新 turn 真的起来了，快照里要么 `turn_running` 为真（composer 被 `running` 禁用，用户按不下去），要么新消息连同回复都已在历史里、用户一眼看得见。后者最坏是用户手动重发一次，那是一次普通的重复提交——可见、可撤（abort），不是数据损坏。用一个判定不了的信号去换这点收益不划算。

Composer 的三条规则：

- `key={editing ? \`edit:${editing.target ?? "orphan"}\` : "new"}` 强制重挂载，初始文本与图片由 props 播种（取自 `editing.text/images`）。不需要任何 `useEffect` 同步，本地 state 的生命周期与编辑态严格对齐；对账把 `target` 降级成 `null` 时 key 也随之改变，于是重挂载并用写回后的草稿重新播种。
- **编辑模式下 `submit()` 不再清空本地 text/images**（现有实现是无条件清空，`composer.tsx:155-160`）。成功时由 `editing = undefined` 触发的重挂载来清；失败时什么都不清。
- `editing.submitting` 为真时禁用提交与取消，与既有的 `starting` 标记同一个用法（`starting` 就是"`open_session` 在途、`running` 覆盖不到的那段窗口"）。

## Data Model

不新增表、不新增列——`message-model-upgrade` + `storage-migration-pg` 已经把 rewind 需要的东西全部备齐了。本设计只是对既有 schema 做一次受约束的写：

- **删除面**：`messages` 中 `turn_id ∈ 待丢弃集合` 的所有行（跨全部线程，走 `(ws, sid, turn_id)` 索引）；`thread_checkpoints` 中剩余消息数为 0 的行；`runtime_snapshots` 中本会话那一行。
- **修改面**：存活线程的 `message_count` 重置为剩余条数；`sessions.updated_at` 前移。
- **不动面**：`sessions.model_binding` / `name`；存活线程的 `todos` / `reply_target` / `parent_thread_id` / `derivation_key`。
- **不变量**：截断后每条存活线程的 `seq` 仍是 `[0, message_count)` 的连续区间（删的恒为尾部），由第 6 步的断言把守；"没有消息的线程不存在"是本设计新引入的不变量。

**共享可变状态**：`EntryState`（hub 的 per-session 槽）在整个 rewind 期间被独占。这不是优化而是正确性要求——闲置校验、停机、截断、重建、提交这五步之间任何一处被别的命令插入都会出问题（例如在停机与重建之间来一个 `task`，会打在一个已经死掉的 session 上）。

## Load-Bearing Decisions

- **rewind 先停运行时、后重建。** 换来一句话就能讲完的正确性论证与 `coda_agent` 零改动；代价是一次运行时重建，以及要等落单的子线程收尾。理由见 Finding 1——这是本设计里最贵也最不可省的一步。
- **rewind 事务清空 `runtime_snapshots`。** 消掉"重建后重放属于已删除分支的 envelope"这类问题；安全性来自同一事务里的 `Generation` 断言（没有线程在等任何一条排队的 envelope），而**不是**时序论证。
- **不追求崩溃原子，改为保证"永不写坏"+ 可恢复。** 代价是极窄的窗口里可能丢掉这次提交；换来的是不引入至少一次的 outbox（Finding 4 / Alternatives）。
- **恢复能力由一个显式的编辑态模型承担，而不是"内容碰巧还在输入框里"。** `editing.target: string | null` 把"这是一次 rewind"和"这只是一份草稿"做成同一个字段的两个取值，于是"截断已提交但新 turn 没起来"从一个需要文档解释的异常，变成状态机里一个正常的、有类型的位置——下一次提交自动走 `task`。代价是编辑态多了两个字段（`target` 可空、`submitting`），以及 composer 要为编辑模式改一处提交后清空的行为。
- **截断提交之后的所有失败共用一条兜底（`Closed` → 重新 attach → 对账）。** 恢复机制只有一套，且它正是崩溃场景唯一可能的恢复路径，因此每条失败路径的测试都在验证同一段代码。代价是极罕见的 `send` 失败也要付一次运行时销毁 + 重新 attach，而不是就地把截断后的历史推给客户端——后者省一个往返，却会引出一套永远无法被崩溃场景验证的第二恢复逻辑。
- **截断逻辑整块下沉到 `PgSessionStorage`，`SessionStorage` trait 不动。** driver / `MemoryStorage` / 测试 stub 零改；代价是这段逻辑只能在 `pg-tests` 门控下测试。
- **按 `turn_id` 集合删除，一条谓词覆盖所有线程。** 代价是把"线程内 turn 成块有序"吃进设计，并用事务内的连续性断言兜底。
- **剩余为 0 的线程行一律删除**（含 root）。少一条分支、多一条不变量，且存储层本来就分辨不出 stateful/stateless；代价是对"todos 不回滚"的一处有意偏离（仅限整条线程消失时），**需求文档已同步修改**。
- **rewind 是一个请求，截断与重新提交不拆开。** 拆成两个请求并不能消除失败窗口（重建仍在截断之后），却多一个往返和一段"截断了但没提交"的可观测状态。
- **结果回传截断后的历史，前端再补一条新 user entry。** 服务端成为截断后状态的唯一权威；代价是一次比现有 snapshot 更小的额外传输。
- **编辑态复用主 composer，提交即确认。** 附件能力、模型校验、快捷键全部免费，并且天然承担失败恢复；代价是进入编辑态会丢弃 composer 里正在写的草稿（见 Risks）。

## Risks / Open Questions

- **最大风险：截断之后还有 checkpoint 写进来。** 缓解就是停机屏障。**第一个要建、也是第一个要反向验证的**：构造一次"root 已结算、子线程尚未保存"的时序，确认停机后再截断的结果正确；然后**把停机那一步去掉，确认同一个测试失败**——不能反向复现的话，这条最贵的决定就没有牙。
- **迟到的 envelope 进快照。** 缓解是事务里删掉快照行。验证要覆盖两种不同的窗口：(a) root 已 `Aborted`、子 agent 尚未发出 `Reply`（Reply 落进 `drained_envelopes`）；(b) **一次旧 abort 的迟到 `Reply` 跨过了后续一整个 turn** 才被取走——这条专门用来证明安全性依据是 `Generation` 断言而非时序，如果谁把论证改回"必属最近一次 turn"，它就该失败。
- **"删的恒为尾部"若破。** 缓解是事务内的 `max(seq) + 1 == count` 断言（回滚而非写坏）。验证：手工构造一条 seq 有洞的线程，确认 `rewind_to` 拒绝且数据未变。
- **`message_count` 重置漏了就静默错位**（`storage-migration-pg` 的 Risks 早就点名了这个坑）。验证必须是**端到端**的：rewind 之后跑一个完整的新 turn，再冷开会话，确认历史既不缺也不重。只断言 `message_count` 的数值不够。
- **截断后的失败路径。** 三个失败点都要有测试或明确论证（见 Roadmap）。其中"重建失败"落到 `Closed` → 重连，而连接层对同一个 key 只做**一次**静默重连（`reattached` 集合）；若同一连接此前已用掉这次机会，客户端会看到会话消失而非自动恢复。当前判断可接受（重建失败意味着 workspace/provider 配置出了问题，本就该显式暴露），但实现时要确认这个交互。
- **进入编辑态会丢弃 composer 里已写的草稿**（keyed 重挂载的直接后果）。可接受：这是一个明确的用户动作。要做到无损，正路是把 composer 草稿整体提升到 store（顺带修掉"草稿会跨会话串"的现有问题——composer 没有按 session 加 key），那是独立的一件事。注意本设计只把**编辑态**的草稿放进了 store，普通草稿仍归 composer 本地所有，两者并存是刻意的最小改动，但也意味着"草稿归谁"这件事暂时有两套答案。
- **编辑态与请求在途的交互面。** 请求在途期间 `running` 仍为假，所以必须靠 `editing.submitting` 挡住重复提交、取消、以及切换编辑目标；漏掉任一处就会出现两个响应反向覆盖前端状态。服务端的 entry 锁只保证数据不写坏，管不了前端。要专门测双击提交、请求失败后的可重试性、以及响应乱序。
- **前端回归防护。** `session.ts` 目前没有可用的测试脚手架（`message-model-upgrade` 落地记录已记此缺口）。本设计把两处最容易出错的逻辑抽成纯函数（`discardedFrom`、`applyRewound`），它们不需要 rpc/store 脚手架就能测；其余部分仍只有 lint / typecheck / 手工验证。

## Implementation Roadmap

- [x] [risk validation] `PgSessionStorage::rewind_to` + `tests/storage_pg.rs`
      - Purpose: 先把最贵的持久化语义钉死——它是其余所有步骤的地基
      - 覆盖：跨 root + stateful 子线程 + stateless 子线程的一次截断；存活线程 `seq` 连续且 `message_count` 相符；首次调用的 stateless / stateful 空线程行都被删除；rewind 到第一条消息时 root 行也被删除且会话仍可重开；`runtime_snapshots` 行被清空；`resume_point != Generation` 时整体拒绝；目标不是 root user 消息时拒绝；seq 有洞时以 `HistoryNotContiguous` 回滚
      - Verification: 上述断言全绿；**反向验证**——去掉 `message_count` 重置那一句，"rewind 后再存一次 checkpoint"的测试必须失败
      - 落地：`RewindError` + `PgSessionStorage::rewind_to`（`storage.rs`），事务步骤与设计一一对应。6 个测试：`a_rewind_drops_the_discarded_turn_from_every_thread_it_reached`（三线程两轮次，断言剩余 `(thread, seq)` 全集、两条存活线程的 `message_count`、快照行被清）、`a_rewound_thread_keeps_growing_from_where_it_was_cut`、`rewinding_to_the_opening_message_leaves_no_session_state_behind`、`a_rewind_is_refused_while_any_thread_is_mid_turn`、`only_a_user_message_of_the_root_thread_can_be_rewound_to`、`a_truncation_that_would_leave_a_gap_is_rolled_back`
      - **三条反向验证都跑过**：(1) 把 `message_count` 重置改成自赋值 → 上述第 1、2 条测试同时失败（第 2 条的失败正是设计预测的"追加被静默丢弃"）；(2) 删掉 `runtime_snapshots` 的删除 → 第 1 条失败；(3) 把闲置判据从 `resume_point != Generation` 换成 `pending_approval` 列 → 第 4 条**只在 `ToolExecution` 那次迭代**失败，精确复现 Finding 3。为让第 (3) 条成立，该测试的 fixture 改成 `resume_point` 与 `pending_approval` 两列同写（`save_checkpoint` 就是这么写的），否则两次迭代都会失败、证明不了缺口在哪
      - 偏差：无
- [x] [risk validation + core logic] 停机屏障、`SessionCommand::Rewind` / `CommandOutcome` / `SessionOpener::rewind` / `SessionHub::handle_rewind`（`hub.rs` + `hub_tests.rs`）
      - **顺序偏差**：设计把时序测试排在实现之前，但那条测试要驱动 `handle_rewind` 才存在，两步无法真正分开，合成一个 phase 落地
      - Purpose: 把闲置校验、停机、截断、重建、提交串成一个持锁的步骤，并证伪 Finding 1
      - 落地：6 个测试——`a_rewind_waits_out_a_sub_agent_that_replied_before_it_saved`（核心）、`a_rewind_replaces_the_discarded_turn_and_reports_what_survived`、`a_rewind_is_refused_while_a_turn_is_in_flight`、`a_rewind_is_refused_while_a_call_waits_on_a_human`、`a_refused_rewind_leaves_the_session_exactly_as_it_was`、`a_rebuild_that_fails_after_the_truncation_sends_the_client_back_for_a_fresh_attach`
      - **屏障反向验证通过**：去掉 `handle_rewind` 里的 `shutdown`，核心测试以"the sub-agent's late checkpoint must not restore the turn that was discarded"失败。测试用 `SlowStorage` 把 `explore` 线程的 checkpoint 写入延迟 300ms，人为把 Finding 1 描述的窗口拉宽到可测
      - **该测试第一版是假通过的**（第一次反向验证没能让它失败）：替换轮次瞬时完成，最终断言跑在延迟写入落地**之前**，于是两种实现都读到空历史。加上"睡过延迟窗口再断言"才真正有牙。记在这里是因为这类"因为错误的原因而通过"的测试，看起来和真测试一模一样
      - 另一个测试基建修正：`session_with_one_turn` 结尾要 `wait_idle`。客户端拿到结算事件**早于** forwarder 更新 entry，直接发命令会撞上 `turn_running` 仍为真

**本阶段推翻的两条设计判断（均由读码/实测得出，设计正文尚未回改）：**

- **`CommandOutcome::RewindNotStarted` 实际上不可从外部构造，因此没有测试。** 设计里"截断成功但 `send` 失败"被当作一条真实故障路径，reviewer 也据此要求断言。但 `AgentRuntime::send_message` 会**先**检查 exit barrier：运行时一旦停止，envelope 被缓冲进 `drained_envelopes` 并返回 `Ok`，而不是报错（`runtime.rs:330-352`）；而 `AgentNotFound` 要求根 agent 未注册，`team.build()` 保证不会发生。所以注入这个故障的尝试失败了（会得到 `TaskAccepted`），测试已删除。分支保留为防御性代码，其恢复路径（`abandon`）由重建失败那条测试覆盖——两条失败共用同一段代码，这正是"只有一套恢复机制"的收益
- **Finding 2 的场景比设计写的窄得多。** 设计称"abort 后 root 已 `Aborted`、hub 认定 idle，子 agent 的迟到 `Reply` 会进快照"。实测追踪：root 在等子 agent 回复时是**空闲等 envelope**，`Abort` 走 `continue` 分支、**不发** `Aborted` 事件，所以 hub 根本不会认定 idle；随后 explore 的 `Reply` 会被还活着的 root 正常消费（`Generation` 下是 warn + no-op），turn 照常结束。envelope 真正滞留到落库，需要"运行时先于 root 取走它而停止"——rewind 自己的 `shutdown` 正好制造这个条件（barrier 竖起后 `send_message` 转为缓冲），或 `run_agent` 退出时把 inbox 里的残余 drain 进 `agent_drained_envelopes`。**结论不变**（快照必须清），但**必要性**的论证要换成这一条，而**安全性**仍由 `Generation` 断言给出。原计划的 (b)(c) 两个 hub 时序测试因此删除：能确定性构造的那个场景需要一个会挂起的本地工具，hub 测试基建里没有，代价不成比例。快照被清空这件事本身由 `storage_pg` 的核心测试覆盖并已反向验证
- [ ] [integration] `rewind` RPC：wire 类型、三个错误码、dispatcher 分支（复用 `sanitize_task_images` 与非空校验）
      - Purpose: 打通到协议边界
      - Verification: `cargo clippy` + `cargo test`；参数畸形 / 目标不存在 / 非 live 各自答出正确的码
- [ ] [web] store：`editing` 状态机（`target` 可空 + `submitting`）、`beginEdit` / `cancelEdit` / `rewindTurn`、纯函数 `discardedFrom` / `applyRewound` / `reconcileEditing`、`TranscriptEntry.messageId`
      - Purpose: 让 rewind 后的前端状态由服务端结果重建，并把"截断已提交但新 turn 没起来"变成状态机里一个有类型的正常位置
      - Verification: `applyRewound` 单测——entries 顺序与新条目的 id/内容/图片、`usage` 重算、`running`、`editing` 被清、`messages` 为空时标题改写；`reconcileEditing` 单测——目标仍在则原样保留、目标消失则 `target` 降级为 `null` 且草稿留存、`submitting` 一律清；`rewindTurn` 单测——提交前把当前输入写回 `editing`；rewind 失败只清 `submitting` 且不动 `target`（这是与 `reconcileEditing` 可交换的前提，值得单独断言）；**`target` 为 `null` 时转调 `startTurn`，成功后 `editing` 被清、失败后草稿留存**；`submitting` 期间的二次提交被丢弃；两次更新（RPC 失败答复 / 重连对账）**任意顺序**到达都收敛到同一状态；`discardedFrom` 单测；`pnpm --filter coda-web lint && test`
- [ ] [web] UI：user 气泡的编辑入口（闲置且非 `submitting` 时可用）、编辑期间待丢弃区段标灰（仅 `target != null`）、composer 编辑横幅 + 取消（Esc）+ keyed 重挂载 + **编辑模式提交后不清空本地状态**
      - Purpose: 让"提交即确认"在界面上自明，并让失败后的重试是零操作成本的
      - Verification: 手工走一遍——编辑一条带图片的中段消息，确认图片被带回 composer 且可逐张删除；确认待丢弃区段可见；提交后 transcript 从该点重建并立刻开跑；取消后一切复原；**构造一次截断后失败（在 opener 里注入重建失败），确认重连后历史停在回退点、输入框内容仍在、且此时按发送走的是普通 `task` 并得到正确结果**；请求在途期间连按两次发送只产生一个请求
