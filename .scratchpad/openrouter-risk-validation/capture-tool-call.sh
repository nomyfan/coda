#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
output_dir="$repo_root/.scratchpad/openrouter-risk-validation"

set -a
source "$repo_root/.env"
set +a
: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY is required}"

capture() {
  local slug="$1"
  local model="$2"
  local effort="$3"
  local request_path="$output_dir/$slug-request.json"
  local response_path="$output_dir/$slug-tool-call.sse"

  jq -n \
    --arg model "$model" \
    --arg effort "$effort" \
    '{
      model: $model,
      stream: true,
      stream_options: {include_usage: true},
      max_tokens: 2048,
      reasoning: {effort: $effort, exclude: false},
      messages: [{
        role: "user",
        content: "Call lookup_weather exactly once for Singapore. Do not answer directly."
      }],
      tools: [{
        type: "function",
        function: {
          name: "lookup_weather",
          description: "Return the current weather for a city.",
          parameters: {
            type: "object",
            properties: {city: {type: "string"}},
            required: ["city"],
            additionalProperties: false
          }
        }
      }],
      tool_choice: "required"
    }' >"$request_path"

  local status
  status=$(curl --silent --show-error --no-buffer \
    --output "$response_path" \
    --write-out '%{http_code}' \
    https://openrouter.ai/api/v1/chat/completions \
    -H "Authorization: Bearer $OPENROUTER_API_KEY" \
    -H 'Content-Type: application/json' \
    --data-binary "@$request_path")

  printf '%s %s\n' "$slug" "$status"
  [[ "$status" == "200" ]]
}

capture grok-4.5 x-ai/grok-4.5 low
capture kimi-k3 moonshotai/kimi-k3 low
capture glm-5.2 z-ai/glm-5.2 high
