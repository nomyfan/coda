# Spike：重启后还能知道哪些轮次活着

`spike.rs` 是一次性的探针，不是测试——它只打印，不断言。要重跑：把它放回
`crates/coda_agent/src/runtime/driver_tests/`，在 `mod.rs` 里加一行 `mod spike;`，然后

```
cargo test -p coda_agent --lib spike -- --nocapture --test-threads=1
```

五个场景，回答 `docs/design/turn-cancellation.md` 里的三个待决点。

---

## F1. 活着的 sub-agent 可能一行 checkpoint 都没有

硬崩场景（sub-agent 正在生成时进程消失）：

```
=== hard crash: what survives ===
thread 7d64e541 agent=coda ...
  resume_point=ToolExecution parent_message_id=64f2e82d... pending_replies=[("call_explore", "explore")]
snapshot present? false
```

存储里**只有 coda 一行**。explore 正卡在生成里，还没走到任何一个落库点，所以它没有
checkpoint、没有 snapshot、什么都没有。

**含义**：想靠「扫 checkpoint 找 `reply_target.call_id` 匹配的那一行」来找 producer 是不成立的
——活着的孩子恰恰可能还没写过库。

**但孩子的 thread_id 是能算出来的**，不需要任何存储：

```
thread_id = uuid5(parent_thread_id, "{parent_message_id}:{call_id}")     # stateless
thread_id = uuid5(parent_thread_id, agent_name)                          # stateful
```

而 `parent_message_id` 和 `call_id` 就在父线程自己的 `ToolExecutionState` 里。审批场景印证了
这个推导：

```
thread 6d2d45c1 agent=explore parent=Some("3eafb720")
  derivation_key=Some("e09931f8-78a8-44f5-a6df-4e002455dc98:call_explore")
```

父的 `parent_message_id` 正是 `e09931f8-…`，pending reply 正是 `call_explore`。

---

## F2. `active_threads` 不是「谁还活着」的清单

sub-agent 调用真的在飞的时候优雅退出：

```
=== graceful exit snapshot ===
active_threads: {}
```

空的。因为**根派发完就空闲了**（`handle_tool_execution` 见到 pending_replies 就 break，
`run` 返回 `TurnOutcome::Completed`，`run_agent` 随即把 `active_thread` 清成 None），而被卡住的
explore 压根没走到 `save_agent_snapshot`。

只有线程**停在 `PendingApproval`** 时才会有条目：

```
=== subagent suspended for approval ===
active_threads: {"explore": "6d2d45c1-…"}
```

**含义**：`active_threads` 的语义是「停在半路的线程」，不是「还会继续跑的工作」。拿它当活跃
判据会漏掉所有正在跑的东西。

---

## F3. 一个 turn_id 覆盖整棵子树

审批场景里 coda 和 explore 的历史条目带的是同一个 turn：

```
thread 3eafb720 agent=coda     turns in history: ["81f73679/user", "81f73679/assistant"]
thread 6d2d45c1 agent=explore  turns in history: ["81f73679/user", "81f73679/assistant"]
```

`EnvelopeBody::ToolCall` 带着 `turn_id`，`opening_user_message` 原样传给孩子。

**含义**：hub 按 `TurnId` 结算是可行的——一轮里所有 agent 发出的事件天然共享同一个 key，
不需要 hub 自己去拼父子关系。

---

## F4. 排队轮次的顺序和 turn_id 都在

屏障后连发两条：

```
drained[coda]: Task { message_id: MessageId(e7a6ee19-…), task: "t2" }
drained[coda]: Task { message_id: MessageId(d46e33de-…), task: "t3" }
```

Vec 保序，`TurnId::from(message_id)` 直接可得。`bootstrap` 回放时先 `agent_drained` 后
`drained`，而 `drained` 装的是屏障之后才到的——次序是对的。

---

## F5. runtime snapshot 不是一次性消费

```
before first restart:                        drained=[("coda", 1)]
after first restart (no graceful exit):      drained=[("coda", 1)]
```

`bootstrap` 只从**内存里的那份拷贝**上 `remove`，存储里那行原封不动，要等下一次优雅退出
（`save_agent_snapshot` / `wait_for_exit` 用新 runtime 自己攒的 snapshot 整体覆盖）才会清掉。
中间再崩一次，同一条用户任务会被回放第二遍。

**含义**：恢复出来的工作清单必须是**收敛**的，不能「snapshot 里有什么就当什么活着」。
这也是一个独立于本设计的既有缺陷，值得单独补一个测试。

---

## 结论：三个待决点怎么落

### 一、有序活跃轮次

**不需要新持久化。** 新轮次只从根 agent 的信箱进——sub-agent 收到的 `ToolCall` / `Reply` 都带着
调用方的 `turn_id`，从不开新轮。所以只有一条排队次序，就是根的：

```
有序活跃轮次 = [根的 current_turn（若未结束）] ++ [根信箱里还没读的 Task，按序]
```

根一次只处理一个信封，所以「未结束的已开始轮次」在根上**至多一个**，就是 `current_turn`；
重启后它由恢复出来的历史末条目给出。队首永远是根的当前轮——中止要取消的正是它。

候选方案里「持久化有序列表」和「承认重启后不需要完整顺序」都可以放弃。

### 二、`TurnId` 送到 hub

事件通道从 `(agent_name, ThreadId, AgentEvent)` 加一项变成 `(agent_name, ThreadId, TurnId, AgentEvent)`。
driver 侧 `AgentLoop` 持有当前 turn，F3 保证子树里每个 agent 报的都是同一个值。
hub 的 `unsettled_user_messages` 从 FIFO 改成按 `TurnId` 索引，`fold_settled_turn` 按 key 删——
天然幂等，同一轮 settle 两次（`Suspended` 一次、结束再一次）第二次自然是 no-op。

### 三、`pending_reply` 的存活判据

**判据落在 thread 粒度，而不是 turn，也不是 call。** 因为每个待回复调用恰好对应一个孩子线程：

- stateless：`derivation_key` 含 `parent_message_id:call_id`，每次调用一个独立线程；
- stateful：一个线程复用，而并发调用在 `driver.rs:943` 就被拒了，所以同一时刻至多一个未决调用。

于是「这个 `call_id` 有没有活着的 producer」等价于「这个孩子线程此刻在本进程里有没有工作」，
而孩子线程 id 由 F1 的推导免费得到。

判据本身要问 runtime，不是问存储：

> 该 thread_id 上，此刻是否有 (a) 正在跑的轮次，或 (b) 尚未被取走的信封？

runtime 看得见全部两者——所有投递都过 `send_message`，所有消费都在 `run_agent` 里。
唯一的漏洞是 `bootstrap` 把 `init_envelopes` 直接塞进 channel、绕过了 `send_message`，
所以登记必须在那里补上，且要早于 agent task 能看见信封。

**「按 call/thread 的可恢复工作清单」不需要另建**——thread 粒度的在途登记就是它。
