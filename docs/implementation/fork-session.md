# Fork Session — 设计

## Problem

以用户自己发过的某条消息为切点复制一份会话，源会话原封不动。新会话带着**这条消息所在那一轮之前**的完整历史（含 sub-agent 线程）独立继续，而这条消息本身填进新会话的输入框——一次非破坏性的 rewind。需求：[`../requirement/fork-session.md`](../requirement/fork-session.md)

## Scope

**In**

- 存储层的事务性复制（含 `thread_id` 整体重映射），跑在 REPEATABLE READ 下。
- hub 的 idle 闸门，与 attach 抢同一把锁。
- `fork_session` RPC。
- Web 端两个入口：正文里每条用户消息上、会话列表上；以及切点尚未落库时的重试。

**Out**

- **不修保存时序。** 运行时是先把事件推给界面、再写数据库，两者没有先后保证，所以数据库可能短暂落后于用户看到的内容。fork 因此可能偶发失败，本设计只做重试补偿，不去改运行时——那是一件独立的事，已单独立项为需求 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md)，另见 Risks 与下面的「为什么不先修保存时序」。
- 不加 `forked_from` 溯源列。
- fork 不自动发消息、不自动跑 turn。
- 不支持切在 assistant 消息、工具结果或 sub-agent 线程消息上；也不支持切在会话第一条用户消息上（前面没有任何一轮）。
- 不动 todos 的存储形态（不按 turn 版本化）。

## Assumptions

- **保留集合以 turn 为单位**。一次提交在它触及的每个线程上打同一个 `turn_id`（`messages_turn` 索引即为此存在），且 turn 在每个线程内按顺序累积。所以「root thread 上 `seq < cut` 的 turn 集合」在任何线程上取到的都是一段**前缀**，`seq` 天然保持 `0..count` 连续，复制时无需重编号。这是整个设计的地基——若不成立，见 Risks 第一条。
- **`thread_id` 必须整体重映射，`message_id` 系列则原样保留**。两者的约束不同：`messages` 的唯一约束是 per-session 的，`turn_id` / `origin_message_id` 的引用在新 `session_id` 下依然自洽（迁移注释已背书）；而 `thread_id` 不是自由标识——root thread 的 id **就是** session_id，子线程的 id 是 `uuid5(parent_thread_id, derivation_key)`（`agent.rs:171`、`driver.rs:907`）。原样复制会同时坏掉两件事：storage 里所有 root thread 查询都写作 `thread_id.eq(&self.session_id)`，新会话将永远查不到自己的历史；而运行时下次调用同一个 stateful sub-agent 时会按新 root 重新派生 id，与复制过来的旧 id 对不上，sub-agent 就此失忆。`parent_thread_id` / `derivation_key` 两列的存在理由正是这个——`persist.rs:36` 的注释直接点名了 fork。
- **源会话所有线程停在 `Generation`**。这是 rewind 已有的检查（storage.rs:783），fork 沿用。于是复制过去的 `resume_point` 全是 `Generation`，`pending_approval` 全是 `false`，无需改写。
- **一个 turn 的开头是它的 user 消息**，`opening_user_message`（driver.rs:1107）用该消息的 id 派生 `turn_id` 并把消息记在同一个 turn 下。切点因此只需是 user 消息，`seq < cut_seq` 就精确等于「这一轮之前」。
- **数据库可能落后于界面**。运行时先 `emit_event`、后 `save_checkpoint`（root 走 driver.rs:297 的 `Done` 分支，sub-agent 更是先投递 Reply 再存档，见 driver.rs:772/837），而 hub 是从事件流折出内存 snapshot 并置 `turn_running = false`（hub.rs:369/1435）。所以「用户看到回复」严格早于「回复落库」。设计假定这个间隔通常远小于人从看到回复到点下 fork 的反应时间——fork 是手动触发的，这是它与 rewind 的关键差别。

  若这个假定不成立（数据库卡顿），后果**分两种，不能一概而论**：root 侧的落后要么被切点自身的前缀保证盖住（`cut: Some`，见 Data Model），要么被整份复制的条数校验转成可重试的错误；而 sub-agent 侧的落后，当它此前存的是 `Generation` 时挡不住，仍会静默产出一份非精确副本。详见 Risks 的四种表现与需求文档的 Known Limitations 第一条。
