# pedelec-agent 使用說明

`pedelec-agent` 是 Pedelec 的輕量 Native Provider。它可以獨立從終端機執行，透過 Ollama 呼叫本機模型，並以 read-only 工具讀取指定 sandbox 內的文字檔，或透過 `pedelec-cli` 呼叫 Pedelec host app tools。

MVP 不會修改、刪除、搬移檔案，也不會執行任意 shell command。stdout 永遠只輸出 JSONL。

## 建置

在 repo 根目錄執行：

```bash
cargo build --manifest-path desktop/tauri/Cargo.toml --bin pedelec-agent
```

## 基本用法

建立新 session 時不傳 session ID，`pedelec-agent` 會自行產生 UUID v7：

```bash
printf '%s' '請讀取 README.md 並整理重點' | \
cargo run --manifest-path desktop/tauri/Cargo.toml --bin pedelec-agent -- \
  --sandbox .
```

也可以保留 `run` 作為相容入口：

```bash
printf '%s' '請列出這個 sandbox 內有哪些文字檔' | \
cargo run --manifest-path desktop/tauri/Cargo.toml --bin pedelec-agent -- \
  run \
  --sandbox .
```

若已經 build 出 binary：

```bash
printf '%s' '請讀 README.md' | pedelec-agent --sandbox .
```

resume 既有 session 時使用 `--session-id`：

```bash
printf '%s' '繼續剛才的分析' | pedelec-agent \
  --session-id 0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176 \
  --sandbox .
```

## 常用選項

```bash
pedelec-agent [run] \
  --session-id <uuid-v7> \
  --sandbox <path> \
  --provider ollama \
  --model <model> \
  --jsonl \
  --env-file .env.local \
  --pedelec-cli <path> \
  --core-runtime-file <path>
```

| 選項 | 說明 |
| --- | --- |
| stdin | 使用者訊息，從 stdin 讀取至 EOF。 |
| `--session-id` | optional。提供既有 UUID v7 時 resume；省略時建立新 session。 |
| `--sandbox` | 限制 agent 只能讀取此目錄內的檔案。 |
| `--provider` | MVP 支援 `ollama`。 |
| `--model` | Ollama model name，例如 `qwen2.5-coder:7b`。 |
| `--jsonl` | 保留參數；stdout 永遠都是 JSONL。 |
| `--env-file` | 指定 env file，預設 `.env.local`。 |
| `--pedelec-cli` | 指定 `pedelec-cli` 路徑。 |
| `--core-runtime-file` | 呼叫 `pedelec-cli` 時傳入的 runtime file。 |

舊格式 `pedelec-agent run "prompt"`、`pedelec-agent "prompt"`、`pedelec-agent run <session_id> "prompt"` 與 `pedelec-agent <session_id> "prompt"` 不再是正式格式。Prompt 必須透過 stdin 傳入。

## `.env.local` 範例

```dotenv
PEDELEC_AGENT_PROVIDER=ollama
PEDELEC_AGENT_MODEL=qwen2.5-coder:7b

PEDELEC_AGENT_SANDBOX=.
PEDELEC_AGENT_MAX_TRANSCRIPT_BYTES=1048576
PEDELEC_AGENT_MAX_TOOL_ROUNDS=8

PEDELEC_CLI_PATH=
PEDELEC_CORE_RUNTIME_FILE=
PEDELEC_AGENT_PEDELEC_CLI_TIMEOUT_MS=60000
```

設定優先序：

```txt
CLI arguments > process env > .env.local > internal default
```

Ollama Base URL 與 Timeout 不使用 `.env.local` 或 process env；`pedelec-agent` 會直接讀取 `~/.pedelec/settings.json` 的 `providerSettings.ollama`，缺少欄位時使用內建預設值。

Ollama API key 只從 process env 的 `OLLAMA_API_KEY` 讀取，不從 `.env.local` 讀取，也沒有 CLI 參數。使用本機 Ollama 時仍需提供任意非空值，例如：

```bash
OLLAMA_API_KEY=ollama pedelec-agent --sandbox . --provider ollama --model qwen2.5-coder:7b
```

使用 Ollama Cloud 時，`~/.pedelec/settings.json` 的 `providerSettings.ollama.baseUrl` 應為 `https://ollama.com`，不可包含 `/api`；`pedelec-agent` 會呼叫 `{baseUrl}/api/chat` 並送出 `Authorization: Bearer <OLLAMA_API_KEY>`。

session 儲存位置不可透過 CLI 或環境變數覆寫，固定由 Pedelec home 推導。

## Session 與 Transcript

資料固定寫在：

```txt
~/.pedelec/
  pedelec-agent/
    sessions/
      YYYY/
        MM/
          <uuid-v7>/
            session.json
            transcript.jsonl
            events.jsonl
```

`YYYY/MM` 由 UUID v7 內含 timestamp 的 UTC 年月推導。resume 時會直接用 UUID v7 定位 session 目錄，不掃描 session tree，也不使用 index。

resume 時會沿用既有 `provider`、`model` 與 `sandboxPath`；如果 CLI 傳入不同 sandbox/model/provider，會回傳錯誤。

## JSONL stdout 範例

stdout 每一行都是一個 JSON object：

```jsonl
{"type":"session","sessionId":"0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176","resumed":false}
{"type":"status","status":"running"}
{"type":"tool_call","tool":"fs.read_text_file","args":{"path":"README.md"}}
{"type":"tool_result","tool":"fs.read_text_file","ok":true,"result":{"path":"README.md","text":"...","truncated":false}}
{"type":"assistant_message","text":"README.md 的重點是..."}
{"type":"status","status":"done"}
{"type":"done"}
```

錯誤也會用 JSONL 輸出：

```jsonl
{"type":"error","error":{"code":"CONFIG_ERROR","message":"Model is required"}}
```

## 範例

讀取 sandbox 內的 README：

```bash
printf '%s' '請讀取 README.md 並用條列整理重點' | \
OLLAMA_API_KEY=ollama \
pedelec-agent \
  --sandbox .
```

指定 Ollama model：

```bash
printf '%s' '請閱讀 sdk/src/index.ts 並說明 Pedelec SDK 的主要 API' | \
OLLAMA_API_KEY=ollama \
pedelec-agent \
  --sandbox . \
  --provider ollama \
  --model qwen2.5-coder:7b
```

透過 `pedelec-cli` 呼叫 host app tool：

```bash
printf '%s' '請呼叫 get_current_page 並整理目前頁面資訊' | \
OLLAMA_API_KEY=ollama \
pedelec-agent \
  --sandbox . \
  --pedelec-cli ./desktop/tauri/target/debug/pedelec-cli \
  --core-runtime-file ~/.pedelec/runtime.json
```

## 限制

- 只支援 Ollama provider。
- 不支援 streaming；完成後輸出 `assistant_message`。
- Prompt 只支援 stdin。
- 不會修改檔案。
- 不會讀取 sandbox 以外的路徑。
- `bash` 是受限 command runner，只允許 `pedelec-cli tool-spec` 與 `pedelec-cli tool-call`，不開放任意 shell。
