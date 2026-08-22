# 基于 rquickjs 的 Programmatic Tool Calling

## Problem

在 Coda 现有 provider-agnostic agent runtime 中实现一个小范围 PTC MVP：模型一次生成 JavaScript，脚本可循环、并发、按条件调用内置 `fs.rs` 与 `todo.rs` 工具，只把最终筛选结果送回模型，从而减少 LLM 往返和中间结果占用的上下文。

## Scope

包含：

- 用 `rquickjs` 执行模型生成的 ES2020 JavaScript。
- 新建 `crates/coda_ptc`，隔离 JS engine、bridge protocol、资源限制和执行报告。
- 只把 `read_file`、`write_file`、`edit_file`、`ls`、`read_todos`、`write_todos` 以异步 JS 函数暴露给脚本。
- 每次 generation 取“固定六项 ∩ 当前 agent 已配置工具 ∩ 当前无需审批工具”，把 capability snapshot 作为隐藏执行元数据持久化到对应的 discovery/runner call；provider-visible 的 `run_javascript` 与 `list_javascript_tools` descriptor 本身保持固定。
- 复用现有工具参数校验、取消、权限、文件锁、thread state 和 artifact 机制。
- 给出安全边界、资源限制、持久化语义、测试与逐步实现方案。

不包含：

- 实现 Anthropic Messages API 的 `code_execution_20260120`、`allowed_callers`、`caller`、`container` 等原生 wire protocol。
- Python、bash、npm、Node.js API 或任意模块安装。
- 持久化/复用 JS heap，或在服务重启后恢复一个暂停的 JS continuation。
- 第一版从 JS 调用 `ask_user` 或 `agent__*` 子 agent。
- 第一版从 JS 调用 `shell`、`grep`、`glob` 或任意 MCP 工具。

这里实现的是 Anthropic 文档所说的 self-managed PTC 模式，不是 Anthropic 托管容器的兼容层。Coda 目前通过 OpenAI-compatible Chat Completions 工作；采用一个普通的 `run_javascript` client tool，能让这项能力不绑定 Claude API。

## Assumptions

- 目标是给 Coda 增加 provider-independent 的本地 PTC，而不是接入 Claude 托管 code execution。若目标其实是完整兼容 Claude 原生协议，需要另做 `coda_anthropic` provider adapter，本设计的数据流会明显不同。
- 首版每次 `run_javascript` 都创建独立、短生命周期的 QuickJS runtime。脚本之间不共享 JS 全局状态；需要持久化的数据仍由现有 `ThreadState` 所有。
- `run_javascript` 本身没有文件、网络、进程等 ambient authority；所有外部能力只能通过受控的工具 bridge 获得。
- MVP 不在 JS 内进入审批流程。`run_javascript` 与伴生 discovery 都是普通外层工具：permission mode 默认自动批准它们；若 workspace `approval_required` 命中，则在执行前走现有审批、暂停和恢复流程，而不是被解释为禁用。
- `run_javascript` 必须属于当前 agent 的配置，并且至少有一项 bridge 工具可注入时，固定 descriptor 的 `run_javascript` 与伴生 synthetic tool `list_javascript_tools` 才一起出现在 request tools 中。`list_javascript_tools` 不需要在 `AGENT.md` 中单独配置，也不能脱离 runner 单独启用。默认 root agent 下，`explore` 注入 `read_file`、`ls`、`read_todos`、`write_todos`；`accept_edits`/`yolo` 注入全部六项。
- workspace 若要禁用 PTC，应在 agent 的 `tools` 配置中省略 `run_javascript`；`approval_required` 只表示外层调用需要人工批准，不兼任 disable 开关。MVP 不另增 `enabled = false`。
- MVP 的每一项 JS bridge capability 也以同名普通 tool descriptor 出现在 provider request 中；因此 discovery 只需返回可用名称，模型从 request tools 读取对应 description 和 input schema。未来若引入 bridge-only capability，再单独扩展 discovery wire shape。
- capability snapshot 在构造 LLM request 时确定，绝不从模型参数接收；provider 返回 discovery 或 runner call 后，driver 把 snapshot 绑定为隐藏执行元数据。它必须跟随自动执行队列、待审批队列和 checkpoint 持久化，跨暂停、mode 变化和进程重启保持不变。
- 每个 bridge call 都重新检查“该名称在 generation snapshot 中，且当前完整 policy 仍无需审批”。实时检查只允许继续收缩，不能扩张：mode 或 workspace rules 中途收紧后，新调用返回 `TOOL_UNAVAILABLE`；反向放宽也不会让当前脚本得到 generation 时未暴露的工具。
- 主 Tokio runtime 上的 supervisor 是唯一权威 wall-clock watchdog。它能从 worker 线程外设置 atomic interrupt flag 并取消 script/host-call token；worker 本地 select 只负责在 Promise 或 host await 正常被 poll 时快速响应，不负责唤醒 CPU 死循环。
- QuickJS heap limit 不覆盖 Rust host 侧的 state 和 artifacts。PTC 另设 script-scoped 累计预算；每个嵌套调用先在隔离的 child context 中暂存 effects，成功才合并，失败或取消则丢弃。
- 顺序嵌套调用共享一次外层 tool call 的 `ThreadState`，因此后一次能看到前一次的写入；并发调用同一个 state key 的结果取决于实际写入顺序，应避免这种用法并在文档中明确。

## Validation Findings

### Claude PTC 的必要语义

[Anthropic PTC 文档](https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling)描述的核心不是特定容器协议，而是：代码调用异步工具，执行在等待工具结果时暂停，结果回到代码而不进入模型上下文，代码结束后只有最终输出交给模型。工具函数接收一个 object/dict 并返回文本字符串，支持循环、条件和并发。

设计含义：Coda 不需要复制 `container` wire protocol 才能获得 PTC 的主要收益；只要 `run_javascript` 的一次普通 tool call 内部完成这条闭环即可。

### rquickjs 能覆盖 async bridge

