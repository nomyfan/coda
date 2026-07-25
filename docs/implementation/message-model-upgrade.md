## Problem

给持久化的 `Message` 建立稳定身份（`message_id`）、turn 归属（`turn_id`）和跨线程调用追溯（`origin`），并把线程之间的父子关系显式记下来，为 rewind / fork 提供精确定位、因果截断和线程树重建的数据基础。

需求：`../requirement/message-model-upgrade.md`

## Scope

**In:**
- `UserMessage` / `AssistantMessage` / `ToolMessage` 增加 `message_id`
- `UserMessage` 增加跨线程来源复合键 `origin: Option<MessageOrigin { message_id, call_id }>`（sub-agent 开场消息指向父 Assistant 及其中的 ToolCall）
- `turn_id`：每次 root 用户提交定义一个 turn，该 turn 在**所有**线程中引发的每条消息都标注同一个 `turn_id`
- 每类消息的 ID 铸造点与传递路径，以及 `turn_id` 的跨线程传播与跨挂起恢复
- 线程拓扑：`StoredCheckpoint` 记下父线程 id 与当初算自己 id 时喂给 uuid5 的那个字符串（`derivation_key`），`ToolCall` envelope 补带 `derivation_key`。放在本设计里是因为它和上面几项是同一趟 `coda_agent` 改动（SQL 列见 `storage-migration-pg`）
- **修正线程 ID 派生**（两处现存缺陷，详见 Validation Findings）：stateless 线程改用复合键 `(父 Assistant message_id, call_id)` 派生，不再只用 `call_id`；`ThreadId::from_uuid5` 对非 UUID 父 ID 改为稳定哈希出 namespace，不再退化成 nil
- `task` 由 notification 改为 request，回传服务端铸造的 `message_id`
- Wire / 前端携带并使用 `message_id`；前端乐观条目 reconcile 到服务端 id

**Out:**
- 不改存储方式（见 `storage-migration-pg`）
- `turn_id` 不进核心 `Message` 类型（理由见 Alternatives），也**不进 wire**——前端的 turn 分组仍由 root 线程的 user 消息序列推导，够用；等 rewind 设计时再评估是否要暴露
- `thread_id` / `seq` 不进核心 `Message` 类型——它们是存储层的列（见 `storage-migration-pg`）
- 不给 `SystemMessage` 加 ID / `turn_id`（每轮临时构造、不持久化、加载时被 `restore_history` 过滤）

## Validation Findings

- **stateless 线程 ID 会跨 turn 撞车（现存缺陷，本设计必须一并修）。** stateless 线程 id = `uuid5(父线程, tool_call.id)`（`driver.rs:862`），而本设计明确**不假设** `tool_call.id` 跨 Assistant 消息唯一（见 Assumptions）——有些 provider 就是按 `call_1`/`call_2` 逐响应编号。撞车后果比"拓扑坍缩"更重：`SessionStorage` 只有 save/load，**没有删除单线程 checkpoint 的方法**（`runtime.rs:64-80`），stateless 线程的历史永久留存，于是第二次调用会**加载并继承第一次的完整对话**——运行时上下文直接串。设计实现：改用复合键 `(父 Assistant message_id, call_id)`，即已有的 `MessageOrigin`；同一个键在消息层标注"哪次调用触发"、在线程层派生 thread id，两处同源，不必另造 invocation id。
- **非 UUID 的 session id 会让派生退化成 nil namespace（现存缺陷）。** `from_uuid5` 在父 id 解析失败时 `unwrap_or(Uuid::nil())`（`agent.rs:133`）。而 root 线程 `thread_id == session_id`，session id 是客户端给的任意字符串。前端 `freshSessionId()` 是 `crypto.randomUUID?.() ?? \`session-${Date.now().toString(36)}\``（`session.ts:161`），而 `crypto.randomUUID` **要求安全上下文**——内网 HTTP 访问时它是 undefined，于是走 fallback、session id 不是 UUID。所以 nil 分支是常见路径，不是边角：`driver.rs:860` 那句"parent thread_id 必是合法 UUID，永不退化到 nil"的注释是错的，且"fork 换 session_id 会带动整棵子线程 id 改变"的推论对这类会话不成立。设计实现：非 UUID 父 id 用固定命名空间常量稳定哈希出 namespace，退化分支彻底消失，session id 的外部契约不动。
- **另有一个不属于本设计的相邻缺陷**（记录备查）：上面那个 fallback 只有毫秒时间戳、无随机分量，同一毫秒内新建两个会话会撞 session id。属 `coda_web`，建议改用 `crypto.getRandomValues` 手搓 v4（非安全上下文也可用），与本设计各自独立。
- **envelope 严格串行，且每个 envelope 开头都从存储重载历史。** 每个 agent 一条顺序循环，一次只取一个 envelope 处理到底（`driver.rs:94-119`）；`AgentLoop::run` 开头必定 `load_checkpoint` + `restore_history`，连无 checkpoint 的情况也显式清空，注释点明"Agent 实例会被不同 thread id 复用，必须清掉残留以免跨线程泄漏对话"（`driver.rs:242-261`）。两个推论：(1) 线程级单值"当前 turn"够用，不会出现两个 turn 的消息交错追加；(2) "当前 turn 由历史末条 entry 反推"是**常规路径**而非仅重启路径，因此确实不需要额外持久化它。另外这也说明**两个 envelope 之间内存里的 `AgentState.messages` 不是权威**——下一个 envelope 会重新从存储读，所以 rewind（本就禁止运行中执行）截断存储 + hub snapshot 即可，不需要一个"跨线程施加内存截断"的入口。