- 单个会话的线程数量在百量级，消息数量在千量级；线程元数据小，消息 payload 可能很大（内联 base64 图片）。

## Validation Findings

**问题**：消息复制能否留在数据库里，而不把每条 payload 拉进服务进程？
**方法**：把 `INSERT INTO messages (...) SELECT ...` 的 diesel DSL 写法追加进 `storage.rs` 跑 `cargo check`，随后还原。存档在 `.scratchpad/fork-insert-select/`。
**结果**：通过。两处踩坑——`into_columns` 挂在 `InsertStatement` 上，要写在 `.values(...)` **之后**；select 列表里的新 `session_id` 字面量需要 `IntoSql::into_sql::<Text>()` 才能定型。
**影响**：消息复制走 `INSERT ... SELECT`，payload 不出库。由于 `thread_id` 也要换成重映射后的值（见 Assumptions），语句从「整表一次」变成「每线程一次」，多带一个字面量——线程数量在百量级，代价可忽略，而 payload 依然留在库内。线程检查点要重算 `message_count`，本就走「读出—计算—写入」，与按线程遍历天然合流。

## Alternatives Considered

**闸门放在哪。** 备选是 RPC 层先问 hub「源会话闲吗」，再直接调 storage 复制。落选原因是两步之间有窗口：源会话可能在查询与复制之间开跑。选定方案把 idle 判断与复制一起放进 hub 的 entry lock 内。

这里有个容易漏的坑：不能照 `delete` 那样「查不到 entry 就直接放行」（hub.rs:1168）。查不到与放行之间同样不是原子的——一个并发的 attach 可以在这中间插入 entry、开 runtime、发 turn，而 fork 已经开始复制。所以 fork 走的是 `lock_entry_for_attach`（hub.rs:496，get-or-insert 并持锁），与 attach 抢同一把锁。

**摘除临时 entry 时必须先立墓碑。** 复制结束后若 phase 仍是 `Uninitialized`（说明这个 entry 是 fork 自己建的），不能直接从 map 删掉了事：一个并发的 attach 可能已经从 map clone 走了这个 `Arc` 并阻塞在 entry mutex 上，它拿到锁时手里的 entry 已不在 map 里，却看到 `Uninitialized` 而照常开起 runtime——于是有了一个跑着却查不到的孤儿会话，任何 command 都够不着它。正确顺序是**先把 phase 置为 `Released`，再从 map 移除**：`lock_entry_for_attach` 的 `Released` 分支本就是为这种情况写的（注释称之为 "Tombstone from a raced release"），等待者拿到锁看到墓碑会回 map 重新取新槽位。

**为什么不先修保存时序。** 备选是先做一个前置改动，让持久化早于对外可见的 settle 信号（sub-agent 两处 reply 顺序 + root 侧把 save 提到 emit 之前），建立「数据库 ≥ 界面」的不变量，fork 之后就能直接读库。这个方向本身是对的，落选是因为代价与收益不匹配：它反转了 Reply 的投递语义（当前「先投递后存档」是至少一次，改后若不额外写一次就变成至多一次，caller 会永久等一个不来的回复），需要自己的崩溃恢复测试面；而它为 fork 消除的，是一个由人手点击天然拉开的瞬时窗口。结论是把它留作独立的后续工作——已立项为 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md)，fork 这边先用重试补偿，等它落地后本设计的四种补偿即可拆除。

**另一个备选**是 fork 时在锁内 graceful shutdown 源 runtime 再重建——`Shutdown::graceful_unbounded()` 是系统里唯一真正的持久化屏障，rewind 正是这么做的（hub.rs:765 的注释解释了为什么非它不可）。落选是因为它要打断源会话并重建，与「源会话无感」直接冲突，还要把 `handle_rewind` 那套失败恢复再写一遍——rewind 付这个代价是因为它降低 `message_count` 后滞后的写回是**静默数据损坏**，而 fork 只读源、写新库，最坏是复制少一段，量级完全不同。

