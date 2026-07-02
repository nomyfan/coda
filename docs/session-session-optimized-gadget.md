# Session 中继层:command/events 转发 + 单客户端 latest-wins + 中途无缝 replay

## Context(为什么做)

现状:每个 WebSocket 连接的 `run_dashboard`(app/coda_server/src/bin/server.rs:1232)私有地持有活跃 `Session`。断连后有 turn 在跑时会"延命"到 turn 结束,但事件被丢弃;重连从磁盘 checkpoint 重建。三个问题:

1. **并发 open 竞态** — `SessionBuilder::open()` 无守卫,重连太快会出现同一 session_id 两个活实例互相覆盖 checkpoint;
2. **中途事件丢失** — 重连的 Snapshot 只有上次 checkpoint 之前的历史,进行中 turn 的流式输出看不到;
3. **无单客户端保证** — 两个 client 可同时驱动同一 session;
4. **用户输入不立即落盘**(已确认的 bug)— 用户消息在 `handle_envelope` 时只进内存历史(driver.rs:253-261),整个 turn 结束才随 checkpoint 持久化(driver.rs:326),中途崩溃/重启会丢这条输入。本计划顺带修复(见 driver.rs 改动),且是 replay 设计的前置条件。

## 设计思路:在 Session 之上加一层 command/events 中继

`coda_agent` 的 `Session` 保持原样(仅一处小改,见下)。在 server 层加一个**进程级中继层**:命令从连接进入中继、事件从中继流向连接,session 生命周期归中继管,与连接解耦。

**对外只有一个抽象**(将来换 Redis 时替换其实现,server.rs 不动):

```rust
// app/coda_server/src/hub.rs(新)
pub type SessionKey = (String, String);   // (workspace_id, session_id)

/// 连接面对的唯一接口。所有输入输出都是可序列化纯数据(无闭包)——
/// 这保证将来的多实例实现可以把命令转发给 owner 实例、事件经共享日志回传,
/// 不依赖 LB 分流。
#[async_trait-ish]
pub trait SessionRelay: Send + Sync {
    /// OpenSession:latest-wins attach。驱逐同 key 上的前一个客户端。
    /// 返回快照 + 事件流:流从当前 turn 的 replay 起点 cursor 开始,先重放已发生
    /// 的事件再无缝转 live;`Evicted`/`Closed` 作为流内最后一个元素到达并终结流。
    /// 订阅语义 = {key, conn_id, cursor} + 顺序事件流 —— 与 EventLog cursor 同构,
    /// Redis 版直接映射为 tail Redis Streams,接口不变。
    async fn attach(&self, key: SessionKey, conn_id: ConnId, provider_id: String,
                    effort: Option<ReasoningEffort>) -> Result<AttachSession, OpenError>;
    async fn command(&self, key: SessionKey, conn_id: ConnId, cmd: SessionCommand) -> CommandOutcome;
    async fn detach(&self, key: SessionKey, conn_id: ConnId);          // CloseSession
    async fn detach_all(&self, conn_id: ConnId);                        // 断连
    async fn delete(&self, key: SessionKey) -> bool;                    // DeleteSession
    async fn shutdown_all(&self);                                       // 进程关停
}

pub enum SessionCommand {          // 纯数据
    Task { task: String, images: Vec<String> },
    Resume { agent_name: String, thread_id: String, decision: ResumeDecision },
    Abort,
    SetModel { provider_id: String, effort: Option<ReasoningEffort> },
}

pub enum RelayEvent {              // 流内元素,纯数据
    Event(Box<WireEvent>),
    Evicted,                       // 被后来者驱逐(终结流)
    Closed,                        // runtime 终止(终结流)
}

pub struct AttachSession {
    pub snapshot: SnapshotPayload,                 // messages、pending_approvals、provider_id、effort、turn_running
    pub events: BoxStream<'static, RelayEvent>,    // replay ++ live 统一为一条流
}
```

连接侧 `run_dashboard` 用 `tokio_stream::StreamMap<SessionKey, BoxStream<RelayEvent>>` 同时收多个已 attach session 的流(进程内实现下每条流由 per-attachment 的 unbounded mpsc 泵出,驱逐/detach 时关闭发送端终结流)。

