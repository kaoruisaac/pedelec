#!/bin/sh
conversation_id=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--conversation" ]; then
    conversation_id="$arg"
    break
  fi
  prev="$arg"
done
if [ -z "$conversation_id" ]; then
  conversation_id="agy-conversation-fresh"
  fresh=1
else
  fresh=0
fi

agent_path=".agents/agents/pedelec-runtime/agent.md"
if [ -f "$agent_path" ]; then
  agent_exists=true
else
  agent_exists=false
fi
if [ -n "${FAKE_AGY_START_LOG:-}" ]; then
  printf 'pid=%s agent=%s args=%s\n' "$$" "$agent_exists" "$*" >> "$FAKE_AGY_START_LOG"
fi
if [ "${FAKE_AGY_REQUIRE_AGENT_FILE:-}" = "1" ] && [ "$agent_exists" != "true" ]; then
  printf '%s\n' 'missing pedelec-runtime custom agent' >&2
  exit 90
fi

turn=0
while IFS= read -r line; do
  turn=$((turn + 1))
  if [ -n "${FAKE_AGY_LOG:-}" ]; then
    printf '%s\n' "$line" >> "$FAKE_AGY_LOG"
  fi
  if [ "${FAKE_AGY_MALFORMED:-}" = "1" ]; then
    printf '%s\n' '{not-json'
    continue
  fi
  if [ "$fresh" -eq 1 ] && [ "$turn" -eq 1 ]; then
    printf '{"event":"init","conversation_id":"%s"}\n' "$conversation_id"
  fi
  case "$line" in
    *CRASH_ACTIVE*)
      printf '%s\n' 'fake Antigravity active crash' >&2
      exit 91
      ;;
  esac
  printf '{"event":"step_update","step_type":"agent_response","text_delta":"哈囉-%s","usage":{"input_tokens":%s,"output_tokens":%s}}\n' "$turn" "$((10 + turn))" "$turn"
  if [ "${FAKE_AGY_STDERR:-}" = "1" ]; then
    printf '%s\n' 'fake Antigravity diagnostic' >&2
  fi
  if [ -n "${FAKE_AGY_DELAY_MS:-}" ]; then
    sleep 1
  fi
  case "$line" in
    *FAIL_RESULT*|*FAIL_PREPARE*"[Session Preparation]"*) status="ERROR"; error='{"code":"fake_failure","message":"requested failure"}' ;;
    *) status="SUCCESS"; error='null' ;;
  esac
  prepare=0
  case "$line" in
    *"Session Preparation"*) prepare=1 ;;
  esac
  include_usage=1
  if [ "$prepare" -eq 1 ] && [ "${FAKE_AGY_PREPARE_USAGE:-0}" != "1" ]; then
    include_usage=0
  fi
  if [ "$include_usage" -eq 1 ]; then
    printf '{"event":"result","result":{"status":"%s","conversation_id":"%s","response":"final-%s","usage":{"input_tokens":%s,"output_tokens":%s,"total_tokens":%s},"error":%s}}\n' "$status" "$conversation_id" "$turn" "$((20 + turn))" "$((2 * turn))" "$((25 + (turn - 1) * 3))" "$error"
  else
    printf '{"event":"result","result":{"status":"%s","conversation_id":"%s","response":"final-%s","error":%s}}\n' "$status" "$conversation_id" "$turn" "$error"
  fi
  case "$line" in
    *EXIT_AFTER_RESULT*) exit 92 ;;
  esac
done
