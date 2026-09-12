## Problem

通过用户声明的模型 family，让历史会话在兼容的 provider/model 之间手动切换，并准确保留每条回复的模型来源。需求见 [model-family-switching](../requirement/model-family-switching.md)。

## Scope

包含模型配置、会话 family 补录与持久化、同 family 切换、不可用模型的手动恢复、assistant 消息来源、Web 选择与历史展示。

不包含自动故障转移、跨 family 历史转换、外部兼容性检测、独立切换子 agent 模型，以及完整请求审计。压缩摘要仍使用现有 `CompactionMessage`，本次不增加其模型审计字段。

## Assumptions

- family 是配置者对历史可互用的声明，包括 reasoning continuation；系统不能证明供应商端的实际兼容性。
- 模型目录仍在服务启动时读取，不新增模型配置热更新。
- 使用现有单个进程级 SessionHub 的会话所有权机制，不引入多个服务器同时运行同一会话的能力。
- 旧历史没有生成元数据是合法状态；不能用当前会话绑定补造历史来源。
- 待审批、冷恢复残留后台执行、目标打开失败都是可达状态，必须处理。

## Validation Findings

| 核查点 | 代码与发现 | 设计影响 |
| --- | --- | --- |
| 现有切换 | `hub.rs::handle_set_model` 在空闲时重建 runtime，仅允许 effort 改变 | 复用 hub 入口与 runtime 构造，不直接修改运行中的 profile |
| 打开是否无副作用 | `SessionBuilder::open` 会调用 `abort_scope` 清理冷恢复的后台执行，再检查审批并 bootstrap | 不能在提交绑定前打开一个候选 runtime 作为无副作用预检 |
| 只读限制 | RPC 的 `provider_of`、hub command、Web store 和选择器均拦截只读切换 | 必须贯通恢复入口，不能只解除 UI 禁用 |
| 当前绑定 | `sessions.model_binding` 是 JSONB；更新函数只修改 effort | 改为完整绑定的条件更新，无需新表或关系型列 |
| 消息来源 | `AssistantMessage` 已含 usage 和生成时间，runtime 在持久化前补齐时间；LLMStart 的模型只在事件中 | 同一位置补齐生成元数据，正常完成与中断消息都覆盖 |
| 历史归属 | `messages` 已有 pid、turn_id，fork 复制 payload，rewind 删除消息行 | 模型来源随消息维护，不新增 turn-model 表 |
| 释放副作用 | `begin_release` 会关闭 session 的后台 registry 并终止 shell 进程 | 切换及可重试的打开失败不能直接调用整个 entry 的 release |

## Data Model

模型配置增加 `family: Option<String>`。省略表示未配置；显式值必须是非空且没有首尾空白的字符串，大小写敏感，不进行名称推断。`""`、全空白、错误类型报配置错误。family 在所有 provider 间共享命名空间。

```toml
[[providers]]
id = "P1"
kind = "generic"
api_key = "${P1_API_KEY}"
base_url = "https://p1.example/v1"
models = [
  { id = "M1-Preview", family = "m1", context_window = 128000 },
  { id = "M1", family = "m1", context_window = 128000 },
]

[[providers]]
id = "P2"
kind = "generic"
api_key = "${P2_API_KEY}"
base_url = "https://p2.example/v1"
models = [{ id = "M1", family = "m1", context_window = 128000 }]
```

以上为 provider 配置片段，其余 server 配置沿用现状。

会话与消息分别保存两种事实：

```rust
struct SessionModelBinding {
    provider_id: String,
    model_id: String,
    reasoning_effort: Option<String>,
    family: Option<String>, // 缺失/null 均表示尚未建立归属
}

struct GenerationMetadata {
    provider_id: String,
    model_id: String, // 该次请求发送的 API 模型 ID
    reasoning_effort: Option<String>,
}

// AssistantMessage 新增：
generation: Option<GenerationMetadata>
```

