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
  local initial_request="$output_dir/$slug-request.json"
  local initial_response="$output_dir/$slug-tool-call.sse"
  local continuation_request="$output_dir/$slug-continuation-request.json"
  local continuation_response="$output_dir/$slug-continuation.sse"

  jq -Rs '
    [split("\n")[]
      | select(startswith("data: {"))
      | sub("^data: "; "")
      | fromjson
    ]
  ' "$initial_response" >"$output_dir/$slug-chunks.json"

  jq --slurpfile chunks "$output_dir/$slug-chunks.json" '
    ($chunks[0]) as $chunks |
    [$chunks[].choices[0].delta.reasoning_details[]?] as $reasoning_details |
    [$chunks[].choices[0].delta.tool_calls[]?] as $tool_fragments |
    ($tool_fragments | map(select(.id != null) | .id) | first) as $tool_call_id |
    ($tool_fragments | map(select(.type != null) | .type) | first) as $tool_call_type |
    ($tool_fragments | map(select(.function.name != null) | .function.name) | first) as $tool_name |
    ($tool_fragments | map(.function.arguments // "") | join("")) as $tool_arguments |
    .messages += [
      {
        role: "assistant",
        content: "",
        reasoning_details: $reasoning_details,
        tool_calls: [{
          id: $tool_call_id,
          type: $tool_call_type,
          function: {name: $tool_name, arguments: $tool_arguments}
        }]
      },
      {
        role: "tool",
        tool_call_id: $tool_call_id,
        content: "{\"temperature_c\":30,\"condition\":\"humid\"}"
      }
    ] |
    .tool_choice = "none"
  ' "$initial_request" >"$continuation_request"

  local status
  status=$(curl --silent --show-error --no-buffer \
    --output "$continuation_response" \
    --write-out '%{http_code}' \
    https://openrouter.ai/api/v1/chat/completions \
    -H "Authorization: Bearer $OPENROUTER_API_KEY" \
    -H 'Content-Type: application/json' \
    --data-binary "@$continuation_request")

  printf '%s %s\n' "$slug" "$status"
  [[ "$status" == "200" ]]
}

capture grok-4.5
capture kimi-k3
capture glm-5.2