## Assumptions

- **消息 append-only**：线程存活期内 `AgentState.messages` 只 `push`，无就地修改（已核验：唯一的 `messages.insert` 在 `agent.rs:469`，是给 LLM 请求另出一份带 System 头的副本，不动存储态）。这让"同一 message_id 恒等于同一条消息"、以及"`turn_id` 盖上即不变"都成立。
- **单条 Assistant 消息内的 `tool_call.id` 互不相同**（同一消息里两个同 id 的 tool call 属畸形响应）。跨消息的 `tool_call.id` 是否复用**不作假设**——来源用 `(父 Assistant message_id, tool_call.id)` 复合键，即使 provider 跨轮复用 id 也唯一。这条假设同时管着**线程 ID 派生**：stateless 线程也必须用同一复合键，只用 `call_id` 会跨 turn 撞车（见 Validation Findings）。
- 一个 `Task`/`ToolCall` envelope 恰好产生一条 user 消息（1:1），因此 user 消息可在 envelope 入口铸造 ID。
- **消息追加只有一个入口**（已核验：`Agent::add_message` / `add_messages`，`agent.rs:453/458`；driver 内 8 处追加全走它，无旁路）。这让 `turn_id` 可以在这一处统一盖章，所有构造点零改动。
- **turn 与 root user 消息 1:1 且按序结算**（已核验：`fold_settled_turn` 每次结算恰好 pop 一条 `unsettled_user_messages`，`hub.rs:346`）。因此"turn 的身份 = 发起它的 root user 消息"是良定义的。
- **同一线程的 envelope 串行处理**（已核验，见 Validation Findings）：一个 envelope 处理完才取下一个，故线程级"当前 turn"单值足够，不会出现两个 turn 的消息交错追加。

## Alternatives Considered

**身份放哪里 —— 内嵌字段 vs 外层包裹。**
- 选择：把 `message_id` 直接作为三个消息结构体的字段。
- 放弃：`Vec<StoredMessage{ id, msg }>` 外层包裹。理由：`AssistantMessage` / `ToolMessage` 以裸结构体流经 `AgentEvent::LLMEnd` / `ToolCallEnd` → hub 折叠进 snapshot。字段内嵌让 ID 免费随现有事件管道流动；外层包裹要求事件枚举也改带包裹类型，plumbing 更多、更浅。

**跨线程来源怎么记 —— 复合键 vs 单 `call_id` vs 独立 `invocation_id`。**
- 选择：`origin = (父 Assistant message_id, tool_call.id)` 复合键。
- 放弃单 `origin_call_id`：依赖 `tool_call.id` 全会话唯一，而该假设不保；PG 一旦只存了 `call_id`，碰撞时因果关系永久丢失、不可回溯修复——必须现在就采集足够信息。
- 放弃服务端新造 `invocation_id`：复合键复用已有标识（父 Assistant 的 `message_id` 在会话内唯一 + 其内部 `tool_call.id` 单消息内唯一），无需再造一个 id 并全链路穿透。来源指向的父线程恒在同一会话内，会话内唯一就够。

**是否引入 `turn_id`（原先列为 Out，现改为 In）。**
- 选择：引入。理由不是"查询方便"，而是它**替 rewind / fork 承担了截断这件事**：一次 root 提交会在多个线程留下消息，只靠 `origin` 判断"哪些 sub-agent 消息属于被丢弃的调用"需要沿 `origin` 边**递归**上溯到 root 线程（PG 里是 recursive CTE，内存里是图遍历）；有了 `turn_id`，截断退化成"丢弃 `turn_id ∈ 待丢弃 turn 集合`的所有消息"——同一条谓词在内存 `AgentState` 和 DB 里都成立，两处逻辑一致。
- 放弃"由 root user 消息序列推导"：turn **边界**确实可推导，但 turn **归属**（尤其是跨线程、跨深度的 sub-agent 消息）不可廉价推导，正是递归上溯那笔成本。
- `turn_id` 不使 `origin` 冗余，二者分工不同：`turn_id` 粗粒度、跨线程分组（截断 / 按 turn 聚合）；`origin` 细粒度、单次调用归属（同一 turn 内两次调用同一个 stateful sub-agent 时，只有 `origin` 能区分是哪一次——UI 嵌套与逐调用归因要它）。

**`turn_id` 用什么值 —— 发起该 turn 的 root user `message_id` vs 会话内自增序号 vs 另铸 uuid。**
- 选择：`turn_id = 发起该 turn 的 root user 消息的 message_id`（`TurnId` 是 `MessageId` 的 newtype）。不新增 ID 空间、不新增铸造点，且"turn 是什么"由类型自解释；顺序沿用 root 线程既有消息顺序，不引入第二套排序真相。
- 放弃会话内自增序号：`WHERE turn_seq >= N` 最省事，但要维护一个跨重启、跨 rewind（rewind 后序号需回退复用）的计数器，多一份可变状态与不一致来源。
- 放弃另铸 uuid：与复用 root user `message_id` 等价，却多一个要全链路穿透的 id。
- 代价：取"某 turn 及其之后"不再是范围比较，得先在 root 线程按 seq 定位再取 turn 集合（一层子查询）。可接受——rewind 本来就要先按 `message_id` 定位目标消息。

