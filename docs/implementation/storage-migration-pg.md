## Problem

把持久化层从 JSON 文件迁到 PostgreSQL：消息按行存储、写入增量且原子、列表用查询代替目录扫描，为 rewind / fork 打好关系型基础——同时保持现有功能行为不变。

需求：`../requirement/storage-migration-pg.md`
依赖：`message-model-upgrade`（本设计的 schema 引用其产出的 `message_id`、`turn_id`、复合来源 `origin`(`origin_message_id`/`origin_call_id`)，且 `StoredCheckpoint.messages` 已换成 `Vec<HistoryEntry>`，必须先落地）

## Scope

**In:**
- schema：`sessions` / `thread_checkpoints` / `messages` / `runtime_snapshots`，消息按行存储，复合外键 + `ON DELETE CASCADE`
- PG 版 session 存储（实现现有 `SessionStorage` trait，替换 `JsonFileStorage`）：load 重建、save 增量追加、都在事务内
- PG 版 `WorkspaceStorage`（元数据 / 列表 / 删除 / 改名 / effort 更新，同一公开方法集）
- 进程级 `PgPool`；启动时跑 sqlx 迁移
- `coda-server.toml` 增 `[database]`
- 专门的 PostgreSQL CI job 跑存储集成测试（`required-features` 门控，本地有 PG 亦可跑）

**Out:**
- 不做旧数据迁移工具（breaking，可接受）
- 不做 SQLite 备选后端
- 不做分页 / 懒加载（P2）
- 不保证跨线程（root + sub-agent + snapshot）整体原子——沿用当前"各自独立写"的粒度

## Assumptions

- 部署环境有可用 PostgreSQL（Docker 起，数据 mount 宿主机）。
- **消息 append-only**（`message-model-upgrade` 已核验）——增量追加"已存数量之后的尾部"是正确的；不存在改写历史中段。
- 热路径不变：streaming events 和 hub 内存 `snapshot` 仍是运行时权威源，DB 只在 checkpoint 保存 / 冷开 / 列表时命中。
- `session_id` 仍由客户端提供。**保留现有 `validate_session_id` 外部契约**（拒绝 `.`/`..`/分隔符/NUL，`storage.rs:27`）——不是为了防路径穿越（参数化 SQL 已消除），而是保持"行为不变"，且 PG `text` 本就不接受 NUL；只更新注释里的安全理由。
- 依赖 `message-model-upgrade` 已落地：schema 的 `message_id` / `turn_id` / `origin_*` 列引用其产出的字段，且 checkpoint 的消息单元已是 `HistoryEntry`（带 `turn_id`），存储层才能逐行写出该列。

## Validation Findings

- **append-only**：`grep` 确认 `AgentState.messages` 无就地修改，唯一 `insert` 是给 LLM 请求 clone 插 System 头 → 增量追加安全。
- **无现存 DB 依赖**：workspace 内无 `sqlx`/`postgres`/`diesel`，全新集成，无版本冲突。
- **存储调用面**：`bin/server.rs` 通过 `WorkspaceStorage` 的 `session()` / `list_sessions` / `initialize_session` / `rename_session` / `update_reasoning_effort` / `delete_session` 使用；`SessionStorage` trait 被 driver 调用。迁移 = 在同一公开面下换实现。

## Alternatives Considered

**消息怎么存 —— 行/条 + JSONB payload vs 整包 JSONB vs 完全列化。**
- 选择：**一条消息一行**，键/查询用到的字段（`message_id` / `turn_id` / `thread_id` / `seq` / `role` / `origin_*` / `created_at`）建**类型列**，消息完整内容存 **`payload jsonb`**。
- 放弃"整包 JSONB"（把整个 `Vec<Message>` 塞一列）：等于换了后端还在整包重写，写放大和声明式截断都没解决——这正是 reviewer 点出的坑。
- 放弃"完全列化"（把 `ContentPart`/`ToolCall`/usage/reasoning 全拆列）：`Message` 结构复杂且在演进，拆列脆弱，而我们并不查询消息内部。行/条 + JSONB 同时拿到增量追加、按位截断、排序、身份、跨线程追溯，且不受 `Message` 形状演进拖累。

