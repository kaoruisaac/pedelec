#!/bin/sh
counter=0
prompt_counter=0
auth_methods='[]'
if [ "$FAKE_ACP_AUTH" = "cursor_login" ]; then
  auth_methods='[{"id":"cursor_login","name":"Cursor Login"}]'
fi
modes='{"currentModeId":"agent-mode","availableModes":[{"id":"ask-mode","name":"Ask"},{"id":"plan-mode","name":"Plan"},{"id":"agent-mode","name":"Agent","description":"Full tool access"}]}'
parameterized_initial='[{"id":"model-picker","category":"model","type":"select","currentValue":"composer-2.5","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]}]'
parameterized_grok='[{"id":"model-picker","category":"model","type":"select","currentValue":"grok-4.7","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"reasoning-picker","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"high","name":"High"},{"value":"xhigh","name":"Extra High"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"},{"value":"false","name":"Off"}]}]'
parameterized_grok_pre_effort='[{"id":"model-picker","category":"model","type":"select","currentValue":"grok-4.7","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"reasoning-picker","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"high","name":"High"},{"value":"xhigh","name":"Extra High"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"}]}]'
parameterized_composer='[{"id":"model-picker","category":"model","type":"select","currentValue":"composer-2.5","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"},{"value":"false","name":"Off"}]}]'
parameterized_grok_high_only='[{"id":"model-picker","category":"model","type":"select","currentValue":"grok-4.7","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"reasoning-picker","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"high","name":"High"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"},{"value":"false","name":"Off"}]}]'
parameterized_grok_no_effort='[{"id":"model-picker","category":"model","type":"select","currentValue":"grok-4.7","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"},{"value":"false","name":"Off"}]}]'
parameterized_composer_no_fast='[{"id":"model-picker","category":"model","type":"select","currentValue":"composer-2.5","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]}]'
parameterized_composer_fast_only='[{"id":"model-picker","category":"model","type":"select","currentValue":"composer-2.5","options":[{"value":"composer-2.5","name":"Composer 2.5"},{"value":"grok-4.7","name":"Grok 4.7"}]},{"id":"fast","category":"model_config","type":"select","currentValue":"true","options":[{"value":"true","name":"Fast"}]}]'
if [ "$FAKE_ACP_CURSOR_CONFIG" = "unsupported_model" ]; then parameterized_initial='[{"id":"model-picker","category":"model","type":"select","currentValue":"composer-2.5","options":[{"value":"composer-2.5","name":"Composer 2.5"}]}]'; fi
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$FAKE_ACP_LOG"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":%s},"authMethods":%s}}\n' "$id" "$FAKE_ACP_LOAD" "$auth_methods" ;;
    *'"method":"authenticate"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/new"'*)
      counter=$((counter + 1)); if [ "$FAKE_ACP_PARAMETERIZED_CURSOR" = "true" ]; then printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"acp-session-%s","configOptions":%s,"modes":%s}}\n' "$id" "$counter" "$parameterized_initial" "$modes"; else printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"acp-session-%s","configOptions":[{"id":"provider-model","category":"model","type":"select","currentValue":"fake/default","options":[{"value":"fake/default","name":"Fake Default"},{"value":"fake/selected","name":"Fake Selected"}]}],"modes":%s}}\n' "$id" "$counter" "$modes"; fi ;;
    *'"method":"session/load"'*)
      load_session_id=""; if [ -n "$FAKE_ACP_LOAD_SESSION_ID" ]; then load_session_id="\"sessionId\":\"$FAKE_ACP_LOAD_SESSION_ID\","; fi
      if [ "$FAKE_ACP_PARAMETERIZED_CURSOR" = "true" ]; then printf '{"jsonrpc":"2.0","id":%s,"result":{%s"configOptions":%s,"modes":%s}}\n' "$id" "$load_session_id" "$parameterized_initial" "$modes"; else printf '{"jsonrpc":"2.0","id":%s,"result":{%s"configOptions":[{"id":"provider-model","category":"model","type":"select","currentValue":"fake/default","options":[{"value":"fake/default","name":"Fake Default"},{"value":"fake/selected","name":"Fake Selected"}]}],"modes":%s}}\n' "$id" "$load_session_id" "$modes"; fi ;;
    *'"method":"session/set_mode"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*)
      if [ "$FAKE_ACP_PARAMETERIZED_CURSOR" = "true" ]; then
        configs="$parameterized_grok_pre_effort"
        case "$FAKE_ACP_CURSOR_CONFIG" in
          unsupported_effort) configs="$parameterized_grok_high_only" ;;
          missing_effort) configs="$parameterized_grok_no_effort" ;;
        esac
        case "$line" in
          *'"configId":"reasoning-picker"'*) configs="$parameterized_grok" ;;
          *'"value":"composer-2.5"'*) configs="$parameterized_composer"; [ "$FAKE_ACP_CURSOR_CONFIG" = "missing_fast" ] && configs="$parameterized_composer_no_fast"; [ "$FAKE_ACP_CURSOR_CONFIG" = "unsupported_fast" ] && configs="$parameterized_composer_fast_only" ;;
        esac
        printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":%s}}\n' "$id" "$configs"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      fi ;;
    *'"method":"session/prompt"'*)
      prompt_counter=$((prompt_counter + 1))
      prompt_total=10
      if [ "${FAKE_ACP_USAGE_SEQUENCE:-}" = "10,20" ] && [ "$prompt_counter" -ge 2 ]; then
        prompt_total=20
      fi
      session_id=$(printf '%s' "$line" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      case "$line" in *'"text":"malformed"'*) printf 'not-json\n'; exit 7 ;; esac
      case "$line" in
        *'"text":"wait-for-cancel"'*)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","messageId":"cancel-message","content":{"type":"text","text":"before cancel"}}}}\n' "$session_id"
          IFS= read -r cancel_line
          printf '%s\n' "$cancel_line" >> "$FAKE_ACP_LOG"
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$id"
          continue ;;
        *'"text":"empty-success"'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
          continue ;;
      esac
      printf 'fake ACP diagnostic\n' >&2
      case "$line" in
        *'"text":"concurrent-'*) ;;
        *)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","messageId":"message-1","content":{"type":"text","text":"hello "}}}}\n' "$session_id"
          printf '{"jsonrpc":"2.0","id":"permission-1","method":"session/request_permission","params":{"sessionId":"%s","toolCall":{"toolCallId":"tool-1","locations":[{"path":"%s"}]},"options":[{"optionId":"provider-allow","name":"Allow","kind":"allow_once"},{"optionId":"provider-reject","name":"Reject","kind":"reject_once"}]}}\n' "$session_id" "$FAKE_ACP_WORKSPACE"
          IFS= read -r permission_response
          printf '%s\n' "$permission_response" >> "$FAKE_ACP_LOG" ;;
      esac
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","messageId":"message-1","content":{"type":"text","text":"world"}}}}\n' "$session_id"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"usage_update","used":4,"size":100}}}\n' "$session_id"
      if [ "${FAKE_ACP_USAGE:-0}" = "1" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"inputTokens":4,"outputTokens":3,"thoughtTokens":1,"cachedReadTokens":2,"cachedWriteTokens":0,"totalTokens":%s}}}\n' "$id" "$prompt_total"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      fi ;;
    *) ;;
  esac
done