- `SessionModelBinding` 留在 `sessions.model_binding`；非空 family 是会话固定的兼容归属，具体 provider/model 可变。
- `generation` 留在 `messages.payload` 中，属于不可改写的历史事实，不复制 family、API key、URL 或模型展示名称。
- `ModelProfile` 增加明确的 `provider_id`。runtime 从该 profile 及实际请求参数构造 metadata，不拆分用于日志的 `label`，不信任 provider 返回值替代配置身份。
- 新的真实生成消息必须有 metadata；旧消息及无真实模型调用的合成消息允许为 None。缺失字段按 None 读取是旧历史的真实语义，不增加格式版本或三态。
- runtime 在发起请求时捕获 metadata，在正常完成和有内容的取消消息上、发事件和保存 checkpoint 前填入；即使 provider 返回 usage=None 也记录来源。
- 未产生消息的失败请求、空输出取消不新增伪 assistant 消息。adapter 转换请求时继续显式选取聊天字段，不向模型发送 metadata。

## Load-Bearing Decisions

### 1. family 的建立与固定

新建会话时保存所选模型的 family。打开已有会话时，若保存的 family 为空且原 selection key 仍存在，就从该模型配置取得非空 family，并通过条件更新补录；否则保持空值。补录失败不能当作已经成功建立归属。

补录在 attach 的持久化绑定解析阶段完成，不在目录浏览时写数据库。已打开的会话在配置不热更新的前提下不需要定时补录。仅修改配置而没有打开旧会话，并不等于已经补录；删除旧配置前须先打开需要保留切换能力的旧会话。

非空 family 永不被正常打开或切换覆盖。fork 复制其归属；rewind 保留当前绑定，不因为回到早期 turn 自动切回历史模型。

如果已保存 family=F，而当前 selection key 的配置变成 G 或移除了 family，原模型不再符合该会话归属，打开为只读 `model_family_changed`。用户可选择仍声明 F 的模型，或恢复原配置。这样不会经由同一个 selection key 把 F 的历史带入 G。

### 2. 切换资格与目标参数

规则优先级：先验证目标模型存在及会话已建立的 family，再处理以下请求分支。只要会话保存了非空 family=F，所有目标都必须仍声明 F，包括同模型 effort 修改、只读恢复、失败重试以及相同选择的幂等请求。目标改属 G 或移除 family 时，不能靠这些分支绕过固定归属。“无需 family”仅适用于尚未建立归属的会话调整原模型 effort；该会话仍不能跨模型切换。

| 请求 | 行为 |
| --- | --- |
| 相同 provider/model/effort，且 runtime 正常可用 | 通过上述 family 校验后幂等，无需重建 |
| 相同 provider/model，只改 effort | 未建立归属时无需 family；已建立归属时目标 family 必须一致；参数必须被目标支持 |
| 不同 provider/model | 会话的非空 family 必须等于目标配置的 family |
| 原模型已移除，会话 family 非空 | 可手动选择同 family 的目标，不依赖已删除的原配置 |
| 原模型已移除，会话 family 为空 | 继续只读；恢复原配置并补录后再切换 |
| 已保存 effort 不受支持 | 通过上述 family 校验后，可在原模型上改为有效 effort，或选择同 family 模型恢复 |
| 上次打开 runtime 失败，重复选择当前目标 | 重新验证目标与固定 family 后重试恢复，不按幂等 no-op 跳过 |

存在前台 turn、compaction、待审批的 Live/Pending 执行、后台子 agent 或 scope 清理时，拒绝改变选择。独立后台 shell 可继续运行，registry、任务结果和通知队列留在 entry 上；切换不终止它们。

只读会话没有运行中的 runtime，允许选兼容模型恢复，即使存储中有待审批调用。恢复先沿用冷启动清理，再进入现有 Pending 审批流程；不提交虚构的批准决定，不绕过 ask_user。恢复成功可能表示“等待审批”，而非“可以直接发送新任务”。