**切点怎么表达。** 备选是按 root thread 的 `seq` 直接截断。落选原因是它只对 root thread 成立：sub-agent 线程有自己的 `seq` 序列，按 root 的 seq 截断无法在其它线程上定位对应位置，还得反过来推 turn。既然最终仍要落到 turn，不如一开始就以 turn 为单位。

**新 `session_id` 谁生成。** 备选是客户端 mint（与 `open_session` 一致），好处是重发同一请求会撞 `SessionExists`，天然幂等。选定服务端生成，因为 fork 出的会话不走 draft 流程（它一创建就带历史，客户端直接 `open_session` 即可），让客户端预先造 id 只是多一份职责。代价是重复提交会得到两个副本——见 Risks。

## Components

- **`WorkspaceStorage::fork_session`** — 在一个事务内校验切点、复制会话行/消息/线程检查点，返回新会话的标识。
- **`SessionOpener::fork`** — hub 到 storage 的注入点，与既有的 `rewind` 并列，使 hub 测试可以替换掉数据库。
- **`SessionHub::fork`**（`SessionRelay` 新方法）— 在源会话的 entry lock 下判定 idle，然后委托给 opener。
- **Web `forkSession`** — 发起请求、更新会话目录、切到新会话。

## Interfaces

```rust
/// 复制 `source_id` 的会话到一个新铸的 session_id 下。
///
/// `cut` 为 `At` 时保留该 user 消息所开启 turn 之前的全部 turn；为 `All` 时
/// 保留全部历史。源会话完全只读。
///
/// 全或无：任何一步失败都回滚，不留下半个会话。整个操作跑在 REPEATABLE READ 下，
/// 所以按线程分批的复制看到的是同一个快照。
///
/// `source` 为 `Live` 且 `cut` 为 `All` 时，调用方内存里那份权威 root 历史的
/// 条数用来判断库里是否已经追上，少了就返回 `Lagging`。`cut` 为 `At` 时不做
/// 这项校验——切点查得到本身就证明了它之前的 root 前缀已落库（见下），再比全量
/// 只会让「从早轮 fork」被无关的最新轮次挡住。
pub async fn fork_session(
    &self,
    source_id: &str,
    cut: ForkCut,
    source: ForkSource,
) -> Result<ForkedSession, ForkError>;

pub enum ForkCut { At(MessageId), All }
pub enum ForkSource { Live { root_messages: usize }, Cold }

pub struct ForkedSession { pub session_id: String, pub name: Option<String> }

pub enum ForkError {
    /// 切点不存在，或不是本会话 root thread 上的一条 user 消息。
    /// 客户端只提供这一个身份，所以伪造或过期的切点也落在这里。
    ///
    /// 也覆盖「切点刚生成、还没落库」这一瞬（见 Assumptions）。两者在库里无法
    /// 区分，所以 RPC 层统一映射成一个可重试的错误码，由前端重试一次。
    CutNotFound,
    /// 某个线程不在普通生成边界上，持久化状态不是一个静止点。
    ///
    /// 是硬失败还是可重试，由**调用方的上下文**决定，storage 自己分不出来：
    /// 源会话 live 且已通过 idle 检查时，这多半只是某个线程的收尾 checkpoint 还没
    /// 落库（它此前存的是 `ToolExecution`），属于可重试；源会话没有 live entry 时，
    /// 持久化状态就是全部真相，那才是真的停在非生成边界（例如上次崩溃留下的）。
    ThreadBusy { thread_id: String },
    /// 复制后某线程的消息不再是 `[0, count)`。保留集合只应取到前缀，
    /// 所以这意味着上游不变量已破——回滚，而不是留下带洞的历史。
    HistoryNotContiguous { thread_id: String },
    /// 数据库的 root 历史比调用方内存里的短——最新一轮还没落库。可重试。
    /// 只挡得住 root 那一半；sub-agent 那一半挡不住，见 Risks 的已知限制。
    Lagging { expected: usize, found: usize },
    /// 新铸的 id 已被占用。绝不能退化成 do_nothing，否则消息会灌进别人的会话。
    SessionExists,
    Persistence(String),
}
```