**`seq` 谁来定 —— 由 `message_count` 推算 vs 数据库自增。**
- 选择：存储层按 `seq = message_count + i` 算出，0 基。
- 放弃数据库自增（`GENERATED ALWAYS AS IDENTITY` / sequence）：自增是**表级**的，给不出"线程内第几条"这个语义，而且拿不到"下标 == 向量下标"这条不变量——增量追加的整套算术就是建立在它上面的。自增还会在事务回滚时留空洞，破坏"连续区间"。
- 放弃 `SELECT max(seq)+1`：多一次往返，且并发下要靠锁才正确；而水位本来就在手上，没必要问数据库。

**会话的模型选择 —— 整存一列 JSONB vs 拆成三列。**
- 选择：一列 `model_binding jsonb`。理由是 Rust 侧已有 `SessionModelBinding` 这个聚合类型，整存是 1:1 映射；三个字段永远一起读、也没有任何查询按 provider/model 过滤或聚合，拆列换不来查询能力。以后要给会话加采样参数之类也不必动 schema。
- 放弃拆成 `provider_id` / `model_id` / `reasoning_effort` 三列：好处是 `NULL` 语义无歧义、`NOT NULL` 能让数据库替你保证 provider/model 必填、CAS 谓词更直白；代价是把一个整体拆散、每次读写都要装拆。权衡后取前者——原子性两种写法都成立（见 Data Model 里的 CAS 写法），丢的只是一点可读性。

**增量写放在哪 —— 下沉进 PG 实现 vs 改 trait 契约。**
- 选择：**`SessionStorage` trait 签名基本不动**，增量逻辑下沉进 PG 实现。driver 照旧递交完整 `StoredCheckpoint`；实现按各线程已存的 `message_count` 只 `INSERT` 尾部新消息，线程状态记录 upsert，全程一个事务。
- 放弃"把 trait 改成分解式 append API"：会波及 driver 多个调用点和 `MemoryStorage`，ripple 大。下沉方案把复杂度收进存储模块（pull complexity down），driver / trait / 测试 stub 不变。
- 澄清 reviewer 的 P0#1："换实现仍写 JSONB blob"——只在实现是"整包写一列"时成立。本设计的实现是**行感知**的（拆行 + 尾部追加），所以换实现即完成迁移，写放大随之解决。

## Components

- **`Db`（进程级）**（`coda_server`）—— 持有 `PgPool`，启动时跑迁移；派发按 `workspace_id` scope 的 `WorkspaceStorage`。
- **`WorkspaceStorage`（PG 版）**（`coda_server::storage`）—— 保持现有公开方法集，内部换成 `pool` + `workspace_id`；`session(id)` 返回 PG 版 session 存储。
- **PG session 存储**（`coda_server::storage`）—— 实现 `SessionStorage`：`load_checkpoint` 拼行重建 `StoredCheckpoint`，`save_checkpoint` 事务内增量追加 + 线程状态记录 upsert，snapshot 整行 upsert。替换 `JsonFileStorage`。

## Interfaces

```rust
// SessionStorage trait 签名不变（见 runtime.rs）。行为契约收紧：
//   save_checkpoint: 幂等；仅追加该线程"已存 message_count 之后"的消息，
//     并 upsert 线程状态记录（agent_name / reply_target / resume_point / todos /
//     suspended_at）+ bump sessions.updated_at，全部在一个事务内。
//   load_checkpoint: 按 seq 拼回完整 messages（每行 payload + turn_id → HistoryEntry）
//     + 线程状态记录，重建 StoredCheckpoint（冷开路径，全量读可接受）。

// WorkspaceStorage 公开方法集不变，构造改为持池 + workspace_id：
WorkspaceStorage::new(pool: PgPool, workspace_id: String) -> Self
// list_sessions / initialize_session / rename_session /
// update_reasoning_effort / delete_session / session(id) 语义不变，底层换 SQL。

// 配置
[database]
url = "${DATABASE_URL}"   // 支持 ${VAR} 展开，与 provider 配置一致
```