目标模型可有不同的 effort 列表；保留当前 effort（若支持），否则 UI 选择目标默认值并显示后提交。显式传入不支持的值由服务端拒绝，省略则使用目标 `default_reasoning_effort`，再回退首项，无控制项则 None。

不要求同 family 的所有配置字段相等。切换预检检查所有将继承新默认模型的已保存 process 的有效模型上下文；存在图片而目标仅支持文本时拒绝，显式指定其他模型的 process 不参与此检查。历史兼容性以服务端候选结果为准，前端只对未提交的草稿图片增加限制；已被压缩覆盖、但仍显示在历史中的图片不能再次禁用合法候选。

上下文容量、输出上限和压缩阈值采用目标配置。既有 usage 是旧请求的统计，不能精确证明新模型的请求会适配；本次不新增 tokenizer 或自动跨模型历史转换，也不因切换直接压缩/删除历史。较小上下文的目标可能连压缩请求都无法接收，不能把自动压缩当作兼容保证；此限制在错误处理和验证中明确保留。

### 3. 绑定提交与 runtime 激活分开

选择数据库绑定的提交作为唯一切换提交点，不尝试让数据库事务覆盖 runtime 启动或外部工具副作用。

1. hub 在会话锁内核验连接所有权、执行状态、完整预期绑定和目标选择，读取稳定历史完成能力预检。
2. 更新完整 `SessionModelBinding`，保留已建立的 family。已确认未写入或已回滚的失败才允许恢复旧 runtime 的接纳能力；连接错误导致结果未知时，旧 runtime 保持暂停接纳，先按下述流程确认，不能把所有存储错误都当作未提交。
3. 提交成功后，阻止旧 runtime 再接受输入或自动任务通知；等待旧 runtime 完全 shutdown，再打开目标 runtime。entry 的 permission cell、后台 registry、通知队列和连接保留，旧 forwarder 通过 generation 失效。
4. 目标打开成功进入 Live；遇到 `PendingApprovalsRequired` 进入 Pending；其他打开错误进入新的 `ReopenRequired` 状态，保留新绑定、历史、审批和后台 registry，等待显式重试。
5. 返回新的完整 snapshot，统一更新模型、family、access、审批与后台状态。只有 Live/Pending 恢复成功才提示切换完成；打开失败明确提示“模型选择已保存，会话恢复失败”，可重试当前目标或改选兼容目标。

`ReopenRequired` 无可执行 runtime，对执行命令只读，允许重新 set_model、查看历史/任务结果及既有管理操作。Snapshot 增加 `runtime_open_failed` 只读原因和可展示的打开错误；已附着会话的目录 access 优先使用 hub 状态。`runtime_open_failed` 和 `binding_unconfirmed` 的 entry 即使没有后台任务或通知，也跨断线保留；新 attach 继续展示原错误，等待显式重试，不因网络重连自动恢复。普通只读会话仍可在无人连接且无后台工作时释放；删除和服务关闭继续清理 entry，服务重启后按数据库绑定走正常打开流程。

重建由 hub 持有并完成，不因原请求连接断开而取消。切换窗口不接纳任务，不投递自动通知；请求的最终 snapshot 必须先于新 runtime 事件交付，避免客户端用旧快照覆盖新消息。复用现有 attach 的订阅与事件排序规则。

数据库写入响应丢失时属于提交结果未知：继续阻止输入、自动通知和新的绑定变更，确认结束前不恢复旧 runtime。普通 `load_model_binding` 不足以确认结果：Read Committed 的普通 SELECT 可能读到旧值，而原事务随后才提交。[PostgreSQL Read Committed 文档](https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED)