```rust
/// 复制一份 `source`，不改动它。任何连接都可以 fork 任何会话——这是非破坏性
/// 操作，不套用 delete 的 latest-wins 校验。
///
/// 源会话若正跑 turn、有待批准的工具调用，**或还有排队未跑的任务**，拒绝并返回
/// `NotIdle`，此时什么也没发生。三个条件缺一不可——见 Load-Bearing Decisions 12。
/// 源会话 live 且 `cut` 为 `None` 时，把内存 snapshot 的条数一并交给 storage 做落库校验。
fn fork<'a>(&'a self, source: SessionKey, cut: Option<MessageId>)
    -> Pin<Box<dyn Future<Output = ForkOutcome> + Send + 'a>>;

pub enum ForkOutcome {
    Forked(ForkedSession),
    NotIdle,
    /// 数据库还没追上，重试即可。hub 在这里把 storage 的 `CutNotFound` / `Lagging`
    /// 归拢进来，并按上下文把 live 源会话的 `ThreadBusy` 也归入此类。
    Retryable(String),
    Failed(ForkError),
}
```

**信任边界**在 RPC 的 `fork_session` handler：`workspace_id` 按既有方式解析到配置的工作区，`cut_message_id` 只是一个 UUID，其「是本会话 root thread 上一条 user 消息」的全部含义由 `fork_session` 在事务内的单条查询裁决。下游不再重复校验。

```
// RPC
fork_session { workspace_id, session_id, cut_message_id? }
  -> { session_id, name, workspaces }   // 带上刷新后的目录，与 delete_session 一致
```

## Data Model

不加表、不加列，迁移文件不动。

**保留集合怎么算出来**（`cut` 为 `Some` 时；`None` 时跳过，保留全部）：

1. 用 `cut_message_id` 在 **root thread**（`thread_id = 源 session_id`）上查它的 `seq`，同时校验 `role = 'user'`。查不到即 `CutNotFound`——这一条查询就是切点的全部校验。
2. 取 root thread 上 `seq < cut_seq` 的所有 **distinct `turn_id`**，即为保留的 turn 集合。`turn_id` 是 UUID、本身无序，「这一轮之前」只能由 root thread 的 `seq` 顺序回答，所以这一步绕不开。
3. 复制 `turn_id ∈ 该集合` 的消息，**不带线程条件**——这正是 sub-agent 线程里属于这些 turn 的消息被一并捞出的原因。

这是 **rewind 的严格镜像**：rewind 从一条 user 消息起删掉 `seq ≥ target` 的 turn，fork 留下 `seq < target` 的。同一个锚点、同一条校验，两个方向。第 3 步按整个 turn 复制，所以副本一定落在 turn 边界上。

**切点为什么必须是 user 消息**，而不是任意消息：user 消息是一个 turn 的开头，选它等于说「从这一轮开始不要」。选别的消息就得回答「这一轮到底要不要」，而无论哪个答案都不好——要，就得面对下面那个半写窗口；不要，那它和这一轮的 user 消息就是同一个意思，只是说法更绕。

**切点查得到 ⟹ 保留的内容已全部落库。** checkpoint 写入是一个事务，消息从 `stored_count` 起按 `seq` 连续 append 并同步更新 `message_count`（storage.rs:613-660），所以 `seq = k` 的行可见就意味着承载它的那次事务已提交，`0..k` 一个不缺——而保留的正是 `0..k-1`。所以 `cut: Some` **不需要**任何全量落库校验：反过来若比对整份 snapshot 的长度，一个尚未落库的第 5 轮会把「从第 3 轮 fork」也一并挡掉，与需求里「从更早的点 fork 完全无关」直接冲突。