**`turn_id` 放哪 —— 状态/存储层包裹 vs 核心 `Message` 字段 vs 只存线程级当前 turn。**
- 选择：`AgentState.messages: Vec<HistoryEntry { turn_id, message }>`，`turn_id` 不进 `Message`。归类同 `thread_id`/`seq`：它描述"这条消息在会话控制流中的位置"，是运行态/存储态元数据，不是消息内容。
- 放弃核心 `Message` 字段：provider adapter 构造 `AssistantMessage` 时根本不知道 turn，必填字段会逼出占位值、`Option` 逼出"实际不可达的 None"——正是 round-2 对 `message_id` 指出的那个坑。（注：`message_id` 之所以能内嵌，是因为它需要随 `LLMEnd`/`ToolCallEnd` 事件流到 hub；`turn_id` 不需要——hub snapshot 不用它，rewind 后 hub 从 DB 重新同步。）
- 放弃"只在 `thread_checkpoints` 存线程级 `current_turn_id`、由存储层给本次新增行盖章"：省一个类型，但依赖"一次 save 的新增消息必属同一 turn"这个无法廉价证明的不变量，一旦不成立数据静默错标；且内存态没有逐条归属，rewind 无法用同一条谓词截断存活线程的 `AgentState`。

**线程父子关系记不记 —— 记(父 + derivation_key) vs 只记父 vs 不记、靠重新推导。**
- 选择：记**一对**——父线程 id + 当初算这个线程 id 时喂给 uuid5 的字符串。
- 放弃"只记 `parent_thread_id`"：不够。重算子线程 id 要 `uuid5(新父 id, name)`，而 `name` 对 stateful 是 `agent_name`、对 stateless 是复合键 `(父 Assistant message_id, call_id)`；已有的 `agent_name` 列表示"属于哪个 agent"，对 stateless 线程并不是算 id 用的那个串。只记父等于留个半成品。
- 放弃"不记、日后重新推导"：拓扑确实推得出来（stateful 可从 root id + `.coda/agents/` 自顶向下算，stateless 的复合键两个分量都躺在父线程的 assistant 消息里），信息不会丢。但推导依赖 agent 配置没变过，且要写一套扫消息的逻辑；而记下来是零成本——driver 派发时两个值都在手里。
- 关键理由是**这对字段不替 fork 做任何决定**：fork 无论选"重算整棵 id 树"还是"照搬 id、把 root 与 session_id 解耦"，都用得上；反之若不记，两条路都得先补一套推导。
- 不记 `root_thread_id`：它等于 `session_id`（`session.rs:336/427`），且"父为空即 root"已经够用。

**服务端铸造的 root user id 怎么到前端 —— request/ack vs 客户端生成+校验。**
- 选择：`task` 从 notification 改为 **request，返回 `{ message_id }`**。前端先用临时 id 乐观渲染，收到 ack 后 reconcile 成服务端 id。保持"ID 只由服务端产生"不变量。
- 放弃客户端生成 + 服务端校验：前端虽天然一致，但 ID 源头落在不可信边界，需在 ingest 处校验格式与会话内唯一；与既定的"服务端铸造"决策相悖。

**Assistant/Tool ID 在哪铸造 —— provider adapter 直接铸造 vs 引入 Draft 类型。**
- 选择：`message_id` 作必填字段，正常 Assistant 在 `coda_openai` 的 `TryFrom` 构造处 `MessageId::new()`，driver 不覆盖；aborted Assistant 与 `ToolMessage` 各在其构造处铸造。
- 放弃 `AssistantMessageDraft`（provider 产出无 id 草稿、Runtime 转成带 id 的正式类型）：类型不变量更硬，但要在 `coda_core` 新增一个与 `AssistantMessage` 几乎重复的类型并改动公共 `LLMStreamEvent::Completed` 载荷，复杂度不划算。
- 安全性论证：assistant/tool 每个对象只在一处构造、构造即定 id、经事件原样流动，不存在 user 消息那种"两处独立构造"的分歧风险，故本地 adapter 铸造本地 UUID 是安全的；无需坚持"Runtime 独占铸造"。timing 字段本就是 adapter 先填、runtime 覆盖（`lib.rs:782-785`），此处沿用同一分工。

**user 消息 ID 在哪铸造。** root user 消息 ID 在**任务入口（hub `handle_task`）一次铸造**，经 `Session::send` → `Envelope::Task` 传给 driver；hub 的 snapshot 副本、driver 持久化副本、以及 ack 回给前端的值，三者同一个 ID。理由：user 消息在两个独立位置各构造一次（hub 内存 snapshot + driver 持久化历史，事件流不携带 user 消息），若各自 `MessageId::new()` 会得到不同 ID，重连/resync 时同一条消息前后 ID 不一致，破坏 rewind 定位与前端 key。

**是否复用 envelope id 作为 message_id。** 放弃：envelope 是传输层、`reply_to` 引用的是 envelope id，把消息身份绑到传输语义是泄漏。二者独立。

## Components

