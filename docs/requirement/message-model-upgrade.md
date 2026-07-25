## Problem

当前 `Message` 是一个纯枚举，存储为 `Vec<Message>`，缺少身份标识和关系追溯能力。定位消息只能靠 Vec index（不稳定），无法表达跨线程的因果关系。Rewind、fork 等操作无法精确定位和关联消息。

## Scenarios

1. **Rewind 定位：** 前端传 message ID 给后端定位目标消息，不依赖 index。

2. **Sub-agent 精确截断：** Rewind/fork 时，能判断 stateful sub-agent 的哪些消息是由已丢弃的 root 调用触发的，只删除这些消息而非整体清除。

3. **Fork 拷贝：** 从某个点 fork 出新 session 时，能判断 stateful sub-agent 的哪些消息属于 fork 点之前的调用，只拷贝这些。

4. **按轮次归集：** 给定一次用户提交，能查出它在所有线程（root + 各层 sub-agent）中引发的全部消息——用于 rewind/fork 的成组处理，也便于按轮次统计与排查。

5. **前端引用：** 前端用稳定的 message ID 标识消息，不因历史变动而失效。

## Scope

**In:**
- 持久化的 Message（User、Assistant、Tool）增加唯一身份标识
- 建立跨线程调用追溯关系：能从 sub-agent 的消息追溯到是父线程的哪次具体调用触发的
- 建立轮次归属：每条消息可归到发起它的那次用户提交，跨线程一致
- 同一消息在整个系统中（Runtime、持久化、Wire、前端）使用同一标识

**Out:**
- 不改变存储方式（存储层迁移是独立需求）
- 不引入 turn 级别的分组结构
- 不给 SystemMessage 加 ID（临时生成，不持久化）

## Constraints

- `Message` 是 `coda_core` 中的核心类型，被所有 crate 依赖，改动影响面广。
- `ToolMessage` 已有 `id` 字段表示 tool-call ID，新增身份标识需避免语义冲突。
- Wire protocol 中的 `Message` 会带上新字段，前端需要适配。

## Success Criteria

- 每条持久化 Message 都有唯一身份标识，全链路一致。
- 能从 sub-agent 的消息追溯到触发它的父线程具体调用。
- 能按用户提交把跨线程的相关消息成组取出。
- 前端能通过 message ID 稳定引用消息。