确认使用同一数据库主库上的新连接和短 Read Committed 事务，按稳定的 `(workspace_id, session_id)` 主键执行 `SELECT model_binding ... FOR UPDATE`，不把旧绑定值放进 WHERE。只有取得与原写事务冲突的行锁、等待其提交或回滚后，才使用锁定读取返回的最终绑定判断：等于 expected 表示此次切换未提交，等于 next 表示已经提交并继续恢复。不能用之前的普通读取结果代替这个结果。[PostgreSQL 行锁文档](https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS)

这一确认依赖写入路径先取得同一行锁，再发送 UPDATE，并一直持锁到事务结束，具体顺序见存储接口约束。锁等待超时、连接再次中断、行消失或读到 expected/next 之外的绑定，都不能解禁执行或自动重写旧绑定。此时继续保留结果未确认的状态，只允许后续重新确认；确认事务正常结束后才按结果推进。确认期间会话锁/操作门禁禁止其他会话绑定变更，因此不会将后续切换的结果误认为本次结果。

显式重试完成确认后，先按当前配置重新判定最终绑定的可用性。若确认回滚到已经下架的 Preview，清除待确认状态及恢复错误，返回普通只读 snapshot，让用户重新选择同 family 的可用模型；不能继续要求该 Preview 通过模型校验。绑定可用时继续恢复；若后续历史校验或打开失败，保留明确的恢复失败状态，不能退回 `binding_unconfirmed`。

进程崩溃后的第一次 attach 在启用 runtime 前也通过这一行锁等待关系读取绑定，不能假定旧数据库连接已随应用进程立即退出。确认后以数据库中保存的绑定为准；若无法确认则保持禁止执行。

这一选择接受“模型已保存、runtime 暂时打不开”的明确状态，避免打开新 runtime 已经做了恢复清理后再回滚模型。失败不丢历史，也不把旧模型留作隐式 fallback。

### 4. assistant 消息是来源的唯一历史依据

根 agent 与继承默认模型的子 agent 使用切换后的 profile；显式模型 override 保持原样，每条消息记录自身实际 profile。不能从根 session 或当前目录推断子 agent 历史来源。

无需新增 turn-model 关系：按 pid/turn_id 查询对应 assistant 消息即可。UI 显示消息记录的模型 ID，详情显示 provider 和 effort；配置已删除时仍能展示原始 ID。旧消息无 metadata 时显示来源未知或不显示标签，不借当前模型补齐。

用户完成前台审批后恢复同一 turn，或者冷恢复后使用新兼容模型继续生成时，以每条消息为准；不建立“一整个 turn 永远只有一个模型”的存储约束。

## Components

- `config.rs` 与 provider catalog：解析 family，提供配置身份和目标参数；同一目录供候选展示和服务端验证使用。
- `session_access.rs`：集中处理固定 family、当前绑定可用性与跨模型资格，避免 RPC、hub、目录各有一套判断。
- `storage.rs`：完整绑定的原子条件更新，以及切换预检所需的 checkpoint 历史读取；不把 provider 配置规则放进存储层。
- `SessionHub`：会话所有权、执行状态门禁、提交后重建、Pending/ReopenRequired 状态、snapshot 与通知顺序。
- `coda_core` / `coda_agent`：GenerationMetadata、明确的 profile provider 身份及正常/取消消息填充。
- `coda_web`：兼容候选、只读恢复操作、完整 snapshot 应用与消息来源展示。

共享可变状态的唯一运行时所有者仍为 hub entry。各 phase 使用完整 binding，不再各自维护可能漂移的 provider_id/effort/family 副本；wire 的 selection key 由 binding 派生。

## Interfaces

以下为接口意图，异步 trait 的装箱方式沿用项目现状。