**Trust boundary —— SQL 边界。** 所有查询参数化，`session_id` / `workspace_id` 作为绑定参数，不拼字符串 → 无注入。`validate_session_id` **保持现有拒绝集**（`.`/`..`/分隔符/NUL），行为不变；仅把注释里"防路径穿越"的理由改成"保持契约 + PG text 不接受 NUL"。

## Data Model

```
sessions(workspace_id, session_id,
         name,
         model_binding jsonb,    -- SessionModelBinding{provider_id, model_id, reasoning_effort}
         created_at, updated_at,
         PK(workspace_id, session_id))

thread_checkpoints(workspace_id, session_id, thread_id,
         agent_name,
         parent_thread_id text,  -- 父线程；NULL 即 root 线程（root 的 thread_id == session_id）
         derivation_key   text,  -- 当初算本线程 id 时喂给 uuid5 的字符串：stateful=agent_name，
                                 -- stateless=(父 Assistant message_id, call_id) 复合键；root 为 NULL
         reply_target   jsonb,   -- Option<ReplyTarget>
         resume_point   jsonb,   -- StoredResumePoint
         todos          jsonb,   -- Vec<TodoItem>
         suspended_at   timestamptz,
         message_count  int,     -- 已写入的消息条数；增量追加从这里开始
         pending_approval bool,  -- 由 resume_point 派生，供列表 O(1) 查询
         PK(workspace_id, session_id, thread_id),
         FK(workspace_id, session_id) → sessions ON DELETE CASCADE)

messages(workspace_id, session_id, thread_id,
         seq                int,   -- 线程内 0 基下标；由 message_count 算出，非数据库自增
         message_id         uuid,  -- 消息身份（来自 message-model-upgrade）；唯一性按会话，见下
         turn_id            uuid,  -- 横切分组：发起该 turn 的 root user message_id
         role               text,  -- 'user' | 'assistant' | 'tool'
         origin_message_id  uuid,  -- 跨线程追溯：父 Assistant message_id（sub-agent 开场 user 消息）
         origin_call_id     text,  -- 跨线程追溯：父 tool_call.id；与上者构成稳固复合键
         payload            jsonb, -- serde_json(Message)
         created_at         timestamptz,
         PK(workspace_id, session_id, thread_id, seq),
         UNIQUE(workspace_id, session_id, message_id),  -- 按会话唯一，不是全局
         INDEX(workspace_id, session_id, turn_id),      -- 按 turn 跨线程取/删
         FK(workspace_id, session_id) → sessions ON DELETE CASCADE)

runtime_snapshots(workspace_id, session_id,
         snapshot jsonb,         -- StoredRuntimeSnapshot（drained/agent_drained/active_threads）
         PK(workspace_id, session_id),
         FK(workspace_id, session_id) → sessions ON DELETE CASCADE)
```

- **所有权 & 删除**：`sessions` 是聚合根；`thread_checkpoints` / `messages` / `runtime_snapshots` 经复合外键归属其下，`ON DELETE CASCADE` 让 `delete_session`（一条 `DELETE FROM sessions`）不留孤儿，等价于旧的删目录行为。
- **跨线程来源复合键**：`(origin_message_id, origin_call_id)` 与 `message-model-upgrade` 的 `MessageOrigin` 对应；不依赖 `tool_call.id` 全局唯一，碰撞不丢关系。root user 消息两列为 NULL。来源指向的父线程必然在同一会话内，所以按 `(workspace_id, session_id, origin_message_id)` 查得到，不需要全局唯一的 `message_id`。
- **`turn_id` 列**（来自 `HistoryEntry.turn_id`，非空）：一次 root 提交在所有线程留下的消息共享同一值，是 rewind/fork 按 turn 跨线程截断的依据——一条 `DELETE ... WHERE turn_id IN (…)` 即可，不需要沿 `origin` 递归 CTE 上溯。索引 `(workspace_id, session_id, turn_id)` 支撑该谓词。
- 只有 `messages` 是增长量、按行拆；线程状态记录（`thread_checkpoints`）、运行时快照（`runtime_snapshots`）体量有界、整行 upsert，仍用 JSONB —— schema 简单且解决主要写放大。
- **`seq` 怎么产生**：它就是这条消息在该线程历史里的**0 基下标**，由存储层在写入时算出——追加第 i 条新消息时 `seq = message_count + i`。不用数据库自增列，也不查 `max(seq)`。
  - 成立的前提是持久化的行序与内存中的 `Vec<HistoryEntry>` 严格 1:1：`SystemMessage` 从不落库、也不在 `AgentState.messages` 里（`restore_history` 过滤它，给 LLM 组请求时插在副本上）。因此有一条强不变量：**`seq` 恒等于 `messages` 向量里的下标，且每条线程的 `seq` 恒是 `[0, message_count)` 的连续区间**。
  - 好处：load 只需 `ORDER BY seq`，save 只需从 `message_count` 切尾部，两边都不查最大值、不多一次往返。
  - 并发：同一线程的两次 save 若算出同一个 `seq`，主键会直接拒绝而不是静默写坏——主键在这里兼任守卫。实际上不会发生：envelope 按 agent 串行处理，一条线程只由一个 agent 驱动。