Session 的构建逻辑(现有 `open_session`,server.rs:224 的 RunConfig 组装)包装成一个构造期注入的工厂(`Arc<dyn SessionOpener>`:`open(key, provider, effort, decisions)` + `load_messages(key)`),配置各实例皆有,任何实例都能本地构建 —— 命令不携带构建逻辑。

**进程内实现 `SessionHub`**(本次唯一要写的实现)。内部结构是实现细节,不进抽象:

```
SessionHub
 ├─ opener: Arc<dyn SessionOpener>
 ├─ entries: std Mutex<HashMap<SessionKey, Arc<SessionEntry>>>       // 注册表
 └─ SessionEntry.inner: tokio Mutex<EntryState>
      ├─ state: Live { session, root_name, provider_id, effort, generation,
      │                turn_running, snapshot: Vec<Message>,          // 内存权威历史
      │                unsettled_users: VecDeque<Message>,
      │                pending_approvals, log: EventLog }             // 当前 turn 的事件
      │        | Pending { needed, decisions, approvals, snapshot, ... }   // 审批门控 open
      │        | Releasing { done: watch::Receiver<bool> }           // 释放中(shutdown 在锁外进行)
      │        | Released                                            // 墓碑
      └─ attached: Option<Attachment { conn_id, tx }>                // 至多一个 = 单客户端
```

- **每个 entry 的事件消费分两级**(都由 hub 拥有):
  - **一级泵(pump)**:专职 task,紧循环 `session.recv()` → 推入 per-entry unbounded mpsc,**不取任何锁** —— 把 broadcast 接收路径压到最短,最大限度降低 lag 概率(正确性兜底由 `Lagged` 显式处理承担,见并发要点);
  - **二级消费(forwarder)**:从 unbounded mpsc 取事件 → entry 锁内转 `WireEvent`、append 到 log、转发给 attached 的流。
- **EventLog 是私有 struct**(cursor 单调、`VecDeque`),非公开 trait;将来 Redis 实现内部用 Streams,与本接口无关。
- **单客户端/驱逐**就是 `attached` 槽位替换 + 给旧 tx 发 `RelayEvent::Evicted`;旧连接之后的 command 因 conn_id 不匹配被拒(warn)。将来 Redis 实现内部用分布式租约实现同一语义。
- **生命周期**:entry 存活当 `turn_running || attached.is_some()`;两者皆无 → 释放。释放走 `Releasing { done: watch::Receiver<bool> }` 状态(desynced draining 复用同一机制):**锁内**只做状态转移(创建 `watch` 通道)并把 `Session` 取出,**锁外** `shutdown().await`(最终 checkpoint 落盘),完成后先从 map 移除、再 `send(true)`。等待方在**锁内**克隆 receiver,锁外 `while !*rx.borrow_and_update() { rx.changed().await }`(sender 被 drop 时 `changed()` 返回 Err,同样视为完成)—— `watch` 的值本身承载"已完成"状态,**不存在 `Notify::notify_waiters()` 的 missed-wakeup 竞态**(释放任务在 waiter 注册前完成也能被 `borrow_and_update()` 看到)。重开只能发生在 map 移除之后 → 新 entry 读到的磁盘必在 checkpoint barrier 之后。现有 `close_requested` 延迟关闭机制整个删除(被此规则取代)。

## 两个已验证的代码事实(设计依据)

1. **settle 事件在最终 checkpoint 之前发出**(driver.rs:313-319 发 `Suspended`;`handle_generation` 内先发 `LLMEnd`;`save_checkpoint` 在 driver.rs:326 之后才跑;forwarder 消费 broadcast 还有任意延迟)。→ 不能在 settle 时靠重读磁盘截断日志;**中继在内存维护权威 snapshot,settle 时把本 turn 事件 fold 进去再清空 log**,整个动作在 entry 锁内原子完成。磁盘只在 entry 创建时读一次(open 已串行化,无竞态)。
2. `Session::resumed_messages()`(session.rs:389)open 时即返回根历史 → Live 路径的初始 snapshot 不需要 `load_checkpoint`;Pending 路径照旧读磁盘。

其他事实:`Session` 是 `Clone`(Arc);`recv()` 是 broadcast 多消费者、lag 丢事件(中继模式下每 session 只剩一个消费者,更不易 lag);四种 turn 结局都会 checkpoint;`OpenError::PendingApprovalsRequired` 不启动 runtime。

## 关键流程

