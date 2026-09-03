#!/bin/sh
session_id=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--resume" ]; then
    session_id="$arg"
    break
  fi
  prev="$arg"
done
if [ -n "${FAKE_CLAUDE_FORCE_SESSION:-}" ]; then
  session_id="$FAKE_CLAUDE_FORCE_SESSION"
fi
if [ -z "$session_id" ]; then
  session_id="claude-session-fresh"
fi

if [ -n "${FAKE_CLAUDE_START_LOG:-}" ]; then
  printf 'pid=%s args=%s\n' "$$" "$*" | tr '\n' ' ' >> "$FAKE_CLAUDE_START_LOG"
  printf '\n' >> "$FAKE_CLAUDE_START_LOG"
fi

turn=0
while IFS= read -r line; do
  turn=$((turn + 1))
  if [ -n "${FAKE_CLAUDE_LOG:-}" ]; then
    printf '%s\n' "$line" >> "$FAKE_CLAUDE_LOG"
  fi
  if [ "${FAKE_CLAUDE_MALFORMED:-}" = "1" ]; then
    printf '%s\n' '{not-json'
    continue
  fi

  emit_session="$session_id"
  if [ "${FAKE_CLAUDE_DRIFT_SESSION:-}" = "1" ] && [ "$turn" -gt 1 ]; then
    emit_session="claude-session-drift"
  fi

  printf '{"type":"system","subtype":"init","session_id":"%s","model":"claude-test","claude_code_version":"2.1.258"}\n' "$emit_session"

  case "$line" in
    *CRASH_ACTIVE*)
      printf '%s\n' 'fake Claude active crash' >&2
      exit 91
      ;;
  esac

  thinking_only=0
  nested_text=0
  multi_text=0
  case "$line" in
    *THINKING_ONLY*) thinking_only=1 ;;
  esac
  case "$line" in
    *NESTED_TEXT*) nested_text=1 ;;
  esac
  case "$line" in
    *MULTI_TEXT*) multi_text=1 ;;
  esac

  if [ "$thinking_only" -eq 0 ]; then
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hidden"}}}'
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"哈囉-%s"}}}\n' "$turn"
  fi

  if [ "$nested_text" -eq 1 ]; then
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"secret","nested":{"text":"should-not-capture"}}]}}'
  else
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hidden-thought","signature":"sig"}]}}'
  fi

  if [ "$multi_text" -eq 1 ]; then
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"thinking","thinking":"skip"},{"type":"text","text":"two"}]}}'
  elif [ "$thinking_only" -eq 0 ]; then
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"final-%s"}]}}\n' "$turn"
  fi

  if [ "${FAKE_CLAUDE_STDERR:-}" = "1" ]; then
    printf '%s\n' 'fake Claude diagnostic' >&2
  fi
  if [ -n "${FAKE_CLAUDE_DELAY_MS:-}" ]; then
    sleep 1
  fi

  fail=0
  case "$line" in
    *FAIL_RESULT*) fail=1 ;;
  esac
  if [ "${FAKE_CLAUDE_FAIL_PREPARE:-}" = "1" ]; then
    case "$line" in
      *"Session Preparation"*) fail=1 ;;
    esac
  fi

  if [ "$fail" -eq 1 ]; then
      printf '{"type":"result","subtype":"error","is_error":true,"terminal_reason":"error","session_id":"%s","usage":{"input_tokens":%s,"output_tokens":%s},"modelUsage":{"claude-test":{"inputTokens":%s,"outputTokens":%s}},"result":"should-not-emit"}\n' "$emit_session" "$((20 + turn))" "$((2 * turn))" "$((20 + turn))" "$((2 * turn))"
  else
      printf '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":"%s","usage":{"input_tokens":%s,"output_tokens":%s},"modelUsage":{"claude-test":{"inputTokens":%s,"outputTokens":%s}},"result":"should-not-emit"}\n' "$emit_session" "$((20 + turn))" "$((2 * turn))" "$((20 + turn))" "$((2 * turn))"
  fi

  case "$line" in
    *EXIT_AFTER_RESULT*) exit 92 ;;
  esac
done
