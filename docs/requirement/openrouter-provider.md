## Problem

Coda 目前把 OpenAI 兼容服务归为 `generic` 或 `deepseek`，只能覆盖基础 Chat Completions 差异。OpenRouter 虽然兼容该协议，但会用 `reasoning` / `reasoning_details` 表达推理，并可能在 HTTP 200 的 SSE 流内返回错误；若只新增普通 provider 配置，推理展示、多轮工具调用和错误处理会不可靠。需要让用户通过静态配置安全地使用 OpenRouter 模型，同时保持现有会话、Agent 和模型选择体验。

## Scenarios

- 部署者在 `coda-server.toml` 中配置 OpenRouter API Key、端点和精选模型，并为每个模型声明名称、上下文、输入模态、推理档位及可选的输出预算。
- 用户在现有模型选择器中选择已配置的 OpenRouter 模型，并只能使用该模型声明支持的图片输入和推理档位。
- Agent 使用推理模型连续调用工具时，Coda 展示可见推理，并在后续请求中无损带回模型继续推理所需的数据。
- OpenRouter 在正常流或 SSE 流中返回内容、工具调用、usage 或错误时，Coda 分别给出正确结果或明确失败，不把错误当成空回复。

## Scope

In:
- OpenRouter 的 OpenAI-compatible Chat Completions 接口，模型由用户静态配置。
- 文本、图片输入、流式文本、流式推理、并行工具调用、usage 和流内错误。
- OpenRouter 的统一 reasoning 配置，以及明文或结构化 reasoning 在工具调用轮次中的持久化与回传。
- 以 `x-ai/grok-4.5`、`moonshotai/kimi-k3`、`z-ai/glm-5.2` 作为兼容性样本。

Out:
- 自动调用 `/models` 发现或刷新模型；OpenRouter 全量模型目录。
- OpenRouter Responses API、Anthropic Messages API、托管工具、插件、presets、BYOK 和高级路由策略。
- xAI、Moonshot、智谱直连 provider；本次只用其协议差异检验抽象边界。
- 会话产生消息后切换模型或 provider，以及由此形成的混合 reasoning continuation 历史。

## Constraints

- 继续由服务端配置决定 dashboard 可见模型；不得因接入 OpenRouter 暴露未配置模型。
- 模型能力不能仅由供应商名称推断：Grok 4.5 推理不可关闭，Kimi K3 为始终推理模型，GLM 5.2 的推理开关与 effort 组合不同。
- OpenRouter 可能返回 `reasoning` 字符串或不可改写的 `reasoning_details` 序列；继续工具调用所需数据必须随会话持久化，而不仅用于 UI 展示。
- 本期只保证会话始终使用开始时选择的模型/provider；不要求转换、裁剪或兼容其它 provider 生成的历史消息。
- 输出 token 预算允许按模型静态声明；未声明时不发送上限、沿用 provider 默认值，不能继续对所有模型固定为 10,000。
- 允许调整内部 API 和持久化格式，无需兼容旧数据；现有 `generic` 和 `deepseek` 行为不得回归。

## Success Criteria

- 配置了 OpenRouter 的服务器能启动，并且 dashboard 只列出静态声明的模型、模态和推理档位。
- 三个样本模型的请求映射分别符合其 OpenRouter 能力，且不向不可关闭推理的模型发送关闭值。
- 流式推理可以实时显示；含工具调用的 assistant 消息在下一次请求中保留完整 reasoning，上下文在进程重启后的会话恢复中仍有效。
- 单次与并行工具调用都能完成至少一轮“模型调用 → 工具结果 → 模型继续生成”。
- 最终 usage 包含标准 token 数和可用的 cached/reasoning token 明细；OpenRouter 流内错误会终止该轮并暴露错误信息。
- 使用模拟 OpenRouter SSE fixture 覆盖文本、推理、工具调用、usage、结构化 reasoning 和流内错误，并通过全量 Rust 测试与 clippy。

## References

- [OpenRouter Chat Completions](https://openrouter.ai/docs/api/reference/overview)
- [OpenRouter reasoning 与跨轮保留](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens)
- [OpenRouter 流式错误](https://openrouter.ai/docs/api/reference/errors-and-debugging)