```rust
// 返回目标绑定，或具体的模型/兼容/参数错误；不启动 runtime、不修改存储。
// 信任边界：先验证目标存在和固定 family，再验证 effort 与受影响历史的输入模态。
// 同模型 effort 修改、恢复、重试和 no-op 都不能绕过已建立的 family。
async fn validate_model_change(
    key: &SessionKey,
    current: &SessionModelBinding,
    requested: &ModelSelection,
) -> Result<SessionModelBinding, ModelChangeError>;

// 仅当持久化完整绑定仍等于 expected 时写入 next；返回保存后的绑定。
// 不存在、绑定已变化、确定未提交、提交结果未知分别报告；可用于 family 补录与切换。
async fn compare_exchange_model_binding(
    session_id: &str,
    expected: &SessionModelBinding,
    next: &SessionModelBinding,
) -> Result<SessionModelBinding, SessionMetadataError>;

// 等待可能仍在执行的旧绑定写事务结束，返回经冲突行锁确认的最终绑定。
// 锁等待/读取/事务结束失败均不得作为“未提交”的证明；调用方在此期间禁止执行。
async fn confirm_model_binding(
    session_id: &str,
) -> Result<SessionModelBinding, SessionMetadataError>;
```

存储条件更新使用短事务：先按主键 `SELECT ... FOR UPDATE` 并等待成功返回，反序列化并比较完整绑定，再发送 UPDATE、等待结果、发送 COMMIT；不将加锁与写入流水线发送，也不在连接错误后继续原写入或自动重试。锁持续持有到事务结束，不通过 savepoint 提前释放。这样，只要 UPDATE 可能已发送，原事务就已取得确认流程将等待的行锁；若写入路径在收到加锁成功响应前失败，后续 UPDATE 根本没有发送，不存在稍后提交绑定的路径。

`SessionMetadataError` 必须区分已知未提交与 `OutcomeUnknown`。字段缺失与 null 按同一 None 语义比较，不直接对新序列化 JSON 做字节/JSON 相等比较。`confirm_model_binding` 必须在同一主库用新的 Read Committed 事务完成锁定读取；不用副本、缓存、普通 SELECT、SKIP LOCKED，NOWAIT/锁超时也不能当作确认成功。attach 对已有绑定采用同样的锁定读取作为启动前提。数据库锁内不读取远程 API 或打开 runtime。

RPC `set_model` 请求保留现有 provider_id selection key 与 reasoning_effort；客户端不上传 family。结果改为完整 Snapshot；提交前失败返回既有/扩充的选择错误，提交后恢复失败返回带错误状态的新 snapshot，不把两者混为“选择未改变”。未附着或旧连接仍拒绝。

Snapshot 增加 `model_family`、兼容候选 selection key 列表，以及可选的 runtime 打开错误。候选由服务端生成并排除已知输入不兼容的目标；RPC 执行时仍在会话锁内验证。候选与“当前是否空闲”分开表达，运行中可以展示列表但不能提交。

ProviderInfoWire 增加 family，供新会话选择和展示使用。已有会话以服务端给出的候选为准，尤其原 selection key 已从目录消失时仍显示可操作选择器。只读禁用发送与执行，不笼统禁用模型恢复入口。

## Alternatives Considered

| 选择 | 可行替代 | 取舍 |
| --- | --- | --- |
| 可选模型 family + 会话固定归属 | 独立 family 实体、成员列表与默认路由 | 当前只需要等值分组；增加实体会引入不需要的自动目标与额外配置 |
| 空值统一表示未建立归属 | 用版本/三态区分旧会话和显式未配置 | 两者补录规则相同，区分不增加行为价值 |
| 非空 family 固定 | 每次使用当前配置的 family | 后者可经配置变更把历史迁到另一兼容组，且原配置删除后失去依据 |
| 先提交绑定，再重建，失败可重试 | 拆分整个 SessionBuilder 为无副作用 prepare 与 activate | prepare/activate 可以保留更多失败原子性，但需重构冷恢复、审批和 bootstrap；本次采用更局部且明确的提交语义 |
| 通过冲突行锁确认未知写结果 | 原草案在写响应丢失后普通读取，读到旧值就认定未提交 | 普通读取不等待原事务，可能先读到旧值再发生原事务提交；因此必须先建立事务结束的等待关系 |
| 每条 assistant 记录来源 | 独立 `(pid, turn_id)` 模型表 | 消息粒度覆盖工具循环与审批恢复，复用 fork/rewind；没有额外表的生命周期维护 |
| family 声明 + 已知输入约束检查 | 强制同 family 所有模型配置完全一致 | 完全一致会不必要地限制供应商的 effort/限额差异，且仍不能证明上游 reasoning 兼容 |

