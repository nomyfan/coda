## Problem

让用户复用 Background tasks 面板查看 shell 结果，见 [需求](../requirement/background-shell-results.md)。

## Decisions

- 复用任务归档、get_task_result 和面板展开交互；不新增存储或通知通道。
- 归档提供独立的 read_result，只读取终态结果，持有任务 commit lock 防止配额删除与读取交错；不修改游标、manifest 或通知确认。
- shell 一次读取每路 ring 的保留窗口（每路最多 512 KiB），返回 stdout、stderr 和各自覆盖字节数。保留失效状态与读取错误分别处理。
- RPC 结果保持 state 标签，available 的 output 用 kind 区分 shell 与 subagent；status 使用现有 TaskStatus，前端不解析状态文本。
- notice 直接保留现有 outcomes 到前端 entry，为每个 finished outcome 提供按 ID 查看入口；content 仅包含任务信息、结束状态和 task_output 读取提示，不保存输出。
- 近期任务列表不含目标 ID 时，面板显示独立的历史任务结果区域，仍调用同一个接口。
- 不提供运行中日志推送；面板读取不算模型已读。subagent 的完整答案展示保持 Markdown，shell 为纯文本。

## Alternatives Considered

- 解析 notice.content：文本不构成稳定协议，且仅包含尾部，无法代替结果读取。
- 让面板调用 task_output：会推进模型游标并把用户查看与模型结果确认耦合，因此独立读取。
- 输出分页：当前每任务最多 1 MiB 原始 shell 输出，先采用有界快照，避免引入额外游标协议。

## Implementation Roadmap

- [x] 归档非消费式结果读取及隔离、覆盖、过期测试。
- [x] 扩展 get_task_result 和 TypeScript 协议，保持状态结构化。
- [x] 面板 shell 展示、历史任务入口和多任务 notice 关联。
- [x] 更新提示词，执行 Rust 与前端项目检查。

## Validation

- cargo clippy、cargo test、cargo check -p coda_server --features pg-tests --all-targets 通过。
- 前端 lint、typecheck 及 138 个测试通过。
- 新增测试覆盖模型游标与通知隔离、重开归档、ring 覆盖、配额淘汰、损坏输出读取失败，以及 shell 纯文本和 subagent Markdown 展示。

## Follow-up

按用户确认，完成通知仅保存任务信息、结束状态和 task_output 读取提示。删除输出尾部的采集与存储；覆盖字节数仅在结果读取时返回。ring 淘汰后历史通知不能恢复输出。