这条保证还顺带盖住了一个曾经很难缠的窗口：driver 在 turn 一开始就单独为 user 消息写一次 checkpoint（driver.rs:292-295，让中途崩溃的快照里已有 prompt），此后到 turn 结束才写第二次；而 abort 事件**先于**那次收尾写入 settle 掉 turn（hub 的 `event_settles_turn`），于是存在一段「hub 说空闲、库里只有这个 turn 的开头」的时间。切点落在 user 消息上时，**那个半写的 turn 恰好就是被分叉掉的那个**，怎么都碰不到。

**thread_id 重映射**是复制的第一步，其余写入都依赖它：

```
old_root (= 源 session_id)          ->  新 session_id
每个子线程（自顶向下，父先于子）    ->  ThreadId::from_uuid5(新父 id, derivation_key)
```

必须复用 `coda_agent::ThreadId::from_uuid5`（已从 crate 导出，`hub_tests.rs:496` 即在用），不能自行实现 uuid5：非 UUID 的父 id 走的是 hash 进固定命名空间的兜底分支，重写一遍就会算出不同的结果。映射表对**所有**检查点行都算（含消息已被切光的线程），只在插入时跳过空线程——否则一个仍有消息的线程可能找不到自己的父。父不在映射表里即为不变量已破，回滚。

新会话写入：
- `sessions` 一行：`model_binding` 与 `name` 都原样复制源的值。**不加 `fork: ` 前缀**——最初的设想是靠前缀在列表里辨认，但未命名会话（常见路径）本就没有名字可加前缀，列表会回退到首条消息预览，前缀对它们完全不起作用；只对已命名会话生效的标记价值不足以换命名规则的复杂度（叠加、120 字截断）。副本靠 `updated_at` 排在列表最前来辨认，用户可自行改名。
- `messages`：按线程逐个 `INSERT ... SELECT`，`session_id` 与 `thread_id` 换成新值，`seq` / `message_id` / `turn_id` / `origin_message_id` / `origin_call_id` 原样。
- `thread_checkpoints`：仅复制尚有消息留存的线程；`thread_id` 与 `parent_thread_id` 走映射表，`derivation_key` 不变（stateless 线程的 key 是 `(message_id, call_id)` 对，而 message_id 原样复制，所以它依然成立），`message_count` 按复制后的实际条数重算。一条消息都没留下的线程整行不复制（与 rewind 的删除分支同理——缺失的检查点与空检查点还原出的状态相同）。`reply_target` 若非 `None`，其 `sender_thread_id` 一并映射；在 `Generation` 边界上它本应已被清空（`driver.rs:756` 发出回复时 `take()`），映射只是不依赖这个推断。
- `runtime_snapshots`：**不复制**。它装的是运行时排队中的信封（`active_threads` 里还存着一份 thread_id），属于源会话的运行时，新会话从静止开始。

`todos` 原样复制源线程当前的值，**不清空、不按 turn 还原**——与 rewind 的现有行为一致（`rewind_to` 同样不碰 todos）。代价说清楚：从历史切点 fork 时，切点之后那些轮次写入的 todo 状态会跟着进新分支，且新会话的 agent 之后调 `read_todos` 就能读到这些「未来」状态。`thread_checkpoints.todos` 只存线程当前值、不按 turn 版本化，要做到「切点时的 todos」就得改存储形态，那超出本设计的范围。需求文档的 Known Limitations 第二条已披露这一行为。

**共享可变状态是存在的**：源会话的 runtime 正在并发往同一个数据库写 checkpoint，entry lock 挡不住它（那把锁只序列化 hub 的命令，不是持久化屏障）。REPEATABLE READ 保证 fork 全程看同一个快照，所以复制出的结果内部自洽；但快照本身可能还没追上界面，见 Risks 的已知限制。

## Load-Bearing Decisions