**Attach(处理 `OpenSession`)**
1. map 锁取/建 `Arc<SessionEntry>`(不跨 await);锁 `entry.inner`,遇 `Released` 墓碑回步骤 1 重试;遇 `Releasing` 则锁内克隆 `done` receiver、锁外等其变 true(或 sender drop)后回步骤 1。
2. 若 `attached` 属别的连接:发 `RelayEvent::Evicted` 并替换(**latest-wins**);同连接重复 open = 幂等刷新。
3. 新 entry:调 `opener`。`Ok` → Live(snapshot = `resumed_messages()`,`turn_running = has_resuming_agents()`,spawn forwarder);`PendingApprovalsRequired` → Pending(snapshot 从 `opener.load_messages`);其他错 → 移除墓碑、`send_open_error`。**持 entry 锁跨 open await 是有意的** —— 这就是并发 open 的串行化守卫。
4. 锁内组装 `AttachSession`:snapshot(messages = snapshot ++ unsettled_users、`turn_running`、pending_approvals)+ 事件流(起点 = `log.first_cursor()`,先吐当前 turn 日志再接 live)。**流注册与 replay 起点截取在同一临界区**(forwarder 也在此锁下 append)→ 每个事件恰好一次且有序地出现在流中。
5. 连接侧发送顺序:`Snapshot` → 从流逐条转发 `Event`(replay 与 live 对连接侧无区别)。

**settle 时的 fold(forwarder 内,entry 锁下)**:`event_settles_turn`(现 server.rs:647,移入 hub.rs)命中时 —— ①**先消费 log 开头连续的根线程 `ToolCallEnd`(stale-envelope 清理事件)推入 snapshot,再弹出 `unsettled_users` 队首插入,再继续扫剩余 log** —— 因为真实 history 是先写 aborted `ToolMessage` 再写新 `User`(driver.rs:434/449、480/507),用户消息不能排在它们前面;②剩余 log 按序把**根线程**的 `LlmEnd`/`ToolCallEnd` 消息推入 snapshot(子 agent 事件跳过,与 checkpoint 历史渲染一致);③清空 log;④`turn_running = false`;⑤跑生命周期检查。`Suspended` settle 时先记入 `pending_approvals`。

fold 的完备性依赖"**凡写入历史的消息必有对应事件**",而现在 abort 路径不满足:generation abort 把 aborted assistant message 写入历史但只发 `Aborted`(driver.rs:622、631),tool abort 把 aborted ToolMessages 写入历史也只发一个 `Aborted`(driver.rs:956),stale-envelope 注入的 aborted ToolMessages 同样无事件(driver.rs:434-447)。→ **在 driver 里先补发 `LLMEnd`/`ToolCallEnd` 消息事件、再发 `Aborted`**(见 coda_agent 改动)。**settle 判定保持单一**:`event_settles_turn` 对 `message.aborted == true` 的 `LlmEnd` 返回 false,`Aborted` 是 abort 路径唯一的 settle marker —— 否则 aborted `LlmEnd` 先命中 settle,无人 attach 时 entry 提前开始释放,随后的 `Aborted` 会进不了 replay/live 流。

**Command**:`Task`/`Resume` 在 entry 锁内按 **校验 → `session.send/resume(...).await` → 仅成功后才写状态**(`turn_running = true`、`Task` 把 `Message::User(...)` 推入 `unsettled_users`、`Resume` 清对应 pending_approval)的顺序执行 —— send 失败不能留下 phantom 用户消息或永久 running 的 entry(对齐现有 server.rs:913 只在成功后置 running 的行为)。整段在锁内,与 fold/attach 原子互斥。`Resume` 对 Pending entry:收集 decisions,凑齐后锁内经 opener 重开升级为 Live(更多审批则维持 Pending,复用 `send_pending_approval_events`)。`SetModel`:仅 Live、当前 attach 者、`!turn_running`;先开新再 abort 旧(沿用 server.rs:1105-1130 顺序),`generation += 1`,旧 forwarder 靠 generation 不匹配自退。

**Detach / 断连**:只清 attached 槽位;turn 未结束 session 继续跑(没人看),settle 时无人 attach 才释放。settle 前重新 open = 带完整 replay 的重 attach(严格优于现在的"取消延迟关闭")。

**Delete**:若 attached(任何连接)先发 `Evicted`;走 `Releasing`(此处用 abort 而非 graceful,保持现有"删除不回写 checkpoint"语义)→ 移除 → server.rs 照旧删存储 + 重发 catalog。

