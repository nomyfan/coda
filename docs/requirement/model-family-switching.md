## Problem

已创建的会话绑定具体 provider/model，无法在兼容供应商之间切换；预览模型下架、改名后，历史会话会变成只读。需要通过用户配置的 family code，允许同一会话继续使用兼容模型。

## Scenarios

- P1:M1 与 P2:M1 完全兼容，配置相同 family 后，用户可在原会话内手动切换，保留历史和状态。
- P1:M1-Preview 下架并从配置移除后，用户打开原会话，手动选择同 family 的 P1:M1 恢复使用，无需新建会话。
- 用户已确认：下架后手动选择替代模型；系统不自动选模型、不自动故障转移。
- 未配置 family 或 family 不同的模型，不能通过本功能切换。

## Scope

- 在每个模型配置上增加可选 `family` 字符串，跨 provider 使用同一命名空间，比较非空 code 是否完全相同。
- family 表示配置者声明成员能够相互继续使用历史消息，包括 tool calls 和适用的 reasoning continuation；不根据名称、provider 或 API 外形推断兼容。
- 会话保留具体 provider/model 和 reasoning effort，同时保存用于后续兼容判断的 family 信息；原模型被删除后仍可找到兼容候选。
- 用户已确认：数据库中 family 缺失或为 null 都表示尚未建立归属，不区分旧会话与创建时未配置 family 的会话，不引入专门区分两者的版本或三态。
- 未建立归属时，原 provider/model 配置后来增加非空 family，可以据此补录；未配置不表示永久禁止加入 family。首次保存非空 family 后固定，后续切换不能改变归属。
- 当前模型可用时允许同 family 切换；原模型不可用时保留只读浏览，并提供手动恢复入口。
- 切换成功后持久化新绑定，重连和服务重启后保持选择；历史、工具状态和会话标识保留，失败不能报告为成功或丢失原会话。
- 用户已确认：模型来源保存在每条 `AssistantMessage` 的生成元数据中，记录该次请求的 provider_id、model_id 和 reasoning_effort；不能通过会话当前绑定反推历史模型。
- 范围按现有 session 的默认模型定义：继承默认模型的子 agent 随之改变，显式指定模型的子 agent 保持其配置。
- 不包含任意跨 family 切换、自动路由、自动验证外部模型兼容性，以及独立切换各个子 agent 的模型。

## Constraints

- 保留现有执行与审批约束；不能通过切换绕过审批、干扰正在运行的 turn、compaction、后台子 agent 或尚未完成的 scope 清理。
- 只读恢复可能遇到持久化的待审批调用，必须保留审批要求，不能因更换模型自动执行这些调用。
- family 只决定兼容范围；目标模型的 reasoning effort、输入模态、上下文容量等约束仍需处理。
- 兼容判断由服务端负责，前端提供一致的候选列表；不能只解除下拉框禁用。
- 项目允许破坏性变更，但本功能需要保留并恢复目标历史会话，不能用删除历史满足这一场景。

## Validation Findings

- `app/coda_server/src/hub.rs::handle_set_model` 当前只允许同一 selection key 调整 effort，已有空闲时重建 runtime 的路径。
- `app/coda_server/src/storage.rs::SessionModelBinding` 当前只保存 provider_id、model_id、reasoning_effort；存储更新方法也只修改 effort。
- `app/coda_server/src/session_access.rs` 在原模型缺失或 effort 不受支持时返回只读；RPC、hub、前端 store 和选择器均阻止只读会话切换。
- `crates/coda_openai/src/lib.rs` 按 provider kind 处理历史 reasoning 数据，证明兼容性不能仅用 API 形状或模型名称判断。

## Success Criteria

- 同 family 跨 provider、同 provider 跨模型 ID 的切换均可保留历史继续对话；缺少 family 或不同 family 的跨模型请求被服务端拒绝。
- 删除已记录 family 的原模型配置并重启后，历史仍可读，用户可手动切换到同 family 模型恢复；没有候选时继续只读。
- 旧会话与新建时未配置 family 的会话遵循相同补录规则；原模型后来声明 family 后都能建立归属，已保存的非空 family 不被后续配置覆盖。
- 切换的持久化失败、重连、fork/rewind、待审批状态及执行中的拒绝路径具有明确且一致的行为。
- 切换后仍能区分历史消息的模型来源，子 agent 使用自己的实际模型；fork 保留来源记录，rewind 不把保留消息改记为当前模型，缺失的历史信息不能伪造。

## Agreed Boundaries

- 会话没有 family 且原模型配置已删除时，先恢复原 provider/model 配置并声明 family，才能补录；不根据名称猜测归属。
- 已保存的非空 family 不随模型配置修改；目标模型的能力差异和切换失败处理由设计明确。
- 当前不新增独立调用审计记录；没有产生 assistant 消息的失败请求不在本次来源记录范围内。