当前稳定版本是 `rquickjs 0.12.2`，最低 Rust 版本为 1.87，低于本项目固定的 Rust 1.95。其 [`futures` 支持](https://docs.rs/rquickjs/latest/rquickjs/)可把 Rust Future 暴露为 ES Promise，也可把 JS Promise 作为 Rust Future 等待；[`Ctx::eval_promise`](https://docs.rs/rquickjs/latest/rquickjs/context/struct.Ctx.html)支持 top-level await；[`function::Async`](https://docs.rs/rquickjs/latest/rquickjs/function/struct.Async.html)可把返回 Future 的 Rust closure 注册成 JS 函数。

设计含义：JS 中的 `await tools.read_file({...})` 可以自然地暂停 QuickJS job，等待现有 `ToolObject::execute` Future 完成，不需要手写 continuation 状态机。

### rquickjs 提供必要的进程内资源阀门

[`AsyncRuntime`](https://docs.rs/rquickjs/latest/rquickjs/struct.AsyncRuntime.html)支持 memory limit、stack limit 和 interrupt handler。engine 正在执行代码时，interrupt handler 返回 `true` 会终止解释器，可用于打断 CPU deadline 和取消；memory limit 在启用 `rust-alloc`/custom allocator 时是 no-op。

interrupt handler 只会在 engine 正在执行 JS 指令时被调用；它不能唤醒 `await new Promise(() => {})`，也不能单独终止等待 host Future 的状态。

设计含义：依赖应使用 `default-features = false, features = ["std", "futures"]`，不要启用 `rust-alloc`、`loader`、`dyn-load`。`parallel` 在 rquickjs 文档中仍标为 experimental，因此首版把 runtime 固定在专用线程，而不是让 `AsyncContext` 跨 Tokio worker 移动。wall-clock 限制必须由两层共同实现：主 Tokio runtime 的 supervisor/watchdog 到时后从 worker 外设置 interrupt flag 并取消 token，保证 CPU 死循环也能被通知；worker 本地 select 处理 pending Promise 和 bridge await。

### 当前 Coda 的接入点

- `ToolObject::execute` 已统一承担 JSON 参数反序列化、异步执行和字符串输出；PTC bridge 应调用它，而不是给每个工具另写 adapter。
- `AgentDriver::handle_generation` 生成 tool descriptors 并用 `ToolApprovalMode` 分流审批；嵌套调用必须复用相同 predicate，否则 `shell` deny、workspace `approval_required` 和 permission mode 会被绕过。
- 当前待执行调用只持久化 `ToolCall + outcome`，待审批队列甚至只保存 `ToolCall`。capability snapshot 若不随 discovery/runner call 经过两条队列持久化，同批其他工具触发审批或进程重启后只能重新计算，并可能在 policy 放宽时扩大权限。
- 每批普通工具目前共享 committed state snapshot、各自记录写入。PTC 可把整个脚本视为一个外层调用，但每个嵌套调用必须使用隔离的 child context；成功 child 才把 state/artifacts 合并到 script scope，最终统一锚定到 `run_javascript` 的 `ToolMessage`。
- file tools 已通过 `ToolCallContext` 记录 file-diff artifacts；PTC 内部调用应继续复用该机制，无需为 MVP 新增消息或 artifact 类型。
- `ToolWrapper::execute` 当前会把完整 JSON input/output 写入 tracing span。PTC 会放大大文件内容和待写内容进入日志的风险，因此安全摘要必须在 bridge 接入前完成，不能推迟到 helper-process hardening。
- `ToolCallContext::record_artifact` 当前没有容量限制，而 `write_file`/`edit_file` 在文件落盘后才生成并记录完整 patch。`CallState` 也会把同一 key 的每次完整写入依次追加。两者都会绕过 QuickJS heap 和 bridge-result budget；artifact budget 必须在任何文件 mutation 前原子预留，script state 则按 key 保留最终值。

### 收缩范围后仍需保留鉴权

`PermissionMode::Explore` 已自动批准 `read_file`、`ls`、`read_todos`、`write_todos`；`AcceptEdits` 再增加 `write_file`、`edit_file`，`Yolo` 包含全部六项。workspace 的 `[permissions.tools].approval_required` 会继续收紧所有 mode，包括 `yolo`。此外 `PermissionModeCell` 是实时可变的。

设计含义：MVP 可以完全不实现“JS 内部暂停等待审批”的控制流，但不能删除权限检查。generation 时计算 `eligible_tools = fixed_tools ∩ agent_tools ∩ policy_auto_approved_tools` 并保存 snapshot；`list_javascript_tools` 返回 snapshot 与执行时 policy 交集中的名称，bridge 每次调用仍检查 snapshot 和当前 policy。前者尊重 `AGENT.md` 的显式 tool 配置、给模型准确的能力视图并限定权限上界，后者处理 mode 中途收紧。外层 `run_javascript` 仍可在启动脚本前走现有审批。

这六项都不是 `shell`，其现有审批判断只依赖工具名，不依赖具体参数。因此可以在 generation 前可靠计算可用子集；将来一旦加入 `shell`，就必须恢复按具体 arguments 判定。

没有进行依赖安装或可执行 spike；这些应成为实现第一步，以真实验证 rquickjs、Tokio 与现有 tool Future 的组合行为。

## Proposed Flow

```text
LLM
  -> list_javascript_tools({})
     -> generation snapshot ∩ current workspace policy
     -> available tool names
  -> LLM writes code using the discovered APIs
  -> run_javascript({ code })
     -> optional outer PendingApproval (before worker starts)
     -> main Tokio supervisor starts authoritative watchdog
        -> deadline: set atomic interrupt flag + cancel script/host calls
     -> QuickJS worker thread
        -> await tools.read_file({ path })
           -> bounded bridge channel
              -> persisted generation snapshot + current workspace policy check
              -> existing ToolObject::execute on Coda's Tokio runtime
              -> raw string result through oneshot
        -> loop/filter/Promise.all inside JS
     -> one bounded final result + existing file-diff artifacts
  -> LLM receives only the outer run_javascript result
```

discovery 会多一次 provider round trip，但其 tool definition 固定，结果进入普通 history 后也可被后续 generation 的 prefix cache 复用。它是能力提示而不是授权凭据：下一次 generation 会产生新的 runner snapshot；若 policy 在 discovery 后收紧，runner/bridge 仍以新 snapshot 和实时检查为准，脚本可能收到 `TOOL_UNAVAILABLE`。

JS 面向模型的接口：

```js
const raw = await tools.read_file({ path: "README.md" });
const matches = (await Promise.all(paths.map(async (path) => {
  const text = await tools.read_file({ path });
  return text.includes("needle") ? path : null;
}))).filter(Boolean);
return { matches };
```

每个函数只接受一个 JSON object，返回原始字符串，与 Anthropic PTC 的工具函数约定一致。需要结构化数据时脚本显式调用 `JSON.parse`。工具结果永远作为 JS string 注入，不拼进源代码，也不自动 `eval`。

### rquickjs API 映射

实现骨架应沿这条路径走；下面是 API 关系示意，不是可直接编译的最终代码：

```rust
// Main Tokio runtime, not the worker-local executor.
let watchdog = tokio::spawn(async move {
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => {
            deadline_exceeded.store(true, Ordering::Release);
            interrupt_requested.store(true, Ordering::Release);
            script_cancel.cancel();
        }
        _ = completed.cancelled() => {}
    }
});

// Dedicated worker thread.
let runtime = rquickjs::AsyncRuntime::new()?;
runtime.set_memory_limit(limits.heap_bytes).await;
runtime.set_max_stack_size(limits.stack_bytes).await;
runtime
    .set_interrupt_handler(Some(Box::new(move || {
        interrupt_requested.load(Ordering::Acquire)
    })))
    .await;

let context = rquickjs::AsyncContext::custom::<RequiredIntrinsics>(&runtime).await?;
context
    .async_with(async move |ctx| {
        let bridge = rquickjs::Function::new(
            ctx.clone(),
            rquickjs::function::Async(move |name: String, arguments: String| {
                let bridge_tx = bridge_tx.clone();
                async move { request_host_call(bridge_tx, name, arguments).await }
            }),
        )?;
        ctx.globals().set("__coda_call_tool", bridge)?;

        install_frozen_tools_object(&ctx, exposed_tool_names)?;
        let promise = ctx.eval_promise(wrap_in_async_function(source))?;
        tokio::select! {
            result = promise.into_future::<String>() => result,
            // Responsiveness path for pending Promise/host await. The external
            // watchdog above remains authoritative for CPU-bound JS.
            _ = script_cancel.cancelled() => {
                if deadline_exceeded.load(Ordering::Acquire) {
                    Err(JsError::DeadlineExceeded)
                } else {
                    Err(JsError::Aborted)
                }
            },
        }
    })
    .await
```

`__coda_call_tool` 只跨 channel 发送 `{name, arguments_json, sequence}`，不直接在 QuickJS 线程执行 Coda 工具。JS bootstrap 只为持久化的 generation snapshot 建立并冻结 `tools` object；每个属性函数先 `JSON.stringify(input)`，再 await bridge。wrapper 把用户代码的最终值安全地序列化成有界 JSON report，并收集有界 `console.log`。

主 Tokio runtime 上的 supervisor/watchdog 独立于 worker 调度。wall-clock budget 从进入 executor、排队等待 worker permit 时就开始计算；permit 获取本身同时受 deadline 和用户取消约束。取得 permit 后，到达同一个绝对 deadline 或收到用户取消时设置 interrupt flag、取消 script token、停止接收新的 bridge request，并取消所有 in-flight host-call child token。interrupt flag 负责让 CPU 循环交还控制权；worker local select 观察同一个 token，让 pending Promise/bridge await 及时结束。worker 获得一个全局并发 permit，并持有到线程真正退出，所以无法 teardown 的线程不会释放容量、继而无限堆积，但后续调用仍能按 deadline/cancel 从排队中退出。

worker 只接受所有已发起的 tool Promise 都已 settle 的正常返回。脚本返回时若仍有 bridge call 未完成，结果改为 `UNAWAITED_TOOL_CALLS`，随后取消这些调用；不把一个可能留下部分外部副作用、却缺少对应 result/artifact 的执行记为 `ok: true`。

supervisor 只在有限 grace period 内等待 context/runtime drop 和线程 join。超过上限时外层返回 `WORKER_UNRESPONSIVE` 并 detach 线程，记录 error metric；这限制调用方等待时间，但不能强杀卡在 native engine 内的线程。严格的进程级 wall-clock 和资源回收只能由 helper process/OS sandbox 提供。已开始的文件副作用与当前 abort 语义一样不自动回滚，超时后到达的 bridge response 被丢弃。

## Alternatives Considered

### 直接使用 Claude 原生 PTC

优点是 Anthropic 托管 sandbox、容器续用和原生 token accounting 都已完成。缺点是绑定 Claude Messages API，与 Coda 当前的 OpenAI-compatible provider abstraction、消息模型和本地工具执行链不兼容；`rquickjs` 在这条路径上没有作用。

结论：若产品目标是“Claude 专属能力”，这是更短的路径；若目标是“所有 provider 都能用的 Coda runtime 能力”，采用本地 `run_javascript`。

### 把 rquickjs 直接放在 Tokio worker 上并启用 `parallel`

代码更少，但 `parallel` 仍被上游标为 experimental，CPU 密集或死循环脚本也会占住 Tokio worker。

### 脚本返回后继续 drain 未 await 的工具调用

这可以尽量收集已经启动的调用结果与 artifacts，但会让 `return` 不再代表脚本执行结束，还会掩盖模型漏写 `await` 的错误；卡住的调用也会消耗剩余 deadline。选择 fail closed：检测到未完成调用就返回 `UNAWAITED_TOOL_CALLS` 并取消，提示调用方显式 await 每个 Promise。

结论：不用。QuickJS runtime 留在专用 OS 线程；现有工具仍在主 Tokio runtime 执行，两边通过有界 channel + oneshot 通信。

### 直接把实现放进 `coda_tools`

优点是少一个 crate，`run_javascript` 表面上也是一个 built-in tool。缺点是 rquickjs 原生依赖、专用线程 runtime、bridge protocol、资源预算和未来进程隔离都比普通工具复杂得多；它们会让 `coda_tools` 同时负责工具实现和一套代码执行 runtime。

结论：不用。新建 `coda_ptc` 作为深模块；`coda_tools` 只提供轻量 `ToolSpec` 注册适配，现有 fs/todos 工具保持不依赖 PTC。

### 用外部 helper process / 容器运行 rquickjs

能隔离 QuickJS C engine 漏洞和进程级崩溃，安全边界明显更强；代价是 IPC、部署、进程池、崩溃恢复和 artifact/state 回传更复杂。

结论：把 bridge 接口设计成消息协议，为以后迁移保留边界；本地开发型 agent 的首版先采用专用线程。若 Coda 要执行多租户、不可信用户生成的代码，应在正式开放前把 executor 移到受 OS sandbox 约束的 helper process，而不是把进程内 QuickJS 宣称成安全 sandbox。

### 执行时重新计算 capability set

无需扩展 checkpoint 类型，但同一批其他工具触发审批、session release 或进程重启后，执行时的 policy 可能已经变化。重新计算会让放宽后的 policy 给脚本增加 generation 时 snapshot 中不存在的能力，也无法证明执行的正是模型生成代码时允许使用的权限上界。

结论：不用。request 构造时生成 capability snapshot，模型返回 call 后将其绑定为不可由模型修改的隐藏元数据；实时 policy 只与 snapshot 求交集。

### 把 available APIs 编进 `run_javascript` descriptor

优点是模型在同一次 generation 就知道精确 capability，无需额外调用。缺点是 permission mode、workspace policy、agent tool set 或 bridge catalog 改变都会改写 runner descriptor，破坏原本稳定的 request prefix；随着可编程工具增多，runner description 还会持续膨胀。

结论：改为固定的 runner descriptor，并自动提供固定 descriptor 的 `list_javascript_tools`。discovery 返回当前可用名称，接受首次使用通常多一次 LLM round trip。结果进入普通 history，后续 generation 可复用；授权仍由下一次 runner snapshot 和 bridge 实时检查决定。

### discovery 返回完整 definitions，而不是只返回名称

完整 definition 可独立描述 bridge-only capability，但会把 description 和 schema 重复写入普通 ToolMessage、长期占用 history，还需要更大的独立 result budget。当前六项 capability 已经全部以同名直接工具出现在 provider request 中，模型只缺少“哪些名称可从 JS 使用”这一信息。

结论：只返回有序名称。新增一个经过 allowlist/policy 的 bridge 工具后，其名称自动进入 discovery；description 和 input schema 继续复用同名直接工具 descriptor。接受暂不支持 bridge-only capability，避免为未出现的需求扩大 wire shape 和 history 占用。

### 让嵌套调用触发现有审批 UI，并在批准后恢复 JS

体验最接近 Anthropic 容器暂停，但 QuickJS continuation 不能由当前 snapshot 序列化，服务重启或 session release 后无法可靠恢复；现有 `Suspended` 语义会退出 agent run，也不适合卡在一个 `ToolObject` Future 内。

结论：MVP 不实现嵌套审批 UI。需要审批的普通工具只从本次 JS capability set 中移除，不影响其他 eligible tools。bridge 每次重新检查；某工具在执行前变得不可用时 reject Promise，错误码为 `TOOL_UNAVAILABLE`，message 同时列出本次脚本仍可用的工具。外层 `run_javascript` 自身若需要审批，则在 worker 创建前照常走现有流程；模型也可在下一轮直接调用普通工具并走原有审批。

### 把每个嵌套调用写成普通 ToolMessage

审计最直接，但 provider adapter 会把这些中间结果重新送入模型，直接失去 PTC 的主要 token 优势。

结论：MVP 不持久化嵌套调用消息或结果；provider-visible history 中只保留外层 `run_javascript` 的一个 ToolMessage。嵌套调用使用 tracing 记录有界诊断信息，已有 file-diff artifacts 聚合到外层 ToolMessage。

## Components

### `coda_ptc` crate

唯一负责 PTC 领域逻辑的 crate：rquickjs runtime 生命周期、JS bootstrap、worker/host bridge、资源限制、typed errors、固定的 runner/discovery descriptors、discovery result wire shape 和最终 report。它依赖 `coda_core`，但不依赖 `coda_agent`、`coda_server` 或 permission mode。

建议内部结构：

```text
crates/coda_ptc/
  Cargo.toml
  src/
    lib.rs        # public API、limits、request/report/error types
    engine.rs     # rquickjs runtime/context/promise lifecycle
    bridge.rs     # bounded request/response protocol
    tool.rs       # RunJavaScriptTool + stable runner/discovery descriptors
    bootstrap.js  # frozen tools object、console、result serialization
```

### `RunJavaScriptTool`（`coda_ptc`）

定义 `run_javascript({code})`、校验源码大小、从外层 `ToolCallContext` 取得 invoker、创建有预算的 `HostCallScope` 并调用 executor，最后把 report 作为外层工具结果返回。每个 bridge call 都从 scope 派生不含 invoker 的 child context；成功 child 只合并进 scope，失败 child effects 丢弃。`RunJavaScriptTool` 即将返回 `ToolResult::Ok(report)` 时才调用一次 `scope.commit_into_outer()`；返回任何 outer `ToolError` 时直接 drop scope。

### `JsExecutor`（`coda_ptc::engine`）

在主 Tokio runtime 启动权威 watchdog，并在专用线程的 current-thread executor 内创建短生命周期的 `AsyncRuntime`/`AsyncContext`。watchdog 从 worker 外驱动 deadline flag/token；worker 安装最小 JS intrinsics、`console` 和工具 bridge，执行 wrapper、响应 cancellation、teardown 和有上限的 join，并生成 `JsRunReport`。它不认识 Coda permission mode 或具体工具。

### `HostToolInvoker`（`coda_core::tool` trait）

这是 executor 唯一能触达宿主能力的通用接口。中性的 `HostToolCallResult`/`HostToolCallError` 与 trait 一起属于 `coda_core`，不引用 `coda_ptc`；PTC-specific limits、report、engine error 和 JS error 映射留在 `coda_ptc`。实现由 agent runtime 创建，只捕获当前 agent 的 `Tools`、实时审批 predicate 和持久化的 generation snapshot，不捕获父 context 或其 handles。受限 child `ToolCallContext` 由 caller 显式传给 `call`。

### `RunJavaScriptToolSpec`（`coda_tools`）

只负责把 `coda_ptc::RunJavaScriptTool` 注册进现有 built-in tool catalog，使 `AGENT.md` 的工具解析、冲突校验和默认 root tool set 沿用现有机制。`coda_tools` 不包含 QuickJS 执行逻辑。

### `list_javascript_tools` synthetic tool（`coda_agent` + `coda_ptc`）

`coda_ptc` 定义固定的 provider-visible descriptor、`{"available_tools":[...]}` result 格式，以及 discovery result 和 `TOOL_UNAVAILABLE` message 各 16 KiB 的上限；`coda_agent` 在当前 agent 配置了 runner 且 capability snapshot 非空时自动注入它，并返回 `snapshot ∩ current policy` 的有序名称。generation 构造 snapshot 时先用两个 formatter 验证完整列表及最长 requested name 都能放入各自上限；超限则 fail closed，记录错误并同时省略 discovery/runner。执行时的可用集合只是 snapshot 子集，因此两种输出都不会意外越界。结果和错误列表都不允许截断，因为不完整列表会误导模型。

它不是普通 `ToolSpec`，不需要也不允许在 `AGENT.md` 中单独配置，不会被注入 JS bridge。`LIST_JAVASCRIPT_TOOLS_TOOL_NAME` 同时进入 `coda_tools` 导出的全局 synthetic reserved-name 集合；`AgentTeam::new` 对每个 agent 无条件拒绝同名 `ToolSpec`，无论 runner 是否启用。这样同名 prebuilt/custom tool 既不能产生重复 descriptor，也不能借 permission mode 对 synthetic name 的 auto-approve 获得普通工具权限。

driver 的 special-case executor 是 model-input trust boundary：只接受 JSON 空对象 `{}`（允许空白，不接受 `null`、array、非 object 或任何属性），错误参数按普通 `InvalidParameters` 结束。它复用 local tool settlement 路径，产生一致的 ToolCallStart/ToolCallEnd、ToolMessage、outcome、started-at/duration 和空 artifacts；由于不经过 `ToolWrapper`，另建只记录 tool name、input/output byte length、status、duration 和 error category 的安全 tracing span，绝不记录 raw arguments/result。permission mode 默认自动批准，workspace `approval_required` 仍可要求外层审批。

### `AgentToolInvoker`（`coda_agent`）

构造 LLM request 时先确认当前 agent 配置了 `run_javascript`，再按 agent registry 和 policy 逐项过滤固定 capability family，生成 `exposed_tools`；为空时不提供 runner/discovery descriptors。driver 保留这个 snapshot，provider 返回任一 synthetic call 后都将其绑定到隐藏执行元数据，再进入普通 approval partition。批准后仍使用原 snapshot。discovery 执行时只返回 `snapshot ∩ current policy` 的有序名称；runner 执行时只接受 snapshot 中的名称，并重新用当前 predicate 检查该工具。名称从未注入或当前已需审批时返回 `TOOL_UNAVAILABLE`，message 同时列出对本次脚本仍可用的 `snapshot ∩ current policy` 名称。没有匹配到当前 generation descriptor、因而没有 snapshot 的伪造或过期 discovery/runner call fail closed 为 `PTC_UNAVAILABLE`。

## Interfaces

以下是调用侧接口草图，不限定内部实现细节。

```rust
/// Trust boundary: receives model-generated source and may request host tools.
pub async fn execute_javascript(
    source: String,
    host: Arc<dyn HostToolInvoker>,
    cancel: CancellationToken,
    limits: JsLimits,
) -> JsRunReport;

// coda_core: neutral host boundary; it must not reference coda_ptc types.
pub trait HostToolInvoker: Send + Sync {
    /// Returns the generation snapshot names installed in the JS runtime.
    /// Authorization is repeated on every call and may further shrink it.
    fn exposed_tools(&self) -> Arc<[String]>;

    /// Trust boundary: validates name/JSON/limits/policy, then executes one
    /// existing tool. Success is its raw textual output; failures are typed so
    /// JS can distinguish disabled capability, validation, execution, limit
    /// and abort. This method never decides whether staged child effects commit.
    fn call(
        &self,
        name: String,
        arguments_json: String,
        // Created by HostCallScope: shares staged state/artifacts and budget,
        // uses a child token, and always has invoker = None.
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = HostToolCallResult> + Send>>;
}

pub struct HostEffectLimits {
    pub state_bytes: usize,
    pub artifact_bytes: usize,
}

/// Owns one script's cumulative host-effect accounting. It references the
/// outer context's state/artifact sinks but never carries an invoker.
pub struct HostCallScope { /* private */ }

/// One host tool call's isolated effects. Drop rolls them back and releases its
/// reservations; commit merges them into the script scope.
pub struct StagedToolCall {
    context: ToolCallContext,
    /* private commit guard */
}

impl ToolCallContext {
    /// Driver-only construction path for the outer run_javascript call.
    pub fn with_host_invoker(self, invoker: Arc<dyn HostToolInvoker>) -> Self;

    /// Returns None for every ordinary and host tool context.
    pub fn host_invoker(&self) -> Option<Arc<dyn HostToolInvoker>>;

    /// Extracts only state/artifact sinks into the scope; it does not clone the
    /// outer context or its invoker.
    pub fn host_call_scope(&self, limits: HostEffectLimits) -> HostCallScope;

    /// Atomically reserves the exact retained size and stages the artifact.
    /// The context returns ResourceLimit without recording it when over budget.
    pub fn record_artifact(&self, artifact: ToolArtifact) -> ToolResult<()>;
}

impl HostCallScope {
    /// Shares the scope's successful state/artifact view and cumulative budget,
    /// uses `cancel`, and deliberately strips the parent's invoker capability.
    pub fn begin_tool_call(&self, cancel: CancellationToken) -> StagedToolCall;

    /// Drains the scope's final state map and accumulated artifacts into the
    /// outer context exactly once. Infallible after successful reservations;
    /// consuming self prevents a second commit.
    pub fn commit_into_outer(self);
}

impl StagedToolCall {
    /// A clone passed by value to HostToolInvoker::call while this guard stays
    /// with the bridge until the future settles.
    pub fn context(&self) -> ToolCallContext;

    /// Called only after HostToolInvoker::call succeeds; infallible because all
    /// state/artifact bytes were reserved when staged.
    pub fn commit(self);
}

pub trait ThreadState: Send + Sync {
    fn get(&self, kind: &str) -> Option<serde_json::Value>;
    /// PTC's staged implementation enforces the state byte budget here and
    /// stores only the latest value per key.
    fn set(&self, kind: &str, value: serde_json::Value) -> Result<(), HostEffectError>;
}

pub struct HostToolCallResult {
    pub output: String,
}

pub enum HostToolCallError {
    Unavailable {
        requested: String,
        available: Vec<String>,
    },
    InvalidParameters(String),
    Execution(String),
    ResourceLimit(String),
    Aborted(String),
}

// coda_ptc: both descriptors are stable across capability changes.
pub fn run_javascript_definition() -> ToolDefinition;
pub fn list_javascript_tools_definition() -> ToolDefinition;

pub const DISCOVERY_RESULT_BYTES: usize = 16 * 1024;
pub const TOOL_UNAVAILABLE_MESSAGE_BYTES: usize = 16 * 1024;

/// Encodes all names or fails without producing a partial capability list.
pub fn available_tools_result(names: &[String]) -> Result<String, CapabilityMessageLimitError>;

/// Includes every currently available name or fails; it never expands beyond
/// the generation snapshot and never emits a partial list.
pub fn tool_unavailable_message(
    requested: &str,
    available: &[String],
) -> Result<String, CapabilityMessageLimitError>;

// coda_tools: names injected outside ToolSpec/ToolObject dispatch.
pub const SYNTHETIC_RESERVED_TOOL_NAMES: &[&str] = &[LIST_JAVASCRIPT_TOOLS_TOOL_NAME];

// AgentTeam::new rejects a ToolSpec using any synthetic reserved name before
// a provider request or permission decision can observe it.
BuildError::ReservedToolName { agent: String, name: String };

// ToolError gains ResourceLimit(String). HostToolInvoker maps it to
// HostToolCallError::ResourceLimit without erasing the category.

pub struct JsRunReport {
    pub ok: bool,
    pub value: Option<serde_json::Value>,
    pub error: Option<JsErrorReport>,
    pub stdout: String,
    pub stdout_truncated: bool,
    pub completed_calls: usize,
}

// coda_ptc: engine/bridge failures mapped into a bounded JsRunReport.
pub enum JsRunError {
    Syntax(String),
    Exception(String),
    ToolCall(HostToolCallError),
    LimitExceeded(String),
    DeadlineExceeded,
    WorkerUnresponsive,
    Aborted,
}
```

bridge 的单次调用顺序固定为：`scope.begin_tool_call(child_token)` → `host.call(name, args, staged_call.context()).await` → 校验单次及累计 result budget → 仅在全部成功时 `staged_call.commit()`。这里的 commit 只写入 scope，不触达外层 `CallState`/artifact sink；host 返回错误或 result 超限都会 drop guard，丢弃该 child 的 staged effects。executor 结束后，`RunJavaScriptTool` 根据准备返回的外层 `ToolResult` 决定是否调用一次 `scope.commit_into_outer()`。`AgentToolInvoker` 从不持有 `staged_call`、scope 或父 context，也无权决定 commit；worker/bridge 持有 child commit guard，host Future 只拿受限 context clone，因此所有权关系闭合且没有递归 capability。

`JavaScriptTool::execute` 对普通语法错误、JS exception、deadline 和工具错误返回一个可解析的文本 report，并明确已经完成多少次嵌套调用。只有用户取消映射为 `ToolError::Aborted`，与现有 turn cancellation 语义一致。

## Data Model

### 外层历史

模型和 provider 只看到：

1. 可选的 `list_javascript_tools({})` ToolCall 及其 ToolMessage；结果是当前有序名称列表，description 和 input schema 来自 request 中的同名直接工具。
2. Assistant 调用 `run_javascript`，参数是源码。
3. 一个回答该 call id 的 ToolMessage，内容是最终 `JsRunReport` 的精简文本/JSON。

中间工具输出不创建 provider-visible Message。

### 隐藏执行元数据

generation capability snapshot 属于 agent scheduler/checkpoint，而不是 provider-visible message，也不是模型可提交的 tool arguments：

```rust
pub struct PreparedToolCall {
    pub tool_call: ToolCall,
    pub execution: ToolExecutionMetadata,
}

pub enum ToolExecutionMetadata {
    None,
    Programmatic {
        exposed_tools: BTreeSet<String>,
    },
}

pub struct PendingToolCall {
    pub prepared: PreparedToolCall,
    pub outcome: ToolCallOutcome,
}
```

`ResumePoint::PendingApproval.pending_approval_calls` 改为 `VecDeque<PreparedToolCall>`，自动执行队列继续保存 `PendingToolCall`；两者的 `Stored*` 形式都序列化 `ToolExecutionMetadata`。`PendingApproval` event 和 web UI 只投影公开的 `ToolCall`，不允许用户或模型改写 snapshot。审批通过只是增加 `ToolCallOutcome::Approved`，不重新生成元数据。项目允许持久化格式 breaking change，因此不增加兼容 shim。

### State 和 artifacts

`RunJavaScriptTool` 从外层 context 建立一个 script-scoped `HostCallScope`。它拥有累计预算和对外层 state/artifact sinks 的引用，但不含 invoker。每个 bridge call 再得到独立的 `StagedToolCall`：

- child context 使用 host-call child token，`invoker = None`，因此普通嵌套工具不能递归调用工具，也不存在 `context → invoker → context` 的引用环。
- child state/artifacts 先隔离暂存。host tool 成功后 bridge 调用 `StagedToolCall::commit()`，只把 effects 合并进 scope；验证失败、执行失败、超时或取消时 drop guard，丢弃该 child effects 并释放预留预算。
- scope 内部用 map 保存 state 的最终值并用 vector 累积 artifacts；它在整个脚本期间不调用外层 `ThreadState::set` 或 `record_artifact`。已成功 child commit 的顺序调用对下一次可见；同一个 state key 只保留最后一个完整值，并发写同 key 仍按实际 child commit 顺序决定最终值，不承诺确定顺序。
- artifact 和 state 在暂存时就原子预留 script 累计预算；commit 因此不再失败。并发 child 的在途 reservation 也计入预算，避免 16 个调用同时越界。
- `RunJavaScriptTool` 返回 `ToolResult::Ok(report)` 前调用一次 `scope.commit_into_outer()`，把最终 state map 的每个 key 只写一次，并批量转移 artifacts。语法错误、普通 JS exception、deadline、bridge/tool error 等若按可解析的 `JsRunReport` 返回，仍属于 `Ok(report)`，所以保留此前已完成调用的 effects。
- 用户取消映射为 outer `ToolError::Aborted`；初始化失败或其他 outer `ToolError` 也不返回 report。这些路径直接 drop scope，不写入外层 state/artifact sink。文件等已经发生的外部副作用与当前被 abort/failed 的普通工具一样不能自动回滚。

file tools 必须调整调用顺序：`write_file` 在 `create_dir_all`/create/write 前构造并 `record_artifact(...)?`；`edit_file` 在持有现有文件锁、完成 read/replace 和构造 patch 后，但在 seek/truncate/write 前调用它。预算不足因此在首次文件 mutation 前 fail closed。后续 IO 若失败，child guard 会丢弃预记录的 artifact；已发生的部分 IO 仍遵循现有不可回滚语义。`write_todos` 的 `state.set(...)?` 同样在成功返回前检查预算。

## Load-Bearing Decisions

### provider-independent synthetic tool，而不是 provider protocol 扩展

选择 `run_javascript` 普通 client tool。接受的代价是不能得到 Anthropic 对 programmatic result 的专属计费豁免，也没有原生 container reuse；换来所有 OpenAI-compatible 模型可使用，且不污染 `LLMProvider` 的通用消息模型。

### 每次执行一个 runtime

选择 ephemeral runtime。它避免跨 turn 的隐藏 mutable state、TTL、内存泄漏、租户串扰和服务重启一致性问题。QuickJS 的启动成本很低，PTC 的收益主要来自省掉 LLM round trips 和大结果上下文，不依赖 heap 复用。

### 工具调用留在主 Tokio runtime

QuickJS 专用线程只解释 JS，通过有界 channel 发 `ProgrammaticCallRequest`；`ToolObject::execute` 仍在现有 Tokio runtime 执行。这样不需要 rquickjs `parallel`，不把现有文件/todo 工具迁到另一套 runtime，也为未来改成 helper process 保留协议边界。

### PTC engine 独立成 crate，policy 留在集成层

`coda_ptc` 只认识 `HostToolInvoker` 和 provider-agnostic tool descriptors，不认识 `PermissionMode`、workspace config 或 `Agent`。eligible subset 由 `coda_agent` 使用现有 approval predicate 计算。接受的代价是 `coda_core::ToolCallContext` 需要一个可选 invoker capability；换来依赖方向单向，且以后把 QuickJS worker 移到 helper process 时无需改 agent policy。

### 每次嵌套调用重新鉴权

`run_javascript` 与 `list_javascript_tools` 加入 `Explore` 的 auto-approved 列表，因此 `AcceptEdits`/`Yolo` 也自然包含它们；workspace `approval_required` 仍可要求对这两个外层调用人工审批。固定六项则逐个通过完整 approval predicate 过滤：命中 tightening 的某一项只移除该项，不影响其余工具。每个 bridge call 都重新检查 generation snapshot 与当前 policy。`TOOL_UNAVAILABLE` 的 message 列出本次 snapshot 中仍通过实时 policy 的名称，使模型可直接调整脚本；它绝不列出 snapshot 外后来放宽的能力。审批规则是硬边界，JS 全局对象或模型提示不是。

这意味着 MVP 没有“脚本内部暂停—打开审批 UI—恢复 continuation”的问题，但外层 runner 在启动前仍可正常审批，而且内部仍然有 authorization 问题。三者不能混为一谈。

### capability snapshot 固定权限上界

snapshot 在 request 构造时产生，但不再编码进 descriptor。provider 返回 discovery 或 runner call 后，snapshot 随调用经过审批、checkpoint 和重启；执行时的有效集合始终是 `generation_snapshot ∩ current_auto_approved_tools`。discovery 结果只描述它自己的 snapshot；下一次 generation 的 runner 会得到新 snapshot，因此提示与后续执行之间允许继续收缩，安全上不允许执行中的脚本扩张。接受这段时序差异，换取稳定 descriptor 和更好的 prefix cache 命中。

### 固定 descriptor + 显式 discovery

`run_javascript` 只描述稳定的 JS 环境、调用约定、错误和 runtime limits，不再列举 capability。伴生的 `list_javascript_tools` descriptor 同样固定，结果只承载当前可用名称；对应 description/schema 复用 request 中的同名直接工具。相比动态 runner descriptor，这通常让同一 provider/model/config 下的 request tool prefix 保持稳定，也让新增 allowlisted bridge tool 自动出现在 discovery 中；代价是模型首次使用或需要刷新 capability 时多一次 round trip。这个调整只提高 API presentation 的扩展性，不把任意工具自动变成安全的 bridge capability：固定 allowlist、agent 配置、policy 和递归隔离仍是接入门槛。

### synthetic 名称是全局保留名

选择在 `AgentTeam::new` 的唯一 validation gate 无条件拒绝任何名为 `list_javascript_tools` 的普通 `ToolSpec`，而不是让 permission predicate 区分 synthetic identity。后者需要把 identity 贯穿 `ToolCall`、wire、审批 UI 和 persistence，远大于当前单个 companion tool 的需求。保留名会让同名 custom/prebuilt tool 无法使用，但换来 descriptor 唯一性和基于名称的 auto-approval 仍然安全；即使 runner 未启用也必须拒绝，避免配置变化后语义翻转。

### wall-clock 由外部 watchdog 和 worker cancellation select 共同保证

主 Tokio runtime 的 supervisor 是权威 watchdog，因此不依赖被 JS 死循环占住的 worker executor 获得 poll。到达 deadline 时，它从另一线程设置 atomic interrupt flag 并取消 script/host-call token：interrupt handler 处理正在执行的 JS 指令，worker local select 处理 pending Promise 和 bridge await。随后 supervisor 进入有限 teardown/join grace period。线程若卡在 native engine 内只能 detach 并占住并发 permit，无法被 Rust 安全强杀；严格终止需要 helper process。接受这个残余风险的前提是 MVP 只面向本地开发环境，不把进程内 QuickJS 宣称为多租户 sandbox。

### host effects 使用独立预算和 child transaction

QuickJS heap 与 bridge-result budget 都不覆盖 Rust 侧 retained state/artifacts。选择 script-scoped budget + per-host-call staging + single final commit：artifact/state 在产生外部副作用前或成功返回前预留，host call 成功只 commit 到 scope；scope 内同 key state 只保留最新值。只有外层 runner 即将返回 `Ok(report)` 时，`commit_into_outer()` 才把每个最终 key 写入一次并批量转移 artifacts。相比每个 child 直接写外层 context，这需要修改 `ThreadState::set`、artifact API 和 file tool 的操作顺序，但能在内存已经膨胀或文件已经写入之前 fail closed，避免外层 `CallState` 重新累计同 key 历史值，并保留 outer error 不提交 effects 的现有语义。

### 中间结果不进入 Message history

只把最终 report 送给模型，中间调用不进入 Message history。MVP 接受 UI 不展示每个嵌套调用的限制；诊断走 tracing，已有 file-diff artifacts 仍正常展示。

## Security and Resource Limits

建议以配置项提供以下首发默认值，真实数值应通过 spike 和 benchmark 校准：

- JS source：256 KiB。
- QuickJS heap：64 MiB；不得启用让 memory limit 失效的 `rust-alloc`。
- QuickJS stack：512 KiB。
- 单次 script wall-clock：120 秒，从等待全局 worker permit 前开始计算；permit acquisition、主 Tokio runtime supervisor 和 worker 执行共享同一个绝对 deadline。supervisor timer 触发时从 worker 外设置 interrupt flag 并取消 script/host-call token。worker local select 只负责 pending Promise/host await 的响应速度。
- context/runtime teardown + worker join grace period：1 秒；超时返回 `WORKER_UNRESPONSIVE`、detach 线程且不释放该 worker 持有的全局并发 permit。数值由 spike 校准。
- 嵌套工具调用：最多 128 次，最多 16 个并发。
- 单条返回给 JS 的工具结果：4 MiB；本次 bridge 累计结果：16 MiB。
- script 累计 staged state：4 MiB，按序列化后的 JSON byte size 计费；同 key 替换时退还旧值额度，只保留最终值。
- script 累计 artifacts：32 MiB，按实际 retained path/metadata/patch byte size 计费；in-flight child reservation 也计入。artifact 必须先成功预留再执行任何对应的文件 mutation。
- `stdout` 与最终序列化 result report：各 1 MiB；后者包含 JSON envelope、不包含 stdout。tracing 默认不记录原始参数和结果，只记录 byte length、状态、duration、error category 及工具级安全摘要；不得靠截断 raw preview 保护敏感内容。
- 全局 `run_javascript` 并发 semaphore，防止多个 session 同时吃满 heap/线程。

补充边界：

- 不注册 module loader、dynamic native module、文件、网络、process、timer API；文件访问只能经固定六项 bridge 工具。
- `tools` 和 bridge 对象冻结，但这只减少误用，不作为权限边界。
- bridge 只接受 JSON object；继续由现有 `ToolWrapper`/tool implementation 做 authoritative 参数校验。
- 工具结果作为 string 返回。外部内容即使含代码也不会自动执行；文档明确禁止对工具结果调用 `eval`/`Function`。
- rquickjs 是对原生 QuickJS-NG engine 的进程内绑定，不是 OS sandbox。多租户部署需要 helper process + OS sandbox。

## Risks / Open Questions

1. **模型使用 JS PTC 的可靠性。** Claude 原生 PTC 训练环境是 Python；不同 OpenAI-compatible 模型对 `run_javascript`、`Promise.all`、只返回精简结果的遵循度未知。必须做有/无 PTC 的任务集对照。
2. **rquickjs async 生命周期。** 需要用真实 `AsyncRuntime` 验证 Rust Future -> Promise、Promise -> Rust Future、callback error、主 runtime watchdog 对 CPU 死循环的跨线程 interrupt、永久 pending Promise、卡住的 host Future、deadline cancellation、context teardown 和 bounded join，尤其是工具 Future 通过 channel 回到主 runtime 的组合。
3. **输出量和 effects 从模型上下文转移到 host 内存。** 中间结果不计模型 token，但仍会占 Rust/QuickJS 内存；QuickJS heap limit 不覆盖 host 侧 Rust `String`、state 或 file-diff artifacts，所以 bridge result、state 和 artifact 必须有三个独立累计预算。预算测量本身最多允许构造一个受现有 10 MiB file limit 约束的候选 patch，不能累计未计费 artifacts。
4. **部分副作用。** 脚本后半段失败不会回滚前面的文件编辑或 todo state；report 必须明确给出已完成调用数，不能把“脚本失败”描述成“什么都没发生”。
5. **实时 mode 变化与暂停恢复。** 已经开始的嵌套调用按现有语义继续；mode 变化作用于下一次 bridge call。`accept_edits` 切到 `explore` 后，snapshot 中的 `write_file`/`edit_file` 调用返回 `TOOL_UNAVAILABLE`，读取和 todos 仍可继续。反向切换不能扩展当前脚本；即使 runner 因同批其他调用或自身审批而跨重启恢复，也使用 generation 时持久化的 snapshot。新能力从下一次 generation 生效。
6. **进程内 worker 无法强杀。** 主 runtime watchdog 和 join grace 能限制请求等待，但 native engine 若失去响应，线程只能 detach 并永久占用一个并发 permit。spike 必须验证正常超时路径都能 teardown；多租户部署必须使用 helper process。
7. **可观测性。** MVP 只有安全摘要 tracing 和外层最终 report，不新增实时嵌套调用 UI。若验证后确有需要，再设计 child events；不能把它们折叠成 provider-visible ToolMessage，也不能重新引入原始 input/output 日志。
8. **discovery 的额外 round trip 是否值得。** 固定 descriptors 只有在 provider 的 prefix cache 覆盖 tool definitions 且 capability 经常变化时才有明显收益；首次 discovery 则必然增加一次 LLM 往返。evaluation 必须同时测 cache hit/input tokens、端到端 latency 和模型是否会不必要地重复 discovery，不能只以 descriptor byte 数判断收益。

## Implementation Roadmap

实现说明（2026-08-22）：风险验证直接落在 `coda_ptc` 的 engine tests 中，没有保留一份会与正式实现漂移的 `.scratchpad` 副本。覆盖项和验证目标不变；macOS 本地验证已完成，Linux 由 CI 覆盖。

- [x] **[risk validation]** 在 `.scratchpad/rquickjs-ptc/` 做最小 spike：专用线程的 current-thread executor 内运行 `AsyncRuntime`，JS `await` 通过 channel 调用主 Tokio runtime 的 fake tool；由主 Tokio runtime supervisor 驱动 watchdog，覆盖顺序、`Promise.all`、异常、CPU 死循环、`await new Promise(() => {})`、永不返回的 host call、用户取消、deadline、teardown 和 bounded join。
      Purpose: 在改公共接口前证明最关键的 runtime/async 假设。
      Verification: spike tests 在 Rust 1.95 的 macOS/Linux CI 目标通过；worker 完全忙于 CPU 循环时，主 runtime watchdog 仍按 deadline 设置 flag 并触发 interrupt；pending Promise/host call 观察同一 cancel token 结束；正常超时路径无线程泄漏或 runtime drop assertion，并记录 native worker 无法 join 时的 fail-closed 行为。

- [x] **[crate boundary]** 新建 `crates/coda_ptc` 并加入 Cargo workspace；依赖 `coda_core`、rquickjs、Tokio/futures、serde 和 tracing，不依赖 `coda_agent`/`coda_server`。
      Purpose: 先固定依赖方向，避免 engine 实现扩散到普通工具和 policy 层。
      Verification: `cargo tree -p coda_ptc` 不出现 `coda_agent`、`coda_tools` 或 `coda_server`。

- [x] **[core API]** 在 `coda_core` 增加中性的 `HostToolInvoker`、`HostToolCallResult`、`HostToolCallError`、`HostCallScope` 和 `StagedToolCall`；`ToolCallContext` 默认不携带 invoker，由 `HostCallScope::begin_tool_call` 派生共享预算但剥离 invoker 的 child context。`StagedToolCall::commit` 只写 scope，`HostCallScope::commit_into_outer` 消费 scope 并执行唯一一次最终提交。让 `ThreadState::set` 和 `record_artifact` 返回可传播的 resource-limit error。JS limits/report/engine error 和 host-error-to-JS 映射留在 `coda_ptc`。
      Purpose: 给 PTC 一个窄的 host trust boundary，同时避免普通工具默认获得调用其他工具的能力。
      Verification: invoker 缺失、调用成功、typed error 和 cancellation 单元测试；weak-reference/drop test 证明无 `context → invoker → context` 环；child context 的 invoker 恒为 `None`，失败 child 不合并 effects；final commit 只能消费 scope 一次。

- [x] **[host effect budgets]** 实现 script-scoped state/artifact 累计预算和 per-child staging。state 按 key last-write-wins；file tools 在 mutation 前构造并记录 artifact，`write_todos` 传播 `state.set` 的预算错误。
      Purpose: 让 QuickJS heap 之外的 retained host memory 也有硬上限，并让 budget exceed 在外部副作用前 fail closed。
      Verification: 128 次 todo 替换在 scope 和外层 `CallState` 都只保留最终 key/value；final commit 前外层 state/artifacts 保持不变；并发 reservations 总和不能越界；write/edit 在 artifact 超限时不创建目录、不创建/截断/写入文件；child 失败或取消释放 reservation 且不写 scope；outer abort/error drop scope 且不写外层；普通 JS exception 返回 `Ok(report)` 时已成功 child effects 正常锚定到外层 ToolMessage。

- [x] **[safe tracing]** 修改 `ToolWrapper::execute` 的 span 字段，不再记录 raw input/output；提供长度、状态、duration、error category 和可选的工具级安全摘要。
      Purpose: 在嵌套调用放大调用量之前关闭文件内容和待写内容进入日志的现有路径。
      Verification: tracing capture tests 用 sentinel 覆盖 read/write/edit 的嵌套输入与超限输出，确认日志不含原始内容且摘要有界。

- [x] **[execution metadata]** 在 agent scheduler 增加 `PreparedToolCall`/`ToolExecutionMetadata` 及对应 `Stored*` 类型，让 generation snapshot 同时经过 auto queue、pending approval、checkpoint 和 resume；公开的 `PendingApproval` 只投影 `ToolCall`。
      Purpose: 固定模型生成代码时的能力上界，防止暂停或重启期间 policy 放宽造成扩权。
      Verification: 同批普通工具触发审批、runner 自身审批、checkpoint round-trip 和 restart tests 均保持原 snapshot；收紧可移除能力，放宽不能增加能力；无 snapshot 的 runner call fail closed。

- [x] **[engine]** 在 `coda_ptc` 实现 `RunJavaScriptTool`、JS bootstrap、主 Tokio supervisor/watchdog 和专用线程 executor；只启用 rquickjs `std` + `futures`，组合跨线程 interrupt、worker cancellation select、host cancellation 与 bounded teardown/join。
      Purpose: 提供无 ambient authority、可取消且有硬资源限制的 ES2020 运行环境。
      Verification: 语法错误、throw、Promise、并发、递归、内存、stack、worker permit 排队 deadline/cancel、外部 watchdog CPU deadline、pending Promise deadline、host-call cancellation、未 await host call、join grace、stdout/final output 截断测试。

- [x] **[registration]** 在 `coda_tools` 增加只负责 catalog 注册的 `RunJavaScriptToolSpec`，其实现来自 `coda_ptc`。
      Purpose: 让默认 tools、`AGENT.md` 解析和名称冲突校验继续走现有路径。
      Verification: spec name 与 built tool name 一致；显式包含/省略 `run_javascript` 的 agent config tests。

- [x] **[integration]** 在 agent driver 创建只捕获 tools/policy/snapshot 的 `AgentToolInvoker`；eligible subset 非空时生成隐藏 snapshot。runner 自身进入普通 approval partition；每次 bridge call 检查持久化 snapshot 和当前 policy，并接收由 `HostCallScope` 显式传入的受限 child context。
      Purpose: 保证 MVP 没有嵌套审批 continuation，同时不绕过 workspace tightening、文件锁、参数校验或 rewind 所依赖的 state anchor。
      Verification: 默认 explore 只暴露四项、accept_edits/yolo 暴露六项；workspace approval_required 只移除命中的内部工具；命中 runner 自身时正常 Suspended，批准后使用原 snapshot；mid-script 收紧 mode 后受影响工具返回 `TOOL_UNAVAILABLE`，放宽不扩权；嵌套普通工具看不到 invoker，并覆盖 abort、deadline、state/artifact budget tests。

- [x] **[synthetic namespace]** 在 `coda_tools` 声明 `list_javascript_tools` 为 synthetic reserved name，并让 `AgentTeam::new` 无条件拒绝任何 agent 的同名 `ToolSpec`。
      Purpose: 防止重复 provider descriptor，并保证按名称 auto-approve 只可能命中内建 synthetic 操作。
      Verification: root/sub-agent 的同名 prebuilt/custom tool 都返回 `ReservedToolName`；runner 未配置时同样拒绝；正常 runner 配置不需要显式 companion spec。

- [x] **[cache-friendly discovery]** 把 `run_javascript` 改为不含 capability 的固定 descriptor；增加自动伴生、固定 descriptor 的 `list_javascript_tools` synthetic tool，返回 `snapshot ∩ current policy` 的有序名称。两种 call 都绑定并持久化 generation snapshot；discovery 不注册为 `ToolSpec`、不进入 JS bridge。
      Purpose: capability/policy 变化不再改写 runner descriptor，同时让新增 allowlisted bridge tool 自动进入可发现名称列表。
      Verification: capability 非空时，不同 permission mode 和 agent tool subset 生成 byte-for-byte 相同的两个 descriptors；discovery 分别返回四项/六项有序名称，且每个名称都在同一 request 中有直接 descriptor；snapshot 经过 approval/checkpoint/restart 后不扩张，执行前收紧会缩小结果；未配置 runner 或空 capability 时两者都不出现；伪造无 snapshot 的 discovery/runner 均 fail closed。

- [x] **[synthetic execution]** 在 driver 增加 discovery special-case executor：只接受空对象，复用 local settlement 产生普通事件、ToolMessage、outcome、duration 和安全 tracing；用 `coda_ptc` formatter 生成 `{"available_tools":[...]}` 和 unavailable message，两者各有 16 KiB 硬上限且禁止部分截断。snapshot 构造时用完整名称集合和最长 requested name 预验证两种格式，上限失败时同时省略 runner/discovery。
      Purpose: 补回不经过 `ToolWrapper` 后必须显式承担的 trust-boundary、可观测性和 history memory 边界。
      Verification: `{}` 与带空白的对象成功；`null`、array、scalar、malformed JSON、非空对象返回 `InvalidParameters`；start/end event、ToolMessage、outcome 和 duration 与普通工具一致且 tracing 不含 sentinel raw data；两种 formatter 恰好等于各自上限时成功，超过一字节 fail closed 且无部分列表；生成阶段任一格式超限都不提供两个 synthetic descriptors。

- [x] **[unavailable recovery]** 将 host unavailable error 扩展为 requested name + 当前 `snapshot ∩ policy` 名称，并映射成稳定的 `TOOL_UNAVAILABLE` message；空集合明确输出 `available tools: none`。
      Purpose: policy 收紧或模型使用过期 capability 时，下一轮可直接重写脚本，不必再调用 discovery。
      Verification: snapshot 外名称、mid-script policy 收紧和空 capability tests 均检查 error code 与有序 available list；policy 放宽后的 snapshot 外工具绝不出现在错误中；uncaught report、try/catch 和 `Promise.allSettled` 都保留同一 message。

- [x] **[prompt/tool UX]** 固定 runner description 说明 `list_javascript_tools`、`tools.<name>(object) -> Promise<string>`、`JSON.parse`、`Promise.all`、runtime limits、返回值以及带 available list 的 `TOOL_UNAVAILABLE`；直接工具 descriptors 保留。
      Purpose: 让模型知道 discovery 只返回名称、schema 应从同名直接工具读取，何时复用已有结果，以及什么时候 PTC 比直接调用更合适。
      Verification: fixture provider 检查 descriptor 跨 policy 保持 byte-for-byte 稳定；人工/自动任务集确认 discovery、工具名和参数生成稳定。

- [ ] **[evaluation]** 对批量文件读取、批量文件编辑、todo 过滤、小型单调用、workspace 强制审批五类任务做 A/B。
      Purpose: 确认收益来自真实 workload，而不是仅增加脚本开销。
      Verification: 比较任务成功率、LLM 请求次数、prompt/completion tokens、wall time、host memory、工具调用数；默认只在明显获益的任务中引导使用。

- [ ] **[hardening, deployment-dependent]** 多租户或远程服务场景把 executor 移入 helper process/OS sandbox，沿用同一 bounded bridge protocol；若产品在 MVP 阶段就要求严格强杀和资源回收，则此项前移为发布前置条件。
      Purpose: 把 QuickJS engine exploit/crash 从 coda_server 进程隔离。
      Verification: kill/timeout/OOM/invalid IPC fuzz tests，worker 崩溃不影响 session hub 和其他 session。

## Sources

- [Anthropic: Programmatic tool calling](https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling)
- [Anthropic: Tool reference / allowed_callers](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-reference)
- [rquickjs repository](https://github.com/DelSkayn/rquickjs)
- [rquickjs 0.12.2 crate documentation](https://docs.rs/rquickjs/latest/rquickjs/)
- [rquickjs AsyncRuntime](https://docs.rs/rquickjs/latest/rquickjs/struct.AsyncRuntime.html)
- [rquickjs Ctx / eval_promise](https://docs.rs/rquickjs/latest/rquickjs/context/struct.Ctx.html)
- [rquickjs Promise](https://docs.rs/rquickjs/latest/rquickjs/struct.Promise.html)