**进程关停**:`main` 在 CancellationToken 触发后调 `shutdown_all()`(逐 entry 走 `Releasing`,`graceful_then_abort(5s)`),取代 `shutdown_active_sessions`。

## 并发要点

- 锁层级:entries map(std Mutex,不跨 await)→ entry.inner(tokio Mutex,可跨 await,**但 `shutdown().await` 绝不在 entry 锁内** —— 统一走 `Releasing` 机制:锁内转状态 + 取出 `Session`,锁外 await shutdown,完成后短暂取 map 锁移除、再置 `done = true`)。竞态 attach 者见 `Releasing` 在锁内拿 receiver 后锁外等 `done`、见 `Released` 直接重试,不忙等、无 missed wakeup。
- 一级泵不取锁、只做 recv+push,把 lag 概率压到最低(正确性兜底在 `Lagged` 分支);forwarder 从 unbounded mpsc 消费,每事件短暂取 entry 锁。attach / command / fold / evict / release 全被这把锁串行化 —— replay 恰好一次与 fold 原子性都由此而来。
- 泵读到流结束(runtime 终止):forwarder 处理完队列尾后,generation 匹配则发 `RelayEvent::Closed`,走 `Releasing` → 移除(runtime 已终止,shutdown 立即返回)。
- log 溢出策略:上限(如 8192)时优先丢 chunk 级事件(LlmStart/ContentChunk/ReasoningChunk/ToolCallStart),保留 message 级事件(LlmEnd/ToolCallEnd/Suspended/Aborted/Error)保证 fold 不丢历史;cursor 单调不复用;溢出 warn。
- **broadcast lag 不可自愈,必须防御**(settle 事件一丢,`turn_running` 永久卡死、entry 无法释放):
  1. 一级泵把接收路径压到最短,最大限度降低 lag 概率;
  2. 顺手把 `AgentRuntime` broadcast 容量从 128 提到 1024(crates/coda_agent/src/runtime.rs:211)降低洪峰压力;
  3. `Session` 暴露 lag 信号(现在 `recv()` 在 session.rs:467-479 吞掉 `Lagged` 只 warn):改为 recv 返回携带 `Lagged(n)` 变体的枚举(breaking change 可接受)。hub 一级泵收到 `Lagged` → entry 标 `desynced`,error 日志,并**阻止 fold**(日志不完整,内存快照不可信)。
  4. **desynced 的释放必须等 checkpoint barrier,barrier 要落成具体 API** —— 注意 settle 事件先于最终 checkpoint(见"已验证事实 1"),settle 一到就发 `Closed` 会让快速重连读到旧磁盘;而现有 `Shutdown::Graceful { on_timeout: Return }` 超时会返回 `false`(session.rs:483),此时 barrier 未达成,不能移除 entry。做法:coda_agent 新增 **`Shutdown::graceful_unbounded()`**(等待在跑的 turn 自然完成、最终 checkpoint 落盘后退出,无超时);hub 的 `Releasing`(normal release 与 desynced draining)用它,并且**仅在 `shutdown()` 返回 true 后**才发 `RelayEvent::Closed`、从 map 移除、置 `done = true`。normal release 时 runtime 空闲,立即返回;desynced draining 拒绝新命令后等 turn 跑完。delete(有意 abort、不回写)与进程关停(`graceful_then_abort(5s)`,进程都要退了,可接受)不受此约束。settle 事件是否被丢不再影响正确性。**不用静默超时判定**(长工具调用在 `execute(...).await` 期间无事件是正常的,driver.rs:887;慢 LLM 流同理,driver.rs:645),静默只用于 UI 警示。

## 修改文件

### 1. `crates/coda_agent` — 五处小改