1. **`thread_id` 重映射、`message_id` 系列不动** — 两类标识的约束不同（见 Assumptions）。代价是复制必须按线程分批、且要走一趟拓扑序；换来的是新会话的 root 查询能命中、stateful sub-agent 不失忆。
2. **保留单位是 turn，不是 message** — 换来跨线程的一致切分和 `seq` 的连续性；代价是切点粒度只能到 turn 边界。
3. **不加 `forked_from`** — 依需求；代价是日后想做支线视图要补迁移。
4. **新会话行用严格 insert** — 冲突即报错。绝不用 `on_conflict do_nothing`（`initialize_session` 那样），否则 id 撞车时消息会灌进一个已存在的会话。
5. **idle 判定与复制同处 entry lock，且走 `lock_entry_for_attach`** — 与 attach 抢同一把锁，杜绝「查不到 entry 就放行」与并发 attach 之间的竞态；代价是大会话复制期间该 entry 短暂阻塞。
6. **`cut` 可选，`None` 即全量** — 把「整份复制」定义成同一机制的特例，而不是让前端去找最后一条最终回复（那样最后一轮若被 abort 在工具执行中就会被静静丢掉）。
7. **REPEATABLE READ** — 按线程分批的复制必须看同一个快照，否则 READ COMMITTED 下每条语句各取快照，源会话的并发写入会把复制切得前后不一致。`build_transaction().repeatable_read()`（diesel-async 0.9 支持）。
8. **todos 复制最新值** — 与 rewind 一致；代价是从历史切点 fork 会带上后续轮次的 todo 状态。
9. **接受数据库短暂落后于界面** — 不改运行时的保存时序。`cut: None` 用 `expected_root_messages` 校验挡成可重试的 `Lagging`，`cut: Some` 靠切点自身的前缀保证、不做全量校验；sub-agent 那半挡不住，作为明知的正确性缺口接受，需求文档已写明 fork 不是精确副本。
10. **摘除 fork 建的临时 entry 前先立 `Released` 墓碑** — 否则已 clone 走 `Arc` 的等待者会在脱离 map 的 entry 上开出孤儿 runtime。
11. **新会话不加 `fork: ` 前缀，名字原样复制** — 前缀对未命名会话（常见路径）不起作用，只对已命名会话生效的标记不值得叠加与截断规则；代价是副本与源同名，靠列表排序区分。
12. **idle 判据是三条而非一条：`!turn_running && pending_approvals.is_empty() && unsettled_user_messages.is_empty()`** — `turn_running` 单独不足以证明会话空闲。hub 允许在 turn 运行时继续 `Task`（`handle_task` 无守卫，hub.rs:684），排队的用户消息堆在 `unsettled_user_messages` 里；一轮 settle 时 `fold_settled_turn` 只 `pop_front()` 一条，随后无条件 `turn_running = false`（hub.rs:1435），而下一条排队任务要等自己的 `LlmStart` 才把它重新置 true（hub.rs:1417）。fork 若落在这中间，会把仍有排队任务的会话判成空闲，复制期间 runtime 正好把那条任务跑起来并写库。加上队列判空才封住这个窗口。

## Risks / Open Questions