- `message_count` 是增量追加的起点，也正是"下一个可用 `seq`"：save 时只 `INSERT messages[message_count..]`，再把它前移，避免每次 `COUNT(*)` 扫表。
- root 线程 `thread_id == session_id`（沿用现状），所以不另存 `root_thread_id`；`parent_thread_id IS NULL` 也能认出 root。`first_user_message` = 该线程 role='user' 最小 seq 一行。
- **线程拓扑**：`(parent_thread_id, derivation_key)` 把原先只藏在 uuid5 推导里的父子关系显式记下来，可直接查、可自顶向下重建整棵线程树。`agent_name` 不能顶替 `derivation_key`——stateless 线程算 id 用的是 `(父 Assistant message_id, call_id)` 复合键，不是 agent 名。
- **模型选择整存一列**：`model_binding` 直接对应 Rust 里已有的 `SessionModelBinding`（`storage.rs:51`，`SessionMetadata.binding`），一个字段一个 JSONB，不拆成三列——拆列等于把领域里本来是一体的东西打散，读写都要装拆一遍。没有任何查询按 provider/model 过滤或聚合，所以拆列也换不来查询能力。`name` 仍是独立列（可能要排序/搜索的标量）。
  - 注意：`update_reasoning_effort` 是 compare-and-set（校验 `provider_id`/`model_id` 与期望一致才改，不符返回 `BindingMismatch`），改成一条 `UPDATE … SET model_binding = jsonb_set(…) WHERE model_binding->>'provider_id' = $x AND model_binding->>'model_id' = $y`，仍是单条原子语句、0 行受影响即 mismatch，不需要读-改-写。`reasoning_effort` 为 `None` 时统一写 JSON `null`（而非删键），与 serde 的 `Option<String>` 对齐。
- 派生查询：`list_sessions` = `sessions` 连 `first_user_message` 子查询 + `pending_approval` 存在性；一条（组）SQL 取代目录扫描，随会话数走索引而非线性 IO。

## Load-Bearing Decisions

- **schema 形状**（行/条消息 + JSONB payload + 线程状态记录用 JSONB）—— 落地后改动昂贵；现阶段 breaking 可接受（直接重建库）。
- **增量追加下沉实现，trait 不变** —— 换实现即迁移，driver/测试零改。代价：driver 仍在内存 clone 完整 history 递交（磁盘写已降为增量；内存 clone 与今日一致，后续可优化为借用）。
- **进程级 `PgPool` + `(workspace_id, session_id)` 列作用域**取代目录作用域 —— `WorkspaceStorage` 构造从"目录"变"池 + id"，`bin/server.rs` 装配处随之改。
- **复合外键 + `ON DELETE CASCADE`，`sessions` 为聚合根** —— 删除语义靠数据库保证，`delete_session` 删根即可，无孤儿。
- **`message_id` 唯一性按会话而非全局** —— 换来 fork 能整片复制、引用不用重写；代价是跨会话看 id 必须带上 `session_id`。改这条要动唯一约束，所以现在定。
- **迁移随启动执行**（sqlx embedded migrations）—— 部署即建表。