- **driver.rs:用户消息立即落盘(修 bug)**。`AgentLoop::run` 中 `handle_envelope` 返回 `Next(rp)` 后(约 253-261 行),若 envelope 是 `Task`(根用户提示,排除子 agent 的 ToolCall envelope),立即 `save_checkpoint(rp.clone(), suspended_at)`(`ResumePoint: Clone` 已具备)。修复"用户输入要等 turn 结束才持久化、中途崩溃丢输入";同时中途重连的磁盘快照即含用户提示,事件层无需承载用户消息。副作用:catalog 的 `first_user_message` 在 turn 开始就可见。
- **driver.rs:abort 路径为已写入历史的消息补发事件**。generation abort 的 aborted assistant message(driver.rs:622、631)补发 `LLMEnd`,tool abort 的 aborted ToolMessages(driver.rs:956)与 stale-envelope 注入的 aborted ToolMessages(driver.rs:434-447)补发 `ToolCallEnd`;**顺序固定为先消息事件、后 `Aborted`**,`Aborted` 保持 stream 终结语义(agent.rs:222)并作为 abort 路径唯一的 settle marker(hub 侧 `event_settles_turn` 配合忽略 aborted `LlmEnd`)。保证"历史写入必有事件",hub 的 fold 才能与真实 history 一致。
- **runtime.rs:broadcast 容量 128 → 1024**(runtime.rs:211)。
- **session.rs:`recv()` 暴露 lag**(session.rs:467-479):不再吞掉 `Lagged` 只 warn,改为返回带 `Lagged(n)` 变体的枚举,由调用方(hub 一级泵)决定策略。breaking change 可接受。
- **session.rs:新增 `Shutdown::graceful_unbounded()`**:等 turn 自然完成、最终 checkpoint 落盘后退出,无超时不 abort —— hub 的 checkpoint barrier 依赖它(现有 `Graceful { on_timeout: Return }` 超时返回 false 无法保证 barrier,session.rs:483)。

### 2. `app/coda_server/src/wire.rs`

- `ServerMessage::Snapshot` 增加 `turn_running: bool`(serde default 兼容旧 JSON)。
- 新增 `ServerMessage::SessionEvicted { workspace_id, session_id }`。
- roundtrip 测试:snapshot 带/不带 `turn_running`、`session_evicted`。

### 3. `app/coda_server/src/hub.rs`(新)+ `lib.rs` 注册

`SessionRelay` trait、`SessionCommand`/`RelayEvent`/`AttachResult`、`SessionOpener` trait、`SessionHub` 实现及内部件(`SessionEntry`、私有 `EventLog`、forwarder、`fold_settled_turn`、`compose_attach`)。纯函数与内部件单独可测;单测用 mock `SessionOpener`。

### 4. `app/coda_server/src/bin/server.rs` — 切到中继层

- 删除:`ActiveSession`、`PendingOpen`、`OpenedSession`、`SessionEnvelope`、`spawn_session_forwarder`、`insert_active_session`、`close_session_now`、`handle_session_envelope`、`any_turn_running`、`shutdown_active_sessions`、`open_session_and_send_snapshot`、`make_pending_open` 及 `active`/`pending`/`close_requested`/`next_generation` 局部状态。
- `AppState` 增 `hub: Arc<dyn SessionRelay>`(实际是 `SessionHub`);现有 `open_session` 包装成 `SessionOpener` 实现在 `main` 注入。
- `run_dashboard` 变薄:conn_id(静态 AtomicU64)+ `StreamMap<SessionKey, BoxStream<RelayEvent>>`(attach 时插入,`Evicted`/`Closed`/detach 时移除)+ 本地 `attached: HashSet<SessionKey>`;断连直接 break(session 归中继管),结尾 `detach_all(conn_id)`。
- `handle_dashboard_command`:各消息映射为 `attach`/`command`/`detach`/`delete`;`Task` 的图片模态校验留在 server 层(向 hub 查 provider 或在 attach 时本地记录 provider_id,逻辑照搬 890-907 行)。
- **provider 归一化留在 server 层**:`OpenSession`/`SetModel` 继续经 `resolve_selection`(server.rs:403)/`normalize_provider_selection`(server.rs:424)校验后才调 hub —— hub 的 attach/SetModel 只接收已校验的 `{provider_id, reasoning_effort}`(`open_session` 内部的 `expect("caller passes a validated provider id")` 前提由此维持),无效选择在进 hub 前就被拒。

### 5. `app/coda_web`

- `src/lib/protocol.ts`:snapshot 加 `turn_running?: boolean`;新增 `session_evicted` 消息类型。
- `src/store/session.ts`:`SessionView` 加 `evicted: boolean`;`applySnapshot` 改动注意:现在 `running = false` 和 approvals 只在 `hasHistory || replaceEmpty` 分支里设置(session.ts:1137)—— 改为**无条件**更新 `running = turn_running`、`approvals`、`evicted = false`,entries 替换逻辑保留原分支;`onmessage` 处理 `session_evicted` → `evicted = true, running = false` + activity 提示;接管动作 = 重发 `open_session`;selector `activeSessionEvicted`。
- `src/components/composer.tsx` 等:`evicted` 时禁用输入,横幅"该 session 已被其他窗口打开 — 接管"。
- 重连路径不用改:`activeSessionToRestore` 已会重发 `open_session`,现在自动获得 snapshot + 中途 replay + live 流。

