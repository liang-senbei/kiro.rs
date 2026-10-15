#!/usr/bin/env bash
set -euo pipefail

# Usage: probe_compaction_cache.sh BASE_URL CONFIG_PATH SESSION_UUID [MODEL]
base_url=${1:?base URL required}
config_path=${2:?config path required}
session_id=${3:?session UUID required}
model=${4:-gpt-5.6-sol}
api_key=$(jq -er '.apiKey' "$config_path")

request=$(jq -nc --arg model "$model" --arg session "$session_id" '
  {
    model: $model,
    prompt_cache_key: $session,
    stream: false,
    max_output_tokens: 256,
    reasoning: {effort: "low"},
    instructions: "You are a coding assistant. Preserve project decisions and answer without calling tools.",
    input: [
      {
        type: "additional_tools",
        tools: [{
          type: "function",
          name: "inspect_module",
          description: "Inspect one source module.",
          parameters: {
            type: "object",
            properties: {path: {type: "string"}},
            required: ["path"],
            additionalProperties: false
          }
        }]
      },
      {
        type: "message", role: "user",
        content: ("Cache probe source context. " +
          ("The module contains a parser returning Result, a formatter returning String, and ASCII tests. " * 700))
      },
      {
        type: "function_call", call_id: "call_cache_probe", name: "inspect_module",
        arguments: "{\"path\":\"src/parser.rs\"}"
      },
      {
        type: "function_call_output", call_id: "call_cache_probe",
        output: "The parser returns Result and the formatter returns String."
      },
      {
        type: "message", role: "user",
        content: "Project status: parser returns Result; formatter returns String; tests cover ASCII only. Decision: add UTF-8 error handling without changing the formatter API. Next action: implement tests, then fix the parser. Briefly confirm the next action without calling tools."
      }
    ]
  }
')
compact_request=$(jq -c '.input += [{type: "compaction_trigger"}]' <<<"$request")

request_upstream() {
  local payload=$1
  curl --fail-with-body --silent --show-error --max-time 180 \
    -H "Authorization: Bearer $api_key" \
    -H 'Content-Type: application/json' \
    --data-binary "$payload" "$base_url/v1/responses"
}

report() {
  local stage=$1 response=$2
  if jq -e '.status != "completed"' <<<"$response" >/dev/null; then
    printf 'Probe stage %s did not complete\n' "$stage" >&2
    return 1
  fi
  jq -c --arg stage "$stage" '{
      stage: $stage,
      status,
      output_types: [.output[]?.type],
      message_preview: ([.output[]? | select(.type == "message") | .content[]?.text] | join(" ") | .[:300]),
      summary_preview: (.output[0].encrypted_content? // "" | sub("^kiro-rs\\.compaction\\.v1:"; "") | .[:300]),
      usage,
      error
    }' <<<"$response"
}

normal_response=$(request_upstream "$request")
report normal_first "$normal_response"
normal_response=$(request_upstream "$request")
report normal_repeat "$normal_response"
compact_response=$(request_upstream "$compact_request")
report compact "$compact_response"
compact_item=$(jq -c '.output[]? | select(.type == "compaction")' <<<"$compact_response")
if [[ -z "$compact_item" ]]; then
  printf 'Compact response did not contain a compaction item\n' >&2
  exit 1
fi
resume_request=$(jq -c --argjson item "$compact_item" '
  .input = [
    $item,
    {type: "message", role: "user", content: "What is the next implementation step? Answer in one sentence."}
  ]
' <<<"$request")
resume_response=$(request_upstream "$resume_request")
report after_compact "$resume_response"