## Risks / Open Questions

- **append-only 若被打破**（未来某路径就地改历史消息）→ 尾部追加会漏写。缓解：save 时可比对 `message_count` 与 `message_id` 前缀，不符则退化为"整线程重写"安全阀；先按 append-only 实现并加一条断言。
- **已知会打破它的是 rewind**：按 turn 截断会删掉每个受影响线程的消息尾部，而 `message_count` 还记着删之前的条数——下一次 save 就会从错误的位置开始插，导致漏写或错位。所以删除和"把 `message_count` 改成剩余条数"必须在同一个事务里完成。截断删的恒为尾部（turn 在各线程按序累积），所以删完 + 重置计数后，`seq` 仍是 `[0, message_count)` 的连续区间，不需要重编号。rewind 设计时把这件事和"清理受影响线程的状态记录"一起处理。
- **stateful sub-agent 线程**跨多次调用在同一 `thread_id` 累积消息——增量追加天然覆盖（每次调用只追加新尾部），无需特殊处理；列个验证用例确认。
- **fork 换 session_id 会连带换掉所有线程 id（已记录拓扑，但策略留给 fork 定）。** 子线程 id 是 `uuid5(父线程 id, derivation_key)` 算出来的，而 root 线程 `thread_id == session_id`——fork 换了 session_id 就等于换了 root，所有派生的子线程 id 都得跟着变；照搬旧 id 会让 runtime 按新 root 算出对不上的 id、开出空线程，拷来的历史等于白拷。现在把 `parent_thread_id` + `derivation_key` 落库，是为了让 fork 两条路都走得通（重算整棵 id 树 / 照搬 id 并把 root 与 session_id 解耦），不预先替它选。rewind 不受影响：按 `turn_id` 截断是 session 级谓词，与拓扑无关。
- **`message_id` 按会话唯一（已定）**，不是全局唯一。这样 fork 可以整片复制消息行、只换 `session_id`，`turn_id` 和 `origin_message_id` 的引用自动仍然成立，不需要重映射（全局唯一的话 fork 就得重铸每条 id 并逐一重写这些引用，漏一步就断因果链）。代价：跨会话对日志时得连 `session_id` 一起看——而这也顺带说明了"这两条是同一条消息的副本"。注意实际生成的仍是 UUID v4、天然不会撞，会话内唯一只是**约束下限**，不是刻意允许重复。
- **PG 测试必须在 CI 真跑，不能静默跳过**：PG 成为唯一生产后端后，"无 `DATABASE_URL` 就跳过"会让 CI 一条持久化测试都不跑却显示绿灯。方案见 Roadmap 的 `storage-pg` job（`services.postgres` + 显式点名跑独立测试目标，连不上就失败）。现有 `rust` job 是 ubuntu + macOS 矩阵，macOS runner 没有 PG，所以不能直接塞进现有矩阵。
- **首个要验证**：sqlx 能否把一条含 `reasoning_continuation` 的 `AssistantMessage` 存进 `payload jsonb` 再无损读回（现有 `checkpoint_round_trips_reasoning_continuation` 测试可平移）。

## Implementation Roadmap

- [x] [risk validation] spike：`.scratchpad/pg-spike/` 里用 sqlx 建最小 `messages` 表，写入/读回一条带 `reasoning_continuation` 的 `AssistantMessage` payload
      - Purpose: 证伪"JSONB 无损 round-trip"与 sqlx/PG 连通性
      - Verification: round-trip 断言通过
      - 落地：spike 一次通过 —— `payload jsonb` 无损（含 `reasoning_continuation` 的 format + 不透明 payload）、`message_id`/`turn_id` 按 `uuid` 列绑定、`where turn_id = $1 order by seq` 跨行取回。**新发现**：`timestamptz` 只有微秒精度，而 jiff 是纳秒。因此写入侧统一走 `as_microsecond()`（截断，不让 PG 去做四舍五入），读回即精确相等——不是"约等于"。同时确认必须给 `MessageId`/`TurnId` 加 `as_uuid()` 访问器（内层 `Uuid` 原本私有）