- **`MessageId`**（`coda_core`）—— UUID 的 newtype，序列化为连字符字符串；`new()` 生成 v4。生成的值天然不会撞，但**约束只要求会话内唯一**（见 `storage-migration-pg`：这样 fork 才能整片复制而不必重铸 id）。
- **`TurnId`**（`coda_core`）—— `MessageId` 的 newtype：发起该 turn 的 root user 消息的 id。与 `MessageId` 类型隔离，避免与"任意某条消息的 id"混用。
- **消息身份字段**（`coda_core::llm`）—— 三个持久化变体的 `message_id`，`UserMessage` 的 `origin`。
- **`HistoryEntry`**（`coda_agent`）—— 线程历史的存储单元：`{ turn_id, message }`。`AgentState.messages` 与 `StoredCheckpoint.messages` 都用它；给 LLM 组请求时剥掉 `turn_id`。
- **turn 盖章 + 传播**（`coda_agent`：`AgentState` + `Agent::add_message` + driver + `Envelope`）—— 线程级"当前 turn"存在 `AgentState`（与 `messages` 同锁），在追加本 envelope 的 user 消息时推进（推进时机见下方盖章规则），`add_message` 统一盖章；`ToolCall` envelope 把 turn 带进 sub-agent 线程；线程恢复时由历史末条 entry 反推。
- **user 消息铸造 + ack**（`coda_server`：rpc handler + hub + `Session::send` + `Envelope::Task`）—— `task` 变 request，服务端在入口铸造 root user 消息 ID，双路分发（driver / snapshot）并 ack 回前端。
- **origin 标注 + 父 ID 传播**（`coda_agent` driver + persist）—— 派发批次的父 Assistant `message_id` 随该批次的 resume 状态一路持久化（正常执行 / 审批恢复 / 进程重启同一条链路），派发时组装 `MessageOrigin` 放进 sub-agent 的 `ToolCall` envelope；sub-agent 侧 `handle_envelope` 的 `ToolCall` 分支据此填开场 user 消息的 `origin` 并铸造其 `message_id`。
- **线程拓扑记录**（`coda_agent` driver + persist）—— 子线程处理 `ToolCall` 时，父线程 id 从 envelope 的 `from: Sender::Agent { thread_id }` 取、算 id 用的串从新增的 `derivation_key` 字段取，两者一并写进自己的 checkpoint 长期保存（`reply_target` 回信后即清，靠不住）。
- **assistant/tool 铸造点** —— 正常 Assistant 在 provider adapter 的 `TryFrom` 构造处铸造；aborted Assistant 与 `ToolMessage` 各在其构造处铸造（均单构造点）。

## Interfaces

```rust
// coda_core: 身份类型，服务端铸造，客户端只读。
pub struct MessageId(Uuid);
impl MessageId { pub fn new() -> Self; }
pub struct MessageOrigin { pub message_id: MessageId, pub call_id: String }

// turn 的身份就是发起它的 root user 消息的 id —— 不另铸，也不另排序。
pub struct TurnId(MessageId);
impl From<MessageId> for TurnId { .. }

// coda_agent: 线程历史的单元。turn_id 在此层，不在 Message 里。
pub struct HistoryEntry { pub turn_id: TurnId, pub message: Message }

// UserMessage 构造需显式传入 ID（不再内部生成），因为 root user 消息由边界铸造、双路共享。
UserMessage::text(id: MessageId, text) -> UserMessage
UserMessage::with_images(id: MessageId, text, images) -> UserMessage
UserMessage::from_subagent_call(id: MessageId, text, origin: MessageOrigin) -> UserMessage

// Assistant / Tool 的 message_id 为必填字段，在各自唯一构造点铸造（每个对象只构造一次
// → 只有一个 id，经事件流动到 hub 不分歧）：
//   - 正常响应：provider adapter 的 `TryFrom<CompletionAccumulator>`（coda_openai/lib.rs:774）
//     构造时 `message_id: MessageId::new()`；driver 只覆盖 timing（同今天），不覆盖 id。
//   - aborted 响应：driver 在 driver.rs:668-678 直接构造，同处铸造 message_id。
//   - ToolMessage::new 已在内部盖 ended_at，同处铸造 message_id，调用点不变。
// 本地 adapter 铸造本地身份 UUID 是安全的：单构造点，不存在两个 id 指向同一对象。

// 跨线程来源 + turn 传播 + 线程拓扑：sub-agent 的 ToolCall envelope 携带完整 MessageOrigin、
// 父线程的 turn，以及父线程算这个子线程 id 时用的那个字符串（stateful 是 agent_name，
// stateless 是 MessageOrigin 的复合键序列化形式，见 driver.rs:858-866 与 Validation
// Findings）。父线程 id 本来就在 envelope 的 `from: Sender::Agent { thread_id }` 里，
// 不用另加字段。
// 派发批次的父 Assistant message_id 随 resume point 持久化（见 Finding 2 链路）。
EnvelopeBody::ToolCall {
    call_id: String, origin: MessageOrigin, turn_id: TurnId,
    derivation_key: String, task: String,
}

// 子线程在处理 ToolCall 时把这两个值记进自己的 checkpoint，长期保存（区别于
// reply_target：那个回信后就被 take 掉了，只活一次调用）。
StoredCheckpoint { .., parent_thread_id: Option<String>, derivation_key: Option<String> }
// root 线程两者都是 None —— “parent 为空”就是 root 的判据，不需要另存 root_thread_id
// （它等于 session_id，见 session.rs:336/427）。

// 任务入口：task 变 request；hub 铸造 ID，双路分发 + ack。
// Task envelope 不需要单独的 turn_id —— 它铸造的 message_id 本身就是这个新 turn 的身份。
Session::send(&self, id: MessageId, task: String, images: Vec<String>) -> Result<...>
EnvelopeBody::Task { message_id: MessageId, task: String, images: Vec<String> }
// wire: task 请求返回 { message_id }

// turn 盖章。分两个入口，把"turn 只在追加 user 消息时推进"变成接口层面的强制，
// 而不是靠调用方按正确顺序调两个方法（那样就丢了同一临界区的原子性）：
//
//   追加 user 消息 + 推进本线程当前 turn，一次加锁完成。root 用 Task envelope 铸的
//   message_id 作 turn；sub-agent 用 ToolCall envelope 带来的父线程 turn（它不开启新
//   turn，只是让本线程的当前 turn 变成那个）——所以叫 add_user_message 而非 start_turn。
Agent::add_user_message(&self, turn_id: TurnId, message: UserMessage)
//   assistant / tool 照旧，从线程当前 turn 盖章，调用点零改动。
Agent::add_message(&self, message: Message)          // 签名不变，内部包成 HistoryEntry
Agent::restore_history(&self, entries: Vec<HistoryEntry>, todos: Vec<TodoItem>)

// 线程 ID 派生（两处修正，见 Validation Findings）：
//   - stateless：派生键从 `call_id` 改为复合键 `(父 Assistant message_id, call_id)`，
//     即已有的 MessageOrigin 序列化形式；跨 turn 复用同一 call_id 也不会撞。
//   - stateful：仍是 agent_name（同名同父故意收敛到一条线程，这是 stateful 的语义）。
//   - 非 UUID 的父 thread_id 不再退化成 Uuid::nil()，而是用固定命名空间常量稳定哈希出
//     namespace —— 退化分支消失，"换 root 必然换整棵子线程 id"这条推论才无条件成立。
ThreadId::from_uuid5(namespace: &ThreadId, name: &str) -> ThreadId
```