- **最大风险：thread_id 重映射不完整。** 少映一处的后果都不是报错而是静默错乱——root 查不到历史、sub-agent 失忆、或者两条线程指向同一个 id。已知需要映射的位置有四处：`messages.thread_id`、`thread_checkpoints.thread_id`、`thread_checkpoints.parent_thread_id`、`reply_target.sender_thread_id`。对策是在 pg-tests 里断言复制后的会话中**不存在任何一个源 thread_id 的字面残留**（含 jsonb 内部），而不是逐字段比对——这样日后新增带 thread_id 的字段时测试会直接失败。
- **保留 turn 集合在每个线程上未必是前缀。** 若某个线程的 turn 不按顺序累积，复制出的 `seq` 就有洞，而后续每次保存都会从错误的水位往后追加。对策是照搬 rewind 的 `max_seq + 1 == count` 校验（storage.rs:879），把破掉的不变量变成回滚而非坏数据；并在 pg-tests 里用一个带 stateful sub-agent、跨多个 turn 的会话直接验证。
- **已知限制：数据库短暂落后于界面。** 根因是运行时先发事件、后写数据库（见 Assumptions）。fork 从数据库读，于是有四种表现，其中前三种已挡住或绕开、第四种接受：
  1. 切点还没落库 → `CutNotFound`，可重试。
  2. 切点所在 turn 还只写了开头 → 不影响；这一轮本来就不复制，而切点可见已证明它之前的 root 前缀完整。`cut: Some` 因此不做 `root_messages` 校验。
     `cut: None` 若最新一轮还没落库，则由 `root_messages` 校验挡下，返回 `Lagging`，可重试。
  3. 某个线程的收尾 checkpoint 还没落库、而它此前存的是 `ToolExecution` → storage 报 `ThreadBusy`。源会话 live 且已过 idle 检查时，这归入可重试；只有没有 live entry 的源会话报 `ThreadBusy` 才是真的停在非生成边界。
  4. **某个 sub-agent 的收尾存档还在路上，而它此前存的是 `Generation` → 新会话里那条线程少最后一段，且不报错。** 挡不住：hub 的内存 snapshot 只 fold root 的事件（hub.rs:369），它不知道 sub-agent 应该有多少条，没有可比对的期望值。

  第 4 种**通常**集中在最新轮次，但这不是运行时保证：`save_checkpoint` 是在 sub-agent 自己的 task 里 await 的，拦不住已经拿到 Reply 的 root 继续往下跑，所以数据库严重卡顿时，第 3 轮 sub-agent 的存档完全可能拖到第 5 轮仍未完成——那时从第 3 轮 fork 一样会缺它的末段。换句话说，**任何仍有 checkpoint 在途的保留轮次都可能受影响**，只是越早的轮次越不可能。**这是一个明知的正确性缺口，不是被论证掉的风险**——需求文档的 Known Limitations 第一条已相应写明，fork 不声称是精确副本。根治办法是让存档早于对外可见的 settle 信号，那是独立的后续工作；做完之后这里的四种补偿都可以拿掉。
- **重复提交产生多个副本**（服务端 mint id 的代价）。前端在请求在途时禁用入口即可，不做服务端去重。**「入口」是按源会话算的**，不是按按钮：一个会话在除第一条外的每条 user 消息上都有一个入口，会话列表里还有一个，所以在途标记必须放在 store 里（`CodaState.forking`，键为 `forkKey`），由 `forkSession` 自己置位与清除，而不是各按钮自管本地 state。注意这与上一条的重试相互作用：只有 `ForkOutcome::Retryable`（即上面第 1、2、3 种，全都发生在写入之前）才重试，`Failed` 与 `NotIdle` 一律不重试。
- **大会话复制的耗时**在事务与 entry lock 内。消息走 `INSERT ... SELECT` 已经把最大的一块留在库内，预期可接受；若日后成为问题，再考虑把复制挪出锁外并改用乐观校验。

## Implementation Roadmap

- [x] [风险验证] thread_id 重映射 + `WorkspaceStorage::fork_session` + pg-tests：一个带 stateful sub-agent、跨 3 个 turn 的会话，切在第 3 个 turn 的 user 消息上并保留前 2 个 turn
      Purpose: 同时验证两个地基假设——重映射无遗漏，且保留集合在每个线程上都是前缀
      Verification: `cargo test -p coda_server --features pg-tests --test storage_pg`；断言新会话中不含任何源 thread_id 的字面残留（含 jsonb 内）、子线程 id 等于按新 root 重新派生的值、每线程 `seq` 为 `0..count`、`message_count` 与实际条数相符、源会话逐行未变
- [x] [存储] 补齐错误路径与隔离级别：切点非法、线程未停在 `Generation`、id 冲突、`cut: None` 全量，事务改 `repeatable_read()`
      Purpose: 把切点这个唯一的客户端输入锁死在一条查询里，并让分批复制看同一个快照
      Verification: 每种 `ForkError` 一个用例，且都断言源会话与数据库无残留