## Risks / Open Questions

- 最大实现风险是“绑定已提交、旧 runtime 关闭、新 runtime 未就绪”之间的事件与失败处理。第一步先用 fake opener 和内存存储验证状态转换，再接 PostgreSQL，避免功能做到最后才发现通知或审批竞态。
- 供应商真实兼容性是用户声明；尤其签名/不透明 reasoning payload、上下文上限不能靠同 family 自动修复。首版不通过丢弃 reasoning 或历史来掩盖不兼容。
- 已保存 family 后修改/移除配置 family 会产生新的只读原因，这是固定归属的直接结果，需要配置说明与错误文案明确。
- family 为空的旧会话只在打开时补录；没有来源元数据的旧 assistant 消息永久保持未知。
- 显式 override 的子 agent 若引用已删除模型，现有启动校验仍会报错；需同步修改其配置，不因 session family 自动改写 agent 配置。
- 暂无需要用户继续澄清的需求边界；以上失败语义和配置约束作为本次待审设计的一部分。

## Implementation Roadmap

- [x] [状态验证] 用 hub fake opener 验证提交前失败、提交后打开失败、Pending 恢复、同目标重试及断线完成；补充旧 runtime 不接收工作、后台 shell registry 不被关闭的断言。
  目的：先验证提交点与 ReopenRequired 状态是否覆盖真实失败路径。
  验证：数据库绑定、内存选择、snapshot、审批及通知顺序一致；OutcomeUnknown 期间保持禁止执行，只有锁定确认旧值后才恢复旧 runtime，确认新值则恢复新模型；失败后没有隐式旧模型调用。
- [x] [配置与存储] 增加 family，集中资格判断，改完整 binding 条件更新与 attach 补录。
  目的：建立持久化兼容归属，原模型移除后仍可解析候选。
  验证：配置空值/错误类型、跨 provider、空值补录、非空不覆盖、配置 family 漂移、并发条件更新、fork/rewind；JSON 缺字段历史可读。固定 F 后把原模型配置改为 G/None，分别验证同模型 effort 修改、只读恢复、失败重试和相同选择请求均被拒绝；保留未建立归属时原模型 effort 可调整的对照用例。
- [x] [数据库时序] 在 throwaway PostgreSQL 用独立连接和同步屏障验证未知提交结果，不能仅用 fake storage 模拟锁语义。
  目的：证明确认读取等待原事务结束，关闭“先读到旧绑定，原事务随后提交”的窗口。
  验证：事务 A 锁行、更新后保持未提交；连接 B 普通读取仍为旧值；确认连接 C 的 FOR UPDATE 必须阻塞。A 提交后 C 返回新值，hub 在此之前不得启用旧 runtime；A 回滚时 C 才返回旧值。另覆盖锁超时/连接错误继续禁止执行、首次 attach 等待残留写事务，以及加锁未确认前不会发送 UPDATE。使用明确的同步信号/数据库锁状态观察保证时序，不靠 sleep 猜测。
- [x] [消息来源] 增加 GenerationMetadata 与 profile provider_id，覆盖完成/取消消息及 provider 请求转换。
  目的：每条历史回复能独立说明当时的模型来源。
  验证：根/继承/override 子 agent、无 usage、取消、旧消息、存储往返、fork、rewind；发往 provider 的请求不含 generation 元数据。