## 边界情况(设计已覆盖)

- **settle 瞬间重连**:fold+清 log+置 flag 在一个临界区;attach 排其前(turn_running=true + 完整日志)或其后(已 fold 快照 + 空日志),无重复无丢失,与磁盘写入时机无关。
- **驱逐发生在 Pending(等审批)时**:attached 槽位挂在 entry 上而非 state 变体上,天然覆盖;已收集 decisions 保留,新 client 只看到仍 `needed` 的审批。
- **abort 时的历史/事件一致性**:由 driver 补发事件的改动根治(所有写入历史的消息都有事件,fold 不再落后于真实 history);aborted `LlmEnd`/`ToolCallEnd` 只参与 fold,settle 由 `Aborted` 唯一承担。
- **fresh entry open 失败**:墓碑 + 从 map 移除,key 不卡死。

## 验证

1. `cargo clippy && cargo test`(workspace 根);`pnpm --filter coda-web lint`。
2. 单测(hub.rs):EventLog cursor 单调/截断/两级溢出;`fold_settled_turn` 各事件组合(含 Suspended、abort 补发的 LlmEnd/ToolCallEnd、**stale-envelope 顺序:log 开头的 ToolCallEnd 先于 unsettled_user 入 snapshot**、子 agent 跳过);`event_settles_turn` 对 aborted `LlmEnd` 返回 false(单 settle);attach 组装三态(运行中/空闲/Pending);latest-wins 驱逐 + stale conn 命令被拒;生命周期释放(`Releasing` 期间 attach 等 `done` 后重试成功、map 移除晚于 shutdown 完成、**missed-wakeup 竞态:释放在 attach 拿到 receiver 后立即完成,attach 仍能退出等待**);**洪峰测试**:突发 >128(broadcast 旧容量)个 chunk 事件后跟 settle,一级泵无丢失、fold 正确;**Lagged 路径**:注入 Lagged → desynced → draining → shutdown 完成后才 Closed;**send 失败**:mock opener 的 session send 报错时不留 phantom user、`turn_running` 不变。
3. coda_agent 测试(driver_tests.rs):send 后 turn 未结束时 `load_checkpoint` 已含用户消息;abort 各路径(generation/tool/stale-envelope)补发的事件与写入历史的消息一一对应;session.rs 的 recv 枚举变更编译通过全 workspace(breaking change 波及点排查)。
4. 手动双开验证(两个浏览器标签):
   - B 中途打开同一 session:A 显示驱逐横幅、只读;B 收到含提示词的 Snapshot + 流式 replay 无缝衔接 live;
   - 中途断 B 的 socket 再重连:无缝 replay;
   - 中途 CloseSession 再在 settle 前重开:replay 正常;放任 settle 且无人 attach:server 日志显示释放,再开从 checkpoint 加载。

## 实施顺序(小步可验证)

1. coda_agent 五处小改(driver.rs 提前 checkpoint、abort 路径补发事件、broadcast 容量、recv 暴露 Lagged、`Shutdown::graceful_unbounded()`)+ 测试 → `cargo test`(workspace,recv 是 breaking change 需全量编译)。
2. wire.rs + protocol.ts 类型 → `cargo test -p coda_server` + 前端 lint。
3. hub.rs:类型与内部件(EventLog、fold、compose)+ 单测,暂不接线;`app/coda_server/Cargo.toml` 加 `tokio-stream` 依赖(当前只有 `futures`,`StreamMap` 需要它;若想省依赖可用 `futures` 手写 keyed select,但 `tokio-stream` 更省事)。
4. hub.rs:`SessionHub` 完整实现(forwarder/attach/command/lifecycle)+ 单测;`event_settles_turn` 迁入。
5. server.rs 切换(AppState.hub、重写 run_dashboard/handle_dashboard_command、删旧机制、main 接 shutdown_all)→ `cargo clippy && cargo test`。
6. coda_web 行为(turn_running/evicted/横幅/接管)→ lint。
7. 全量检查 + 双标签手动验证。
