#!/bin/sh
session_counter=0
init_mode=${FAKE_PEDELEC_AGENT_INIT_MODE:-ok}
open_mode=${FAKE_PEDELEC_AGENT_OPEN_MODE:-ok}
turn_mode=${FAKE_PEDELEC_AGENT_TURN_MODE:-ok}
close_mode=${FAKE_PEDELEC_AGENT_CLOSE_MODE:-ok}
provider=${FAKE_PEDELEC_AGENT_PROVIDER:-ollama}
server_name=${FAKE_PEDELEC_AGENT_SERVER_NAME:-pedelec-agent}
pending_thread=
pending_session=
pending_turn=

json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

while IFS= read -r line; do
  if [ -n "$FAKE_PEDELEC_AGENT_LOG" ]; then
    printf '%s\n' "$line" >> "$FAKE_PEDELEC_AGENT_LOG"
  fi
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      if [ "$init_mode" = "malformed" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":"not-an-object"}\n' "$id"
        continue
      fi
      protocol_version=1
      if [ "$init_mode" = "bad-version" ]; then protocol_version=99; fi
      multiple_sessions=true
      if [ "$init_mode" = "no-multiple-sessions" ]; then multiple_sessions=false; fi
      name="$server_name"
      if [ "$init_mode" = "wrong-name" ]; then name="not-pedelec-agent"; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":%s,"serverInfo":{"name":"%s","version":"test"},"provider":"%s","capabilities":{"multipleSessions":%s,"assistantDelta":true,"usage":true}}}\n' \
        "$id" "$protocol_version" "$(json_escape "$name")" "$(json_escape "$provider")" "$multiple_sessions"
      ;;
    session/open)
      if [ "$open_mode" = "resume-mismatch" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"agent-session-wrong","resumed":true,"alreadyAttached":false,"modelCapabilities":{"tools":true,"vision":false}}}\n' "$id"
        continue
      fi
      requested=$(printf '%s' "$line" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      if [ -z "$requested" ] || printf '%s' "$line" | grep -q '"sessionId":null'; then
        session_counter=$((session_counter + 1))
        session_id="agent-session-$session_counter"
        resumed=false
      else
        session_id="$requested"
        resumed=true
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s","resumed":%s,"alreadyAttached":false,"modelCapabilities":{"tools":true,"vision":false}}}\n' \
        "$id" "$(json_escape "$session_id")" "$resumed"
      ;;
    turn/start)
      thread_id=$(printf '%s' "$line" | sed -n 's/.*"threadId":"\([^"]*\)".*/\1/p' | head -n 1)
      session_id=$(printf '%s' "$line" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p' | head -n 1)
      turn_id=$(printf '%s' "$line" | sed -n 's/.*"turnId":"\([^"]*\)".*/\1/p' | head -n 1)
      if [ "$turn_mode" = "busy" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"Session already has an active turn.","data":{"code":"SESSION_BUSY","message":"Session already has an active turn.","details":{"turnId":"other-turn"}}}}\n' "$id"
        continue
      fi
      if [ "$turn_mode" = "timeout" ]; then
        sleep 30
        continue
      fi
      notice_session="$session_id"
      notice_turn="$turn_id"
      if [ "$turn_mode" = "unknown-session" ]; then notice_session="unknown-session"; fi
      if [ "$turn_mode" = "wrong-turn" ]; then notice_turn="wrong-turn"; fi
      if [ "$turn_mode" = "malformed" ]; then
        printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"%s","turnId":"%s"}}\n' "$(json_escape "$thread_id")" "$(json_escape "$turn_id")"
      else
        printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"%s","sessionId":"%s","turnId":"%s"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$notice_session")" "$(json_escape "$notice_turn")"
      fi
      if [ "$turn_mode" = "ok" ]; then
        printf '{"jsonrpc":"2.0","method":"turn/assistant_delta","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"hello"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/assistant_message","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"hello world"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/usage","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","usage":{"inputTokens":1,"outputTokens":2,"totalTokens":3}}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/tool_call","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","toolCallId":"tool-1","name":"read_file"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","status":"completed"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
      fi
      if [ "$turn_mode" = "multi-round" ]; then
        printf '{"jsonrpc":"2.0","method":"turn/assistant_delta","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"thinking "}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/tool_call","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","toolCallId":"tool-1","name":"read_file"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/tool_result","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","toolCallId":"tool-1","ok":true}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/usage","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3}}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/assistant_delta","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"final"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/assistant_message","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"final answer"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/usage","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","usage":{"inputTokens":4,"outputTokens":3,"totalTokens":7}}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
        printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","status":"completed"}}\n' \
          "$(json_escape "$thread_id")" "$(json_escape "$session_id")" "$(json_escape "$turn_id")"
      fi
      if [ "$turn_mode" = "pending" ]; then
        pending_thread="$thread_id"
        pending_session="$session_id"
        pending_turn="$turn_id"
      fi
      already_started=false
      if [ "$turn_mode" = "already-started" ]; then already_started=true; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"turnId":"%s","accepted":true,"alreadyStarted":%s}}\n' \
        "$id" "$(json_escape "$turn_id")" "$already_started"
      ;;
    session/close)
      if [ "$close_mode" = "timeout" ]; then
        sleep 30
        continue
      fi
      if [ "$close_mode" = "malformed" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":"not-closed"}\n' "$id"
        continue
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"closed":true}}\n' "$id"
      if [ -n "$pending_turn" ]; then
        printf '{"jsonrpc":"2.0","method":"turn/assistant_delta","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","text":"stale"}}\n' \
          "$(json_escape "$pending_thread")" "$(json_escape "$pending_session")" "$(json_escape "$pending_turn")"
        printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"%s","sessionId":"%s","turnId":"%s","status":"completed"}}\n' \
          "$(json_escape "$pending_thread")" "$(json_escape "$pending_session")" "$(json_escape "$pending_turn")"
        pending_thread=
        pending_session=
        pending_turn=
      fi
      ;;
    shutdown)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
    *)
      if [ -n "$id" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      fi
      ;;
  esac
done