- [x] [schema] 落 migrations：四张表 + 索引（`(ws,session,message_id)` 唯一、`(ws,session,thread,seq)` 主键、`(ws,session,turn_id)`）+ 复合外键 `ON DELETE CASCADE`
      - Purpose: 固化数据模型与删除语义
      - Verification: 迁移在空库跑通；`DELETE FROM sessions` 级联清掉 messages/thread_checkpoints/runtime_snapshots，无孤儿
      - 落地：`app/coda_server/migrations/20260725000000_sessions.sql`（`sqlx::migrate!` 编译期内嵌）。测试 `deleting_a_session_takes_its_threads_messages_and_snapshot_with_it` 建两个会话各带 checkpoint/message/snapshot，删其一后断言三张表只剩另一个会话的行（反向验证：把 DELETE 的 session_id 打错 → 断言以 "thread_checkpoints was not cascaded" 失败）；`a_thread_cannot_belong_to_a_session_that_does_not_exist` 反向确认外键真的存在，不只是"删对了"
- [x] [core logic] PG session 存储实现 `SessionStorage`：事务内增量追加 + 线程状态记录 upsert；load 拼行重建
      - Purpose: 核心行为可单测
      - Verification: save→load round-trip 等价（含每行 `turn_id` / `origin_*`）；连续两次 save 只新增尾部行（`message_count` 生效）；平移 `checkpoint_round_trips_reasoning_continuation`；按 `turn_id` 跨线程查回一次提交的全部消息
      - 落地：`PgSessionStorage`（`storage.rs`）。7 个测试：`a_saved_thread_comes_back_whole`（每个字段 + 拆出来的 `turn_id`/`origin_*`/`pending_approval`/`message_count` 列与 payload 一致）、`saving_twice_appends_only_the_new_messages`、`a_checkpoint_that_lost_messages_is_refused`（append-only 断言）、`an_assistant_message_keeps_its_reasoning_continuation`、`one_submission_is_recoverable_across_every_thread_it_reached`、`a_stateful_sub_agent_thread_grows_across_calls`、`the_runtime_snapshot_is_replaced_not_accumulated`
      - 「只追加尾部」怎么证：用 `xmin`（最后写这行的事务号）当探针。把实现改成"每次 save 重写全部行"（`on conflict do update`）后，前两行的 `xmin` 变化 → 断言以 "the messages saved the first time must not be rewritten" 失败。这正是本次迁移要消掉的写放大，所以这条断言是有牙的。另一次反向验证：把 `turn_id` 换成随机 uuid → 4 个测试同时失败
- [x] [core logic] PG `WorkspaceStorage`：list / initialize / rename / effort / delete 的 SQL 版
      - Purpose: 覆盖元数据与列表
      - Verification: 平移 `storage.rs` 现有单测（list 排序、pending_approval 标记、image-only 预览、改名/effort）到 PG 后端
      - 落地：9 个测试搬到 `tests/storage_pg.rs`（`the_session_list_leads_with_the_most_recently_active_session`、`an_image_only_first_turn_previews_as_a_placeholder`、`the_session_list_flags_a_session_waiting_on_a_human`、`reopening_a_session_keeps_the_binding_it_was_created_with`、`a_session_name_can_be_set_and_cleared_without_touching_its_binding`、`clearing_the_reasoning_effort_stores_a_json_null`、`an_effort_update_for_a_different_model_is_rejected`、`renaming_does_not_create_a_missing_session`、`a_deleted_session_leaves_the_list_and_is_reopenable`）。留在内联的是两个 `validate_session_id_*` 和名字规范化——纯字符串逻辑，普通 `cargo test` 仍覆盖
      - 三个旧测试删掉而不是平移，因为它们测的状态在 PG 下不存在：`failed_atomic_metadata_write_preserves_the_previous_file`（靠改目录权限模拟半写，事务已经保证）、`list_sessions_skips_missing_or_invalid_metadata`（`name` 是列、`model_binding` 是 `not null`，"元数据坏了"不再是可表示的状态）、`delete_session_rejects_traversal_without_touching_filesystem`（没有文件系统可穿越；拒绝集本身仍有内联测试）
      - `has_pending_approval` 的判据从"root 线程 + snapshot 里 `active_threads` 列出的线程"变成"该会话任意线程"。`active_threads` 按 agent 名索引，一个 agent 名只记一条线程，所以旧判据依赖"同名 agent 不会有第二条活跃线程"这个隐含前提；新判据是一条 `exists` 子查询，不依赖任何前提，也更简单
