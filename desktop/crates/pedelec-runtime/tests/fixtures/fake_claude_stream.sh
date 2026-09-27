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
  artifact_mode=""
  case "$line" in
    *THINKING_ONLY*) thinking_only=1 ;;
  esac
  case "$line" in
    *NESTED_TEXT*) nested_text=1 ;;
  esac
  case "$line" in
    *MULTI_TEXT*) multi_text=1 ;;
  esac
  case "$line" in
    *ARTIFACT_TOOL*) artifact_mode="tool" ;;
    *ARTIFACT_MIXED*) artifact_mode="mixed" ;;
    *ARTIFACT_DUP*) artifact_mode="duplicate" ;;
    *ARTIFACT_BAD*) artifact_mode="bad" ;;
    *ARTIFACT_USER_INPUT*) artifact_mode="user-input" ;;
    *ARTIFACT_MULTI*) artifact_mode="multi" ;;
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

  image='{"type":"image","id":"image-1","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}}'
  jpeg_image='{"type":"image","id":"image-jpeg","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/2Q=="}}'
  case "$artifact_mode" in
    tool)
      printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-9","content":[{"type":"text","text":"generated"},%s]}]}}\n' "$image"
      ;;
    mixed)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"with image"},%s]}}\n' "$jpeg_image"
      ;;
    duplicate)
      printf '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":%s}}\n' "$image"
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"with image"},%s]}}\n' "$image"
      ;;
    bad)
      printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-bad","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"%%%"}}]}]}}'
      ;;
    user-input)
      printf '{"type":"user","message":{"content":[%s]}}\n' "$image"
      ;;
    multi)
      printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-multi","content":[{"type":"image","id":"multi-1","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}},{"type":"image","id":"multi-2","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}}]}]}}'
      ;;
  esac

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

  prepare=0
  case "$line" in
    *"Session Preparation"*) prepare=1 ;;
  esac
  include_usage=1
  if [ "$prepare" -eq 1 ] && [ "${FAKE_CLAUDE_PREPARE_USAGE:-0}" != "1" ]; then
    include_usage=0
  fi
  if [ "$fail" -eq 1 ]; then
    if [ "$include_usage" -eq 1 ]; then
      printf '{"type":"result","subtype":"error","is_error":true,"terminal_reason":"error","session_id":"%s","usage":{"input_tokens":%s,"output_tokens":%s},"modelUsage":{"claude-test":{"inputTokens":%s,"outputTokens":%s}},"result":"should-not-emit"}\n' "$emit_session" "$((20 + turn))" "$((2 * turn))" "$((20 + turn))" "$((2 * turn))"
    else
      printf '{"type":"result","subtype":"error","is_error":true,"terminal_reason":"error","session_id":"%s","result":"should-not-emit"}\n' "$emit_session"
    fi
  else
    if [ "$include_usage" -eq 1 ]; then
      printf '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":"%s","usage":{"input_tokens":%s,"output_tokens":%s},"modelUsage":{"claude-test":{"inputTokens":%s,"outputTokens":%s}},"result":"should-not-emit"}\n' "$emit_session" "$((20 + turn))" "$((2 * turn))" "$((20 + turn))" "$((2 * turn))"
    else
      printf '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":"%s","result":"should-not-emit"}\n' "$emit_session"
    fi
  fi

  case "$line" in
    *EXIT_AFTER_RESULT*) exit 92 ;;
  esac
done
