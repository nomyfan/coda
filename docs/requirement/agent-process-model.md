# coda_agent 执行模型整理

## Problem

`coda_agent` 的 `Agent` 同时承担实例模板和运行对象的职责，program 定义、thread 状态、driver 与 execution scope 的关系不够直观。借用操作系统的 program / process / process group 概念整理职责和状态归属，让维护者更容易理解实例创建、并发、恢复及取消；目标是结构性重构，而非实现操作系统或单纯替换名称。

## Scenarios

- 同一个 program 或不同 program 的多个独立实例同时执行，各自拥有独立上下文；前台调用可以先启动多个实例，再等待结果。
- 调用已有实例时，通过其身份定位状态；driver 已回收时加载 checkpoint，继续已有上下文，不因恢复而变成新实例。
- 前台 subagent 继承调用方的执行分组；后台 subagent 创建独立分组，其同步后代继承该组。多个后台分组可以同时运行。
- 停止 root 当前执行不终止独立后台工作；停止后台分组会取消组内执行及其拥有的后台 shell；关闭 session 清理全部工作。

## Scope

- `AgentSpec` 对应 source code，声明 prompt、工具及 subagent 引用；program 保存校验、解析和构建后的定义，包括 prompt、工具对象和可调用目标。program 在同一 session 内供多个 process 复用，不跨 session 共享绑定了会话资源的工具。
- 拆分现有 `Agent`：配置和构建结果归入 program，独立上下文和执行状态归入 process；process 引用 program，现有 `ThreadId` 承担 pid 的身份职责。
- program 是定义对象，不接收 envelope、不拥有 inbox 或 driver、不承担执行或消息路由。runtime 直接按 pid 路由、启动或恢复 process，消除无独立职责的 agent 中间层。
- 新 subagent 实例的创建以 spawn 理解，已有 stateful 实例继续调用并保留上下文；支持前台等待与后台运行。后台调用建立独立 process group，前台调用继承当前执行的组。
- session 容纳按 turn 建立的前台分组和多个后台分组，不再将整个 session 等同于唯一的 process group；分组归属于 execution，不将长期 process 永久绑定在某组。
- 梳理历史、模型可见 working memory 与执行恢复状态的区别，以及 envelope、通信 channel、事件广播的职责；不强求与 OS 一一对应，也不要求增加 Pipe 抽象。
- 将 `coda_process` crate 重命名为 `coda_execution`，避免与 agent process 模型混淆；继续承担 OS 子进程执行和 shell/subagent 后台任务管理，不因改名拆分职责。保留内部 `process` 模块、`GroupedChild` 等准确描述 OS 进程的名称，并同步更新 workspace、依赖、代码引用及相关文档。
- 本轮按保留现有行为整理需求：显式暴露 spawn/pid 调用接口、取消 stateful/stateless 隐式复用规则，均留待单独确认，不作为本轮成功条件。
- 不新增嵌套后台启动能力，不改变现有仅 root 可启动后台 subagent 的限制；不预先确定 fork 的新接口或复制语义。

## Constraints

- 当前已经按 `ThreadId` 创建独立 Agent/driver，同名 stateless subagent 可以实际并发执行；同一父 thread 下的 stateful subagent 隐式复用实例并拒绝重叠调用。不能以“现有 subagent 全部串行”为重构前提。
- 区分 process 身份、单次 execution 身份与 driver 生命周期；完成一次调用或回收 driver，不等于删除实例记忆。
- 分组成员以 process 和 execution 的组合身份识别；旧组取消或清理不得影响同 pid 的后续执行。后台答案完成后可能仍有其拥有的 shell 运行，必须保留结果完成与资源全部结束的区别。
- 现有 scope 归属于 execution：前台按 turn、后台按 task。设计必须明确它与 process group 的关系，不能因术语替换而把长期实例生命周期与单次执行混为一谈。
- 保留审批归属、取消传播、checkpoint 失败后的清理与隔离、后台完成通知及去重、冷启动不恢复后台执行，以及 fork/rewind 等已有约束。
- 同批重复调用同一 stateful 实例时，启动前识别并拒绝全部重复项，其他调用可继续；退出期间停止新调用准入，但仍接收并保存已有执行的有效在途消息，保留重开恢复路径及现有存储失败处理。
- 有界 inbox 的背压不得阻止其他 process 分发调用或启动 shutdown；回复数量超过 inbox 容量时，整批执行仍能完成，并保留退出切换时的消息归档保证。
- 允许必要的内部 API、序列化及持久化格式破坏性变化，无需兼容层；本轮保留现有 SQL/协议的 thread 字段名称，不为纯命名做迁移，也不机械改写用户界面或工具名称。
- 若后续改变模型需要理解的运行规则，同步更新默认 system prompt 和相关 templates。

## Success Criteria

- 从类型和接口能够明确识别 source code 声明、构建后的 program、process 状态、执行分组和 session 容器；无需借助同一个 `Agent` 名称解释模板与实例两种含义。
- 每份可变上下文、执行恢复状态及通信入口的所有者明确；实例选择和消息路由不依赖无独立职责的 agent 中间层。
- 相同与不同 program 的独立实例并发、已有实例恢复、前台等待及多个后台分组运行均保持正确；同一实例的重叠调用策略保持现有行为。
- 取消、审批、持久化恢复和后台结果交付的现有测试继续通过；针对职责迁移产生的实际风险补充验证。
- Rust 修改完成后通过 `cargo clippy`、`cargo test` 和 `cargo check -p coda_server --features pg-tests --all-targets`。