**turn 盖章规则（对 rewind 正确性 load-bearing）。** 线程级"当前 turn"的推进时机不是 envelope 入口，而是**该 envelope 自己的 user 消息被追加的那一刻**——由 `add_user_message` 一次加锁完成，所以"turn 已推进但消息还没写"这个中间态在接口层面就不存在。在此之前写入的消息一律沿用上一个 turn。这条顺序不是风格问题：新 task 抢占待审批调用时，driver 会先把被丢弃的调用写成 aborted `ToolMessage`（`fold_settled_turn` 里它们排在新 user 消息**之前**，`hub_tests.rs:132`）。这些 tool 结果在语义上属于**上一个 turn**——若按 envelope 入口盖章成新 turn，则 rewind 到新 turn 会把它们一起删掉，留下父 Assistant 的 `tool_call` 没有对应结果，送给 provider 的历史直接畸形。按上述规则，它们自然继承旧 turn，rewind 时被保留。

线程恢复（冷开 / 审批 resume / 进程重启）时"当前 turn"= **历史末条 `HistoryEntry` 的 `turn_id`**：待恢复的工作必然属于最后一条消息所在的 turn。因此不需要在 `thread_checkpoints` 额外持久化线程级当前 turn，也不需要给待派发批次加 turn 字段——少一个字段、少一处可能不一致。历史为空时无当前 turn，而此时也必然还没有任何消息要盖章。

**Finding 2 链路 —— 父 Assistant ID 跨挂起恢复。** 一批 tool call 恰好来自一条父 Assistant 消息（一次 generation 产出一批），故在**批次粒度**持久化一个 `parent_message_id`，而非逐 call 冗余：
- 运行态 `ToolExecutionState` 与 `ResumePoint::PendingApproval` 各加 `parent_message_id: MessageId`；
- 存储态 `StoredToolExecutionState` 与 `StoredResumePoint::PendingApproval` 同步加该字段（`persist.rs`）；
- sub-agent `ToolCall` envelope 携带完整 `MessageOrigin`。
这样正常执行、审批 resume、进程重启三条路径都从同一 `parent_message_id` 组装 `MessageOrigin`，不再依赖"当前作用域恰好还持有父消息"。

**Trust boundary —— `hub::handle_task`（task request handler）。** 客户端 `task` 请求在此进入。`message_id` **一律服务端铸造**，绝不接受客户端提供的值，杜绝客户端制造 ID 碰撞或伪造 rewind 目标；铸造后经 ack 回传前端。下游（driver、持久化、wire）可直接信任该 ID 在会话内唯一且服务端可控。

## Data Model

- 三个持久化消息变体各带一个 `message_id`，在**会话内**唯一（约束范围见 `storage-migration-pg`）。`SystemMessage` 无 ID。
- `origin` 建立唯一的跨线程边：sub-agent 线程的**开场 user 消息** → `(父 Assistant message_id, 触发它的 tool_call.id)`。root user 消息为 `None`。
- stateful sub-agent 的同一线程内每次调用各追加一条开场 user 消息，各带自己的 `origin` —— 这正是未来按调用精确截断所需的粒度。
- 线程内因果是线性的（Vec 顺序即因果），无需额外 parent 指针；**消息层面**的跨线程关系只需 `origin` 一条边（线程层面的父子关系是另一件事，见下方"线程拓扑"）。
- `turn_id` 是**会话范围的横切分组键**：一次 root 提交在 root 线程、各 stateful/stateless sub-agent 线程、任意深度留下的所有消息共享同一个 `turn_id`。它与 `origin` 分工正交——`turn_id` 回答"这条消息属于哪次用户提交"（截断、按 turn 聚合），`origin` 回答"这条消息由哪一次具体调用触发"（同一 turn 内重复调用同一 stateful sub-agent 时的区分、UI 嵌套）。
- `turn_id` 在**运行态与存储态**都逐条存在（`HistoryEntry` / `messages.turn_id` 列），不在 `Message` 内容里、不在事件流里、不在 wire 上。这让 rewind 能用同一条谓词同时截断存活线程的内存历史和 DB 行。
- **线程拓扑**：每个子线程记下父线程 id 和算自己 id 时用的那个串（`derivation_key`：stateful 是 agent 名，stateless 是 `(父 Assistant message_id, call_id)` 复合键），父为空即 root。原先这层关系只藏在 `uuid5(父, 串)` 的单向推导里，现在可直接查、可自顶向下重建整棵树。它与消息层的 `origin` 不重复：`origin` 是"哪条消息触发了哪条消息"，这里是"哪个线程从属于哪个线程"。
- **共享可变状态**：`AgentState.messages` 与线程当前 turn 同在一把 `state` 锁下（`Agent::add_message` 已持锁），盖章与追加在同一临界区内完成，不存在"turn 已推进但消息还没写"的中间态被观察到。