- [x] [服务端整合] 打通 live 切换、只读恢复、状态快照、目标模态校验和 effort 选择，保留 cold-open 清理及审批。
  目的：把配置分组变成可恢复且状态一致的用户操作。
  验证：真实 throwaway PostgreSQL 的 RPC 测试覆盖 Preview 配置删除并重启、待审批恢复、当前 effort 下架、scope 清理失败、打开失败重试与断线重连；不访问真实 LLM。
- [x] [Web 与说明] 替换 modelLocked 判定，接收完整 snapshot，增加恢复错误/重试和消息来源展示，更新配置示例与项目说明。
  目的：旧模型缺失时仍可操作，界面不误报恢复成功，不反推历史来源。
  验证：正常切换、只读恢复、目标默认 effort、无候选、旧消息、子 agent 来源、旧响应/新事件顺序；配置目录删除后 ID 仍可显示。
- [x] [最终检查] Rust 执行 `cargo clippy`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`；在 throwaway 数据库执行 pg-tests。Web 执行 `pnpm --filter coda-web lint` 与 `pnpm --filter coda-web test`。
  目的：覆盖默认构建未编译的存储测试，以及协议两端。
  验证：全部通过；检查默认系统提示与 templates，仅在模型/审批恢复行为影响 agent 决策时补充简短规则，不向 prompt 复制配置或存储实现。

## Deviations from Design

- `ReopenRequired` 复用 `ReadOnlyState` 表示，以 `runtime_open_failed` / `binding_unconfirmed` 区分打开失败与绑定待确认，避免复制只读历史、任务结果及管理操作的处理。断线释放时单独保留这两种状态，不能把它们当作普通只读 entry 释放。
- attach 补录 family 时，也会持久化原绑定缺少的默认 effort；不能只修改内存中的 effort，否则后续完整绑定条件更新会与数据库不匹配。

## Implementation Verification

- hub 测试覆盖提交前失败、提交结果未知、确认提交/回滚、确认失败保持只读、打开失败后同目标重试、请求取消，以及后台 shell registry 和 permission mode 保留。
- 本次 review 的回归测试先复现、后验证修复：确认回滚到已下架 Preview 后返回普通只读 snapshot，并可手动改选兼容模型；两种恢复错误在无后台工作时跨断线保留，重连不调用 opener 或重新确认，显式重试才恢复。另有对照测试验证普通只读会话仍在断线后释放。
- 独立 PostgreSQL 测试库验证真实行锁等待、普通读取旧值后原事务提交/回滚、确认锁超时、首次 attach 等待残留事务、绑定条件更新，以及消息来源随 fork/rewind 保留。
- 数据库 RPC 测试覆盖 Preview 删除并重启、跨 provider 手动恢复、待审批保留、family 漂移拒绝 effort/no-op/重试、空归属补录、子 agent 图片及显式 override、冷恢复 scope 清理失败后重试。
- 生成来源测试覆盖根 agent、继承模型及显式 override 的子 agent、无 usage 的正常回复、取消后的部分回复、旧消息反序列化，以及 provider 请求不携带历史来源字段。
- Web 测试覆盖完整 snapshot 恢复、已提交但未能打开时保持只读、同目标重试、已删除模型的选择器展示、目标默认 effort 和历史/子 agent 消息来源。
- Web 回归测试通过实际 App → Composer → ModelSelector 的数据传递，验证压缩后的历史图片不禁用服务端允许的文本候选、未提交的草稿图片仍要求图片能力，以及服务端排除的候选不会被前端启用。
- 已检查默认系统提示与 templates。本次模型配置和 UI 恢复未新增 agent 需要遵守的执行规则，因此无需修改 prompt。

最终检查：`cargo clippy`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets` 均通过；完整 `cargo test --features pg-tests` 使用本次新建的临时测试库通过，其中 hub 所在的服务端库测试 278 项、数据库存储测试 54 项、数据库 RPC 测试 7 项。Web 的 lint、typecheck 和 167 项测试全部通过。`git diff --check` 无错误。临时测试库已删除，未调用真实 LLM。