- [x] [integration] `[database]` 配置 + 进程级 `PgPool` + 迁移随启动；`bin/server.rs` 装配换 PG，移除 `JsonFileStorage`
      - Purpose: 真实路径打通
      - Verification: 起服务→开会话→发 task→重开会话，历史一致；`list_workspaces` 返回正确会话列表
      - 落地：`[database].url` 必填（缺了直接启动失败，有 `parse_server_config_requires_a_database` 覆盖），支持 `${VAR}`；`main` 先 `storage::connect`（连接 + 跑迁移）再逐个 workspace 建 `WorkspaceStorage::new(pool.clone(), &workspace.id)`。`JsonFileStorage` 整块删除，随之不再需要的 `atomic-write-file` 依赖也一起摘掉
      - 真实路径怎么验的：一次性脚本起真 server（配置指向一个本地 stub 的 OpenAI 兼容端点，避免真实 API 调用），走 `list_workspaces` → `open_session` → `task` → 断连 → 新连接 `open_session`，确认历史（user + assistant 两条）从库里读回、`list_workspaces` 里该会话的预览是首个 user 消息。脚本是验证工具，不进仓库
- [x] [ci] 给 `.github/workflows/ci.yml` 增设第三个 job `storage-pg`（现有两个是 `rust` 矩阵和 `web`）
      - 落地已验证：`DATABASE_URL` 指向不存在的实例 → 退出码 101（不是 0）；完全不设 `DATABASE_URL` → 同样 101，且报错就是那句"必须指向一个一次性数据库"；不加 `--features pg-tests` 时 `cargo test -p coda_server --no-run` 的产物里没有 `storage_pg` 这个目标
      - `runs-on: ubuntu-latest` + `services.postgres`（`postgres:17`，带 `pg_isready` 健康检查），注入 `DATABASE_URL`
      - 存储集成测试放独立测试目标 `app/coda_server/tests/storage_pg.rs`，并用 **Cargo `required-features` 把它挡在默认构建之外**（`[features] pg-tests = []` + `[[test]] name = "storage_pg" required-features = ["pg-tests"]`）。注意：仅仅"放到 `tests/` 下"是**不够**的——`cargo test` 会编译并运行 `tests/` 下所有目标；靠 `required-features` 才能让未开 feature 时该目标根本不编译
      - 该 job 显式开启：`cargo test -p coda_server --features pg-tests --test storage_pg`
      - Purpose: PG 成为唯一生产存储后端后，保证它在 CI 被真正验证。关键是"连不上 PG 就让 job 失败"，而不是"没设 `DATABASE_URL` 就跳过"——后者会让 CI 一条持久化测试都不跑却显示绿灯。测试目标内 `DATABASE_URL` 缺失应直接 panic，不得跳过
      - Verification: 该 job 拉起 PG 并跑通存储测试；把 `DATABASE_URL` 指向不存在的实例时该 job **失败**（证明没有静默跳过）；不加 `--features pg-tests` 时 `cargo test` 连这个目标都不编译（现有 `rust` 矩阵 job 含无 PG 的 macOS，因此必须如此）
      - 测试隔离：**每个测试用一个随机 `workspace_id`**。schema 全部以 `(workspace_id, session_id)` 为键、`WorkspaceStorage` 本身就是 workspace 作用域，所以这样即可并行而互不干扰（`list_sessions` 也不会看到别的测试的行），不需要 `--test-threads=1` 或逐测试清库
      - 连接池：**每个测试各建一个池**，而不是靠 static 全局共享一个。sqlx 的池绑定在创建它的 tokio runtime 上，而 `#[tokio::test]` 每个测试各有一个 runtime——共享池在第一个测试的 runtime 关闭后就开始 `PoolTimedOut`（实测踩到）。连接按需建立、迁移取 PG advisory lock，所以每测试一个池的代价只是一条连接加一次版本检查
      - 测试搬家：`storage.rs` 现有那批临时目录测试（列表排序、`pending_approval`、image-only 预览、改名、effort）都要从内联 `#[cfg(test)]` 搬到 `tests/storage_pg.rs`——内联测试属于 lib 的 unittest target，管不了 `required-features`。`WorkspaceStorage::new` / `validate_session_id` 均为 `pub`，搬出去无可见性问题；两个 `validate_session_id_*` 是纯字符串校验、不碰 DB，**留在内联**，让普通 `cargo test` 仍覆盖它们
      - 已知副作用：macOS 本地跑 `cargo test` 不再覆盖存储路径，需自行起 PG（**用专门的空库如 `coda_test`，不要指向真实数据库**——迁移随启动自动执行，测试还会写入和删除）并设 `DATABASE_URL`。`coda_agent` 侧不受影响（用 `MemoryStorage`）