## Load-Bearing Decisions

- **改核心 `Message` 序列化格式**（新增字段）—— breaking，可接受（无兼容层）。影响 `coda_core` 及所有依赖 crate 编译。
- **`task` 从 notification 改 request** —— wire 协议形状变化；前端需引入乐观临时 id → 服务端 id 的 reconcile。
- **每条消息单一铸造点** —— root user 在 hub 边界（服务端铸造，客户端只读）；sub-agent user 在 driver；正常 assistant 在 provider adapter 的 `TryFrom`，aborted assistant 与 tool 在各自构造处。单构造点保证一对象一 id。
- **来源用复合键 `(父 message_id, call_id)`** —— 不依赖 `tool_call.id` 全局唯一；采集时机不可后置（丢了不可补）。
- **`turn_id` 逐条持久化，且值取发起 turn 的 root user `message_id`** —— 换来 rewind/fork 的截断在内存和 DB 都是一条谓词，代价是接受一处受控冗余（归属可由 `origin` 递归推导，但太贵）。同样属于"采集时机不可后置"：历史一旦落库没带 turn，事后只能靠递归上溯猜。
- **`turn_id` 落在 `HistoryEntry` 而非 `Message`** —— `AgentState.messages` / `StoredCheckpoint.messages` / `restore_history` 的类型随之变（波及 driver、persist、PG 存储、`MemoryStorage`、测试 stub），换来 provider adapter 与所有消息构造点完全不感知 turn。
- **当前 turn 不额外持久化，由历史末条 entry 反推** —— 少一个字段和一处不一致来源；代价是把"待恢复的工作必属最后一条消息所在 turn"这条不变量吃进设计。
- **线程拓扑显式落库（父线程 id + `derivation_key`）** —— 从"靠 uuid5 单向推导"变成"可直接查"，为 fork 重建线程树留好料；代价是每个子线程的 checkpoint 多两个字段、`ToolCall` envelope 多一个字段。不记 `root_thread_id`（等于 `session_id`）。
- **改线程 ID 派生规则** —— stateless 用复合键、非 UUID 父 id 稳定哈希。既有会话的 stateless 线程 id 会变（breaking，可接受：库要重建）。修的是现存缺陷，且必须在拓扑落库之前修——否则等于把错的派生键持久化下来。
- **`add_user_message` 与 `add_message` 分两个入口** —— 用接口形状保证"turn 只在追加 user 消息时推进"这条规则，而不是靠调用顺序约定。代价是 driver 里追加 user 消息的几处（Task / ToolCall / Resume 分支）要换方法。
- **父 Assistant `message_id` 随 resume 状态持久化** —— 运行态 + 存储态（`ToolExecutionState`/`PendingApproval` 及其 Stored 形）都带 `parent_message_id`，让审批恢复 / 进程重启后仍能拼出 `MessageOrigin`。这是新增的持久化字段，属 breaking schema 变更（可接受）。

## Risks / Open Questions

- **最大风险：root user 消息双路 ID 一致性。** 缓解 = 边界单点铸造 + envelope 透传 + ack。首个要验证：一条 task 走完 `handle_task` → snapshot 副本、driver 持久化副本、ack 返回值三者 `message_id` 相等。
- **父 Assistant message_id 跨挂起恢复（已定方案，待落地验证）。** 一批 tool call 是否恒等于一条父 Assistant——需在实现时确认"一次 generation 一批"不变量成立（否则批次粒度的单 `parent_message_id` 不够，得退回逐 call）。审批挂起→进程退出→恢复后再派发 sub-agent 时，`MessageOrigin` 必须能从持久化的 `parent_message_id` 拼出。
- **构造点排查完整性。** `UserMessage::text/with_images` 调用点分散（driver 多处、hub、测试），签名改动需逐一过；`ToolMessage::new`、正常/aborted assistant 构造处内部铸造 id 则调用点零改动。
- ~~同一线程 envelope 是否串行处理~~ **已核验成立**（`driver.rs:94-119`），见 Validation Findings。原先设想的退路（让待派发批次自带 `turn_id`）不需要了。
- **aborted `ToolMessage` 的 turn 归属。** 上面那条盖章规则要成立，得确认 driver 确实在追加新 user 消息**之前**写完抢占产生的 aborted `ToolMessage`。验证方式很直接：抢占后按 turn 截断，父 Assistant 的每个 `tool_call` 仍有配对的 `ToolMessage`。

## Implementation Roadmap

