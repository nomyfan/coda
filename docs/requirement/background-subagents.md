## Problem

Root agent 目前只能同步调用 subagent，必须等待其结束。首版允许 root 选择后台委派，先继续当前工作，并复用现有后台任务的观察、审批和控制体验。

## Scenarios

- Root 调用自己可用的 `agent__*` 时设置 `run_in_background: true`，立即获得 task ID，随后继续工作或结束当前 turn。
- Root A → 后台 B → 同步 C：B 等待并处理 C 的答复，B 完成后通知 A。B 和 C 均不能启动后台 subagent；它们仍可按现有配置同步委派。
- 后台任务面板展示 B 的 agent 名称、状态和最终答复，以及 B 或其同步后代启动的后台 shell 的归属；不展示 reasoning 或逐条工具调用。
- B 或其同步后代需要人工审批时，B 的 task 显示“等待审批”。用户通过现有审批界面批准或拒绝，独立处理对应审批批次；session 存在待审批项时不能提交新的 root 消息。
- 停止 B 的后台 task，一并取消 B、它的所有同步 subagent 后代及这些调用启动的后台 shell；同 session 的其他任务不受影响。

## Scope

In:
- 仅 root 调用 subagent 时可使用 `run_in_background`，默认仍为同步调用；stateful 和 stateless 均支持。
- 后台 task 的状态、最终答复、完成通知、读取与停止接入现有机制，保留后台 shell 的能力及归属展示。
- 后台 subagent 完成后以 task notice 通知直接调用方 root，并在 root 空闲、没有待审批项时触发后续处理。
- 后台执行中的审批进入 session 队列，标明 task、实际 agent 和同步调用路径；多个 task 可独立处理审批。

Out:
- 后台任务 reasoning、完整消息记录或逐条工具调用展示。
- 服务重启后恢复后台执行；改变现有同步委派和后台 shell 的默认行为。

## Constraints

- Root 的资格由 runtime 的实际 root thread 身份决定。非 root 不暴露后台委派参数，强行传入 `run_in_background: true` 必须返回工具错误，不能悄悄同步执行。
- Stateful 会话按父 thread + subagent 名称隔离；已有同步或后台调用未结束时，新调用立即报错、不排队。
- 后台审批只暂停对应执行，其他已运行工作可继续；session 有任意未决审批时，拒绝新的用户消息和自动 root notice turn。
- 拒绝审批作为该 tool call 的拒绝结果返回，由 subagent 继续处理或结束；停止 task 时移除其未决审批。
- 聊天区停止只取消当前 root turn 及其同步调用，后台任务独立存活。任务面板停止和 `task_kill` 才取消指定任务及其所拥有的工作。
- 浏览器断线不停止任务或丢失审批；服务重启后非终态后台任务标记为 `interrupted`，不恢复后台运行或审批。
- 后台 subagent 或其同步后代的任意 checkpoint 保存失败，必须结束所属后台执行并清理审批和等待；相关 thread 只有在中止状态成功持久化后才可复用，无关后台任务不被连带取消。
- 继续遵守现有 permission mode、workspace 强制审批规则和 shell allow/deny 规则。

## Success Criteria

- Root 可同步或后台调用其可用的 subagent，后台调用立即获得唯一 task ID；任何非 root 的后台委派都被拒绝。
- 后台 B 内部可进行多层同步委派，最终答复按 C → B → A 的调用关系返回。
- Root 在后台任务运行时可以继续工作、结束 turn，用户可继续对话；出现待审批项后，新消息提交被禁用。
- 面板准确展示后台 subagent 和关联 shell 的状态、归属及最终结果；多个审批在重连后仍可定向处理。
- 完成、失败、拒绝后结束或被停止的任务都有明确终态，后台 subagent 完成通知最终只交付一次。
- 停止后台 B 后，其同步后代、关联后台 shell 和未决审批全部清理，无孤儿执行，也不影响无关任务。
- B → 同步 C 中，C 保存待审批或工具执行 checkpoint 失败后，B 能进入明确失败终态并释放任务名额，无残留等待或审批，无关后台任务继续运行；中止清理失败时相关 thread 保持隔离，恢复或重启后不重放 C 的旧调用。
