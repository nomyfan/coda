## Problem

用户在与 agent 对话过程中，经常想修改之前发出的某条消息重新来过——可能是提问不够精确、给了错误的上下文、或者想换一个方向探索。目前没有回退机制，用户只能开新会话或者在当前会话追加修正，这两种方式都会丢失上下文或产生冗余对话。

Rewind 功能让用户可以选中某条历史 user message，编辑后确认，会话截断到该消息位置，然后以编辑后的内容作为新的 user task 继续执行 agent。

## Scenarios

1. **修正早期指令：** 用户在第 3 轮发了一条描述不清的需求，agent 走偏了。用户点击该消息的编辑按钮，修改措辞后确认。会话回退到第 3 轮，第 3 轮之后的所有消息永久丢弃，agent 以修改后的消息作为新 task 开始执行。

2. **换方向探索：** 用户在对话进行了多轮之后，想从某个分叉点换一个完全不同的方向。用户 rewind 到目标消息，替换内容后继续。

3. **只在闲置时可 rewind：** agent 正在处理一个 turn、或挂起等待工具审批时，rewind 操作不可用（按钮置灰 / 不可点击）。用户需要先 abort 当前 turn、或先答复待审批的调用，回到闲置状态再执行 rewind。

## Dependencies

- [消息模型升级](message-model-upgrade.md) — 消息需要稳定身份、轮次归属和跨线程调用追溯
- [存储层迁移到 PostgreSQL](storage-migration-pg.md) — 持久化层从 JSON 文件迁移到 PG

## Scope

**In:**
- 仅支持 rewind root thread 的 user message
- 编辑确认后永久丢弃 rewind 点之后的所有消息
- Rewind 时精确截断 stateful sub-agent 的历史：只丢弃由 rewind 点及其之后的轮次引发的 sub-agent 消息，保留更早轮次留下的上下文
- 编辑确认后自动以新内容提交 task，触发 agent 执行
- 前端 UI：user message 上的编辑入口、编辑态、确认/取消操作
- 后端 RPC：新增 rewind 请求，处理截断 + 重新提交
- 持久化：DB 同步更新为截断后的状态

**Out:**
- 不做分支/撤销（无法恢复被丢弃的消息）
- 不支持 rewind 到 sub-agent 线程内部的消息
- 不支持在 agent 运行中或挂起等待审批时执行 rewind（仅闲置状态可用）
- 不支持编辑 assistant message 或 tool message

## Constraints

- `SessionHub` 中的 `LiveState.snapshot`（内存中的已结算历史）和 DB 中的持久化状态都需要同步截断。各 agent 的内存历史不用管：driver 每处理一个 envelope 都会从存储重新加载，两个 envelope 之间它不是权威。
- 前端的 `TranscriptEntry` 使用合成 ID（`history:user:${index}`），rewind 后 index 会变化，需要完整重建 entries。
- **仅闲置状态可 rewind**：不只是"没有 turn 在跑"，还要求没有线程挂在待审批或待工具回复上。在数据层这等价于"每个线程的 `resume_point` 都是 `Generation`"（turn 正常跑完就是这个值，`driver.rs:756/827`）。这样带引用的恢复点（`PendingApproval` 存着待审批的 tool call、`ToolExecution` 存着待回复的调用）根本不可能指向被丢弃的 turn——不需要写清理逻辑，rewind 前断言该前置条件即可（`resume_point` 是 JSONB 列，一条查询可验）。
- 截断消息之后，已写入消息的计数必须一并重置，否则后续保存会从错误位置追加（详见 `../implementation/storage-migration-pg.md` 的 Risks）。
- 会变空的线程要有处置：**剩余消息数为 0 的线程记录一律删除**，不区分 stateless / stateful，root 线程也一样（rewind 到第一条消息时 root 就会变空）。
  - **不需要区分的原因**：stateless 线程 id 由 `(父 Assistant message_id, call_id)` 派生，一个线程恰好对应一次调用、于是恰好属于一个 turn（它派生出的整棵子树继承同一个 turn），所以它只有"整条丢弃"和"一条不碰"两种命运，永远不会被部分截断。能带着剩余消息活下来的必然是在多个 turn 里被调用过的线程。真正的判据只有一个：**剩下几条消息**。
  - 剩 0 条时，对 stateless 是**必须删**——父 Assistant 消息已随之删除，再没有任何东西能算出这个 id，空行是永久垃圾；对 stateful / root 是**删与留等价**——"没有 checkpoint"与"有一份空 checkpoint"在 driver 里走同一段代码，而稳定派生保证下次调用仍是同一个 id。
  - 换来的不变量：**没有消息的线程不存在**。
- `todos` **不回滚**：它是按线程整存的一份清单、没有 turn 归属，回滚成本高；而它本质是 agent 的工作草稿而非对话事实，错位可接受，agent 下一轮会自行重写。
  - **唯一例外**：整条线程被删掉时（其全部消息都在丢弃范围内），该线程的 todos 随之消失。对 root 而言这只发生在 rewind 到第一条消息时——那意味着整段对话都被丢弃，留着一份来自被抹掉对话的清单反而更奇怪。

## Success Criteria

- 用户可以点击任意历史 user message 进入编辑态，修改内容后确认触发 rewind。
- 确认后，目标消息及其之后的所有消息从会话中永久移除，编辑后的消息成为最新的 user message。
- Agent 立即以编辑后的内容开始新 turn 的执行。
- 持久化状态与内存状态一致。
- Agent 运行中或有待审批调用时，rewind 操作不可用。