- [x] [risk validation] 打通 root user 消息三路 ID 一致
      - `task` 改 request；`hub::handle_task` 铸造 `MessageId`，经 `Session::send` → `EnvelopeBody::Task` 传到 driver；hub snapshot 副本用同一 ID；ack 回前端
      - Purpose: 先证伪最大风险——两条构造路径 + ack 产出同一 ID
      - Verification: 集成测试发一条 task，断言 snapshot user `message_id` == 持久化 checkpoint == ack 返回值
      - 落地：ack 经新增的 `CommandOutcome::TaskAccepted { message_id }` 从 hub 传回 `dispatch_request`，答以 `TaskAccepted { message_id }`。三方断言在 `hub_tests::snapshot_and_checkpoint_agree_on_every_message_id` 里；ack 那一方单独反向验证过（只让 hub 返回另铸的 id，断言即失败）。**最大风险已排除**
- [x] [core logic] 铸造点：正常 assistant 在 `coda_openai` 的 `TryFrom` 铸造 id；aborted assistant（driver 668-678）与 `ToolMessage::new` 各自铸造
      - Purpose: 补齐 assistant/tool 单构造点铸造
      - Verification: 单测——同一条 assistant/tool 消息经 `LLMEnd`/`ToolCallEnd` 事件到 hub snapshot 后 `message_id` 不变
      - 落地：`hub_tests::snapshot_and_checkpoint_agree_on_every_message_id`。断言一整轮（user → assistant(带 tool call) → tool → assistant）在 snapshot 与持久化 checkpoint 中的 id 序列逐条相等，一次覆盖三种变体的两条不同路径。已反向验证：只让事件副本与历史副本的 assistant id 分岔，该测试即失败（user/tool 仍相等），证明它确实盯着事件流那条路
- [x] [core logic] 父 ID 传播链路：`ToolExecutionState`/`PendingApproval` 及 Stored 形加 `parent_message_id`；sub-agent `ToolCall` envelope 带 `MessageOrigin`；sub-agent `handle_envelope` 据此填 `origin` 并铸造开场 user `message_id`
      - Purpose: 打通正常/审批恢复/重启三路径的 origin
      - Verification: 单测——stateful sub-agent 多次调用后每条开场 user 消息 `origin` == 对应父 `(message_id, tool_call.id)`；**审批挂起→重开会话→approve 派发**后 origin 仍正确
      - 落地：`driver_tests::stateful_subagent_records_which_call_opened_each_invocation`（同一 turn 内连调两次 stateful sub-agent，脚本**故意复用同一个 `call_id`**，所以只有父 message_id 能区分两次调用）与 `subagent_dispatched_after_approval_restart_still_records_its_origin`（挂起→shutdown→restart→approve）。两条都反向验证过：分别把 origin 的父 id 改成新铸的、把 `StoredResumePoint::PendingApproval` 落库的父 id 改成新铸的，对应测试各自失败
- [x] [core logic] 修线程 ID 派生（**必须排在拓扑落库之前**，否则等于把错的派生键持久化）：stateless 改用复合键 `(父 Assistant message_id, call_id)`；`from_uuid5` 对非 UUID 父 id 稳定哈希出 namespace，去掉 nil 退化分支；同时修掉 `driver.rs:860` 那句已经不成立的注释
      - Purpose: 消掉两个现存缺陷——stateless 线程跨 turn 继承他人历史、非 UUID 会话共用 namespace
      - Verification: 单测——(a) **同一父线程在两个不同 turn 收到相同 `tool_call.id`，两次 stateless 调用得到不同 thread id、第二次历史为空**；(b) 两个不同的非 UUID session id 派生出的同名子线程 id 互不相同；(c) 父 id 是合法 UUID 时的派生结果与改动前一致（stateful 路径不回归）
      - 落地：(a) `driver_tests::stateless_invocations_reusing_a_call_id_get_separate_threads`（复用同一 `call_id` 的两次 stateless 调用，断言 thread id 不同且各线程恰有一条开场消息）；(b)(c) `agent::thread_id_tests` 两条。复合键由 `MessageOrigin::derivation_key()` 生成。**两个缺陷都反向复现过**：退回裸 `call_id` → (a) 失败；退回 `unwrap_or(Uuid::nil())` → (b) 失败且两个不同非 UUID 会话派生出**完全相同**的子线程 id，(c) 仍通过（证明 UUID 路径无回归）
- [x] [core logic] 线程拓扑落库：`ToolCall` envelope 加 `derivation_key`；`StoredCheckpoint` 加 `parent_thread_id` / `derivation_key`，子线程处理 `ToolCall` 时从 envelope sender + 该字段捕获并写入
      - Purpose: 把只藏在 uuid5 推导里的父子关系变成可直接查的记录，给 fork 备料
      - Verification: 单测——含嵌套 sub-agent 的会话跑完后，能只靠 checkpoint 自顶向下重建整棵线程树（父为空的恰好一个且等于 session_id）；对每个子线程校验 `uuid5(parent_thread_id, derivation_key)` == 它自己的 `thread_id`；stateless 记的是复合键、stateful 记的是 agent 名
      - 落地：`driver_tests::every_thread_records_how_its_parent_addressed_it`，三层 `coda → explore(stateful) → probe(stateless)`，四条断言全覆盖。运行态用 `AgentLoop::origin_thread` 承载，只在收到 `ToolCall` 时写入（其他 envelope 不表态、不清除），随 checkpoint 存取。反向验证过：checkpoint 不写 `parent_thread_id` → 三个线程全都自称 root，断言失败