- [x] [hub] `SessionOpener::fork` + `SessionHub::fork`，闸门走 `lock_entry_for_attach`，摘除临时 entry 前立 `Released` 墓碑，live 时传 `expected_root_messages`
      Purpose: 让「源会话在跑」与「源会话没打开」两条路都走对，不被并发 attach 插队，也不留下孤儿 runtime
      Verification: `hub_tests.rs` 覆盖 entry 不存在 / Live 且 idle / Live 且 turn_running / 有 pending approval；外加三个竞态用例——(a) fork 在 storage 调用处暂停时并发 attach + task，断言后者进不了复制窗口；(b) 让 attach 在 fork 摘除 entry **之前**完成 `Arc` clone、**之后**才拿到 entry lock，断言它看到墓碑后回 map 重取，而不是在孤儿 entry 上开 runtime；(c) **排队任务窗口**——连发两条 task，让测试确定性地停在第一条 settle 之后、第二条 `LlmStart` 之前，断言 fork 返回 `NotIdle` 而非复制出一份被排队任务写坏的副本。再加三个落库校验用例：`cut: None` 且内存 snapshot 比库里长时返回 `Lagging`；**同样状态下 `cut: Some` 指向已落库的早轮时照常成功**；live 源会话报 `ThreadBusy` 时归为 `Retryable`、无 live entry 时归为 `Failed`
- [x] [RPC] `fork_session` 方法、wire 类型与目录刷新，`CutNotFound` / `Lagging` 映射成同一个可重试错误码
      Purpose: 打通协议边界，并把「数据库还没追上」这一瞬表达成前端能处理的形态
      Verification: wire 类型 roundtrip 测试；`cargo clippy` + `cargo test`
- [x] [Web] 除第一条外每条用户消息上的 fork 按钮（与 rewind 的编辑按钮并列）+ 会话列表入口 + 切到新会话并把切点消息填进输入框 + 重试
      Purpose: 暴露到真实交互，并兜住保存时序带来的偶发失败
      Verification: `pnpm --filter coda-web lint` + `test`；用例覆盖「fork 后目录含新会话且当前会话切过去」、「切点消息落进副本的输入框」、「首次可重试错误后自动重试一次即成功」、「一次分叉在途时同源的第二次请求根本不发出」、「非可重试错误不重试」

## Deviations from Design

- 多了一个 `ForkError::SourceNotFound`：设计没提源会话不存在的情况，而 `session_id`
  来自客户端，`cut: None` 时会静默建出一个空会话。
- 多了一个 `ForkError::OrphanThread`：检查点的父不在映射表里时回滚，对应 Data Model
  里「父不在映射表里即为不变量已破」那句。
- 多了一个 `ForkError::SourceNotIdle`，映射到 `ForkOutcome::NotIdle`：设计只让存储层
  查检查点，但检查点证明不了冷会话是空闲的 —— 排在上一个 turn 后面的任务会随
  `runtime_snapshots` 活过关闭，而它留下的检查点都是 `Generation`，下次 `Session::open`
  又会把这活捡回来。所以冷路径额外查一次运行时快照。这个检查**只在冷路径做**：
  那一行只在 agent 退出时重写，会话活着的时候它描述的是上次关闭，不是现在。
- Web 端 fork 入口没走「hub 借来的 entry」那条路的额外提示；`ForkOutcome::Retryable`
  与 `CutNotFound` / `Lagging` 一起映射到 `FORK_NOT_READY`，前端只重试这一个码。
- **切点从「AI 的最终回复，含该轮」改成「用户自己的消息，不含该轮」**，是产品语义的变更，
  需求文档已同步。动机是设计原本的锚点覆盖不到「工具执行中被中断的那一轮」（它没有最终
  回复），而换锚点比给它开特例更简单：新语义是 rewind 的严格镜像，同一个锚点、同一条校验。
  连带效果——`cut: Some` 重新回到「不做任何落库校验」的干净规则，因为可能半写的那个 turn
  恰好是被分叉掉的那个（见 Data Model）；前端也不再需要判断「这一轮结束了没有」。
  代价是正文里最后一轮之后没有下一条用户消息可挂，「从最新一轮之后分叉」只剩会话列表这一个
  入口。
- 切点消息本身填进新会话的输入框（`OpenedSession.seed`，composer 靠 `key` 重挂载时读一次，
  发送时清除）。设计里没有这一条——它是换锚点后才成立的：那条消息不进副本，总得有个去处。