## Deviations from Design

- **没有 `Db` 类型。** 设计的 Components 里列了一个进程级 `Db`（持池 + 跑迁移 + 派发 workspace 存储）。实际落地是 `storage::connect(url) -> PgPool`（连接 + 迁移）加 `WorkspaceStorage::new(pool, workspace_id)`——`PgPool` 本身就是那个进程级句柄，`Db` 只会是个转发壳。少一个类型，语义不变。
- **`initialize_session` 直接返回 `SessionModelBinding`**，不再返回 `InitializedSession { metadata: SessionMetadata, created }`。storage 之外没有任何地方读 `name` 和 `created`；而在 PG 下要报 `created` 还得专门把 `on conflict do nothing` 的影响行数传出来。设计说的"同一公开方法集"仍然成立，方法没增没减。
- **`SessionFile` 改名 `SessionSummary`，`updated_at_ms` 从 `Option<u64>` 变 `u64`。** 旧类型的 `Option` 表达的是"文件 mtime 可能读不到"；`sessions.updated_at` 是 `not null default now()`，"不知道最后活跃时间"这个状态不存在了。线上 wire 类型 `SessionSummaryWire` 保持 `Option<u64>` 不动，协议和前端零改。
- **用 sqlx 的运行时 API，不用 `query!` 宏。** 宏要在编译期连库（或维护一份 `.sqlx` 离线缓存），会让"没有 PostgreSQL 就编译不过"，也多一份要跟 schema 同步的产物。`macros` feature 只为 `sqlx::migrate!`（编译期内嵌迁移文件）而开。
- **`timestamptz` 只有微秒精度**，jiff 是纳秒。写入侧先 `as_microsecond()` 截断再交给 PG（而不是让 PG 去 round），读回即精确相等。受影响的只有 `thread_checkpoints.suspended_at`——消息自身的时间戳在 `payload` 里，无损；而 `suspended_at` 的所有消费方都是毫秒级（`session.rs` 的 `as_millisecond()` 和 UI 显示）。
- **`messages.created_at` / `sessions.updated_at` 由数据库 `now()` 生成**，不从 Rust 绑。前者是"这行什么时候写的"（消息自己的时间在 payload 里），后者是列表排序用的活跃时间。`rename_session` 和 `update_reasoning_effort` 故意**不** bump `updated_at`——旧实现改元数据也不动 checkpoint 的 mtime，改名不该让会话跳到列表最前。
- **`Message::System` 进 checkpoint 是硬错误。** `messages` 的 `message_id` 是 `not null uuid`，而 `SystemMessage` 没有 id。系统提示从不进历史（`restore_history` 过滤、组请求时插在副本上），所以这里返回 `Err` 而不是临时铸一个没人能追溯的 id。