- [x] [core types] `MessageId` / `MessageOrigin` / `TurnId` + 三个变体的字段 + `UserMessage::origin`；`HistoryEntry` 与 `AgentState.messages` / `StoredCheckpoint.messages` / `restore_history` 换型
      - Purpose: 落地数据模型
      - Verification: `cargo build`（含 `MemoryStorage`、测试 stub 随之编过）；序列化 round-trip 单测
      - 落地：分批完成（见 Deviations）。`Session::resumed_messages()` 与 `SessionOpener::load_messages` 在边界剥掉 `turn_id`，所以 wire / 前端形状不变，符合设计里"turn_id 不进 wire"
- [x] [core logic] turn 盖章 + 传播：新增 `add_user_message(turn_id, user)`（一次加锁内推进当前 turn + 追加），driver 里追加 user 消息的几处（Task / ToolCall / Resume 分支）换用它；`add_message` 从线程当前 turn 盖章；`ToolCall` envelope 带 `turn_id`；恢复时由末条 entry 反推
      - Purpose: 让 turn 归属在所有线程、所有恢复路径上成立——这是 rewind 截断的地基
      - Verification: 单测——(a) sub-agent（含嵌套、stateful 多次调用）的每条消息 `turn_id` == 触发它的 root 提交；(b) 待审批时发新 task 抢占，aborted `ToolMessage` 归属**旧** turn，按新 turn 截断后父 Assistant 的 `tool_call` 仍配对齐全；(c) 审批挂起→重开会话→approve，派发出去的 `ToolCall` 仍带正确 `turn_id`
      - 落地：(a) `one_submission_tags_every_thread_it_reaches`（三层 coda→explore→probe，断言三个线程每条消息的 turn 都等于 root 提交）；(b) `preempted_calls_are_written_off_under_the_turn_they_belonged_to`，其中第二段断言直接模拟 rewind——滤掉新 turn 后，残留的每个 `tool_call` 仍有配对结果；(c) 由 `subagent_dispatched_after_approval_restart_still_records_its_origin` 覆盖同一条链路（`turn_id` 与 `parent_message_id` 一起随 resume 状态过挂起）。**(b) 反向验证过**：把 turn 推进时机改到 envelope 入口（设计明确警告的错法），该测试立刻失败
- [x] [integration] wire task→request 返回 message_id；前端乐观条目 reconcile；展示 key 从 `message_id` 派生
      - Purpose: 打通到 UI
      - Verification: `pnpm --filter coda-web lint && test`；前端乐观渲染后收到 ack 正确 reconcile，无重复条目
      - 落地：TS 三个消息类型加 `message_id`；`historyToEntries` 的 key 从 index 合成（`history:user:${index}`）改为从 `message_id` 派生，`index` 参数随之删除。"无重复条目"这条**靠结构保证而非测试**：`userEntryId()` 是唯一生成 user 条目 key 的地方，乐观路径与历史回放路径都走它，所以同一条消息前后必然同 key。
      - **已知测试缺口**：没有加 store 层测试覆盖 reconcile。现有 web 测试只覆盖纯模块（`test/model-preferences.test.ts`），`session.ts` 目前没有可用的测试脚手架（需要 fake RPC + store 装配）。lint / typecheck / 现有 12 个测试全过，但 reconcile 路径只有上面的结构性论证，没有回归防护。要补的话是独立一件事。

## Deviations from Design

- **步骤 1–3 的落地顺序做了拆分**（仅顺序，接口/数据模型/取舍均按原设计）。改 `UserMessage` 构造签名会一次性打断全部调用点，所以先落"身份类型 + 三个变体的 `message_id` + 各铸造点 + root user 的 id 在 hub 单点铸造并经 `Task` envelope 传到 driver"（第 3 步 + 第 1、2 步的一部分），再落 `task` 改 request 与 ack（补齐第 1 步）。**第 2 步暂未勾选**：`MessageOrigin` / `TurnId` / `HistoryEntry` 推迟到各自有值可携带的步骤（origin 传播、turn 盖章）再引入，避免先落一批没有写入方的死字段。
- **`AgentState.current_turn` 是 `Option<TurnId>`，`add_message` 遇到 `None` 时会记 `error!` 并新起一个 turn。** 设计断言这不可达（assistant/tool 消息不可能是线程首条），实现也确认了这一点——`restore_history` 会从末条 entry 回填当前 turn，而抢占写 aborted 结果时历史必非空。但类型上无法证明，所以留了兜底；两种坏法里选轻的：错分组只是 rewind 不准，丢消息会让父 Assistant 的 `tool_call` 没有结果、provider 直接拒绝整段历史。
- **`ToolCall` envelope 只带 `parent_message_id`，不带整个 `MessageOrigin`。** 设计里写的是 `ToolCall { call_id, origin: MessageOrigin, .. }`，但 `origin.call_id` 恒等于同一 envelope 上的 `call_id` —— 两个必须永远相等的字段会让读者以为它们可能不等。改为只传父 id，由收件线程用手边已有的 `call_id` 组装出 `MessageOrigin`。信息量不变，少一处可能自相矛盾的冗余。
- **`task` 改成 request 后，两条原本静默/走事件的失败路径改为直接答以 RPC 错误**：模型不接受图片时原先推一条 `WireEvent::Error` 到事件流，现在答 `INVALID_PARAMS`；空任务与"会话不 live"原先静默丢弃，现在分别答 `INVALID_PARAMS` 与 `SESSION_NOT_LIVE`。设计只说了"`task` 由 notification 改为 request"，没交代这两条；改成请求的直接应答更贴合 request 语义（错误与发起它的请求相关联），但前端呈现随之从 transcript 里的错误事件变成一条 danger 活动记录。
