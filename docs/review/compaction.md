# Compaction — 实现评审

> 对象：`61e3b38178fce51100ea22a778b69c70430b47ea`（`feat/compaction`）
> 对照：`docs/implementation/compaction.md`（v4.4）
> 日期：2026-08-16
>
> 后续已处理：连接循环阻塞、`delete` 门闩、空会话、`/compact` 命令入口（新会话 / 编辑 / mention Enter）、指令放在 transcript 前、忙时错误文案、发送 `/compact` 时乐观展示用户消息（`pendingCompact` 标记，结束快照按内容对账）。ContextUsage 环暂不改。

后续 `40c0e120` 只动了 `.gitignore`，不在本次范围内。

## 结论

机制本身和设计对齐：压缩只改模型视图、不改历史；边界就是 `Custom{kind:"compaction"}`；`Message` / `RequestMessage` 拆开后降级是类型保证；门闩挂 `EntryState`；提交用 root `message_count` CAS；transcript 只走 snapshot。hub / storage / view 的测试盖到了该盖的竞态。

主路径有一个会让「正在压缩」对发起者不可见的集成洞，应先修。其余是 UX 与边界情况。

## 必须修

### 1. `compact` 堵死连接循环，发起者收不到 `compacting` snapshot

`list_files` 特意 `tokio::spawn`，注释写明：在连接循环里 await 长操作会冻住**同一条连接**上的 event 转发。`compact` 却走了 `dispatch_request` 的同步路径，LLM 最多 60s：

```875:887:app/coda_server/src/bin/server.rs
        rpc::Incoming::Request { id, method, params } if method == "list_files" => {
            // ...
            tokio::spawn(async move { /* ... */ });
            true
        }
        rpc::Incoming::Request { id, method, params } => {
            let reply =
                dispatch_request(app, conn_id, streams, selections, id, &method, params).await;
            transport.send(&reply).await
        }
```

单 tab 的正常路径因此失效：

1. `handle_compact` 里 `push_snapshot(compacting=true)` 只进了 hub 的 mpsc，连接循环卡在 RPC 上，`streams.next()` 不被 poll。
2. 前端故意不在本地置 `compacting`，只信 snapshot。发起者在整段摘要期间：composer 仍可输入、无 spinner、可再点发送。
3. keepalive ping 在 `transport.recv()` 里，循环不转就不 ping。默认 30s 一次，压缩上限 60s，中间代理可能掐连接。
4. 同连接上的 `open_session` / `task` / `detach` / `delete` 全部排队。切会话、删会话都会卡住。

hub 测试直接读 `RelayEvent`，绕过连接层，所以测不出来。

**修法：** 和 `list_files` 一样把 `compact` 丢到独立 task（它不碰 `streams` / `selections`）。补一条测试：发起连接在 RPC 返回前就能收到 `compacting: true`。前端在发出 RPC 时先乐观置 `compacting=true` 只是补丁，根因是 snapshot 送不出去。

## 中等

### 2. 压完之后 context 环不降

`ContextUsage` 取的是**最后一条 root assistant** 的 `usage`。摘要是 `Custom`，没有 usage，环会一直停在压缩前的高水位，直到下一轮 turn。用户就是看着这个环才按 `/compact` 的，压完环不动，看起来像没生效。

`historyUsage` 仍然扫全量历史里所有 `Assistant`——设计只防了「摘要把环抬高」，没处理「旧 usage 把环钉死」。

压完至少应把 usage 清掉，或只从最新 compaction 边界往后算；环先掉下去，下一轮再被真实 `prompt_tokens` 校准。

### 3. 新会话 / 编辑路径不认 `/compact`

`App.tsx` 的 `handleSend`：新会话 composer 直接 `sendTaskToNewSession`，编辑中直接 `rewindTurn`，都不走命令解析。

- 新会话里输入 `/compact …`：模型收到字面量。`compactActiveSession` 也拒 draft。
- 编辑 `/compact` 那条 user 气泡再提交：先 rewind（把这次压缩撤掉），再把 `/compact …` 当普通 turn 发出去。用户以为在改指令重压，实际是撤销压缩 + 跟模型聊天。

`/compact` 行是普通 `User` 消息，transcript 上有 edit / fork。要么不要给它这些入口，要么 edit 提交时仍走 compact，而不是 rewind+task。

### 4. `/` mention 会抢走 Enter

`/compact` 被 `detectTrigger` 当成 skill token。workspace 里只要有 skill fuzzy 匹配 `compact`（`composer-mentions` 测试里就有叫 `compact` 的例子），Enter 会插入 skill，不会发命令。要先 Esc。

设计只说了「发送前拦截」，没处理菜单抢键。整行是 `/compact` 命令时，菜单应关掉，或 Enter 优先当提交。

### 5. 没有 checkpoint 被报成 `Stale`

```353:357:app/coda_server/src/bin/server.rs
            let checkpoint = storage
                .load_checkpoint(&key.1)
                .await
                .map_err(CompactError::Storage)?
                .ok_or(CompactError::Stale)?;
```

刚 `open_session`、还没第一条消息时，错误文案是 *“the conversation changed while it was being summarized”*。应在 opener 里直接拒空视图，或单独一种结果，不要冒充 CAS 失败。

### 6. `delete` 没进 compacting 门闩

五个历史改写点（task / set_model / fork / rewind / 二次 compact）都挡了，`delete` 没有。正确性靠 CAS + FK；代价是浪费一次 LLM，以及发起者稍后看到费解的 Abandoned。和设计 Decision 6「门闩是为了省掉白花的调用」不一致。前端 `deleteSession` 也不看 `compacting`。

## 小问题

- **commit message 是 `.`**，不符合 Conventional Commits。至少该是 `feat: add explicit /compact`。
- **用户指令接在 transcript 后面**。窗口快满时，部分 provider 截尾会丢掉「只保留架构决策」这类约束。指令放在 `<transcript>` 前面更稳。
- 压缩中再 compact 回的是 `SESSION_NOT_IDLE` / “finish or abort its current turn”；`set_model` 回 `TurnRunning`。都不是 turn。
- 设计文档标题仍是「v4，待评审」，但已经在 `docs/implementation/`。
- 设计里写过的风险仍然在：视图本身已经超窗时，`/compact` 救不了自己；没有 abort，只靠 60s timeout。

## 做得好的地方

- `RequestMessage` 装不下 `Custom`，`coda_openai` 只换了类型名，kind 没有漏进 provider。
- `compaction::view()` 是唯一切片实现，runtime 组请求和应用层取摘要输入共用，5 条纯函数测试清楚。
- 门闩挂 entry 不挂 `LiveState`，躲过 `make_live` 重建把旗标冲掉；CAS 用的是 session id 当 root `thread_id`，不是字面量 `"root"`。
- RPC 响应不带消息实体，避免和 snapshot 双写 transcript。
- 失败写 `compaction_failed`、不动边界；Stale / Storage 一条不写。这个拆分是对的。
- 对话摊成纯文本再摘要，避开 tool definitions / reasoning continuation / 孤儿 tool result。
- `storage_pg` 覆盖了 watermark 推进、随后普通 save 不撞主键、stale、删会话不复活。当前环境没有 `DATABASE_URL`，尚未实跑——文档里承认了，不是漏测。

## 建议顺序

1. 把 compact 移出连接循环，并补「发起连接在 RPC 返回前就能收到 `compacting: true`」的测试。
2. 压完更新 context 环。
3. 新会话 / 编辑 / mention 菜单三条命令入口收口。
4. 空会话与 delete 门闩。
