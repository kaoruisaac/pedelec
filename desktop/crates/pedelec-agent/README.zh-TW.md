# pedelec-agent 使用說明

`pedelec-agent` 是 Pedelec Desktop 管理的 persistent local server。它以 JSON-RPC 執行，透過 Ollama 呼叫本機模型，並以 read-only 工具讀取指定 sandbox 內的文字檔，或透過 `pedelec-cli` 呼叫 Pedelec host app tools。

同一 process 可承載多個 AgentSession；不同 session 可同時執行 turn，同一 session 同時間最多一個 active turn。stdout 只能出現 JSON-RPC 2.0 frames；diagnostics 走 stderr。

MVP 不會修改、刪除、搬移檔案，也不會執行任意 shell command。

## Architecture

Pedelec Desktop 啟動並管理：

```bash
pedelec-agent serve --provider ollama
```

這不是一般使用者面對的 chat CLI。Client 是 Desktop persistent runtime：先 `initialize`，再為每個 Core thread `session/open`、`turn/start`、`session/close`；process 結束時 `shutdown`。

同一 generation / process 可同時承載多個 session。Ollama settings 更新會 retire 目前 generation，下一次 prepare/turn 才會 lazy spawn 下一支 process。

## 建置

在 repo 根目錄執行：

```bash
cargo build --manifest-path desktop/Cargo.toml -p pedelec-agent
```

## 啟動

唯一 production 入口：

```bash
OLLAMA_API_KEY=ollama pedelec-agent serve --provider ollama
```

或在 repo 內：

```bash
OLLAMA_API_KEY=ollama \
cargo run --manifest-path desktop/Cargo.toml -p pedelec-agent -- \
  serve --provider ollama
```

Transport 是 stdin/stdout 上的 JSON Lines JSON-RPC 2.0。

已移除、且不會再接受：

- `pedelec-agent run`
- 從 stdin 一次讀完整 prompt
- `--session-id` / `--sandbox` / `--model` / `--jsonl` one-shot 參數
- 舊 `AgentEvent` JSONL stdout protocol

## Protocol

Client 必須先 `initialize`（ownerless），再對個別 thread 做 session/turn RPCs。

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"pedelec-desktop"}}}
```

```json
{"jsonrpc":"2.0","id":2,"method":"session/open","params":{
  "threadId":"t000001",
  "sessionId":null,
  "model":"qwen2.5-coder:7b",
  "workspacePath":".",
  "hostInstructions":"..."
}}
```

`sessionId` 為 `null` 時建立新 session；傳既有 UUID v7 則 resume。選中的 model 必須支援 tool calling，否則 `session/open` 失敗。vision 為 optional。

Turn 使用 Core `local_turn_id`，server 不會另造 provider turn ID：

```json
{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{
  "threadId":"t000001",
  "sessionId":"0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176",
  "turnId":"local_...",
  "message":"請讀取 README.md 並整理重點"
}}
```

成功 admission 的 wire 順序固定為：

1. `turn/start` success response
2. `turn/started` notification
3. 之後才會出現 async `turn/assistant_delta` / `turn/usage` / `turn/tool_call` / `turn/tool_result` / `turn/assistant_message` / `turn/completed`

`session/close` 只 detach runtime attachment，不會刪除 durable session：

```json
{"jsonrpc":"2.0","id":4,"method":"session/close","params":{"threadId":"t000001","sessionId":"0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176"}}
```

結束 process：

```json
{"jsonrpc":"2.0","id":5,"method":"shutdown","params":{}}
```

Developer 可用以上 JSON-RPC line 手動測 protocol，但不要把它當成一般 user-facing chat CLI。

## Sessions

- 同一 process 可有多個 session。
- 每個 session 同時間最多一個 active turn。
- Durable history 是 committed-turn records，不是舊的 message-by-message transcript。
- 舊 session schema / 舊 transcript format 不相容，也不做 migration。若目錄中有舊資料，請建立新 session。

資料固定寫在：

```txt
~/.pedelec/
  agent/
    sessions/
      YYYY/
        MM/
          <uuid-v7>/
            session.json
            transcript.jsonl
```

`YYYY/MM` 由 UUID v7 內含 timestamp 的 UTC 年月推導。resume 時會直接用 UUID v7 定位 session 目錄。

## Ollama

- 選中的 model 必須支援 tools；`session/open` 會硬性檢查。
- vision 為 optional；有能力時才提供圖像工具。
- endpoint / timeout / credentials 屬於 server generation config。Ollama Base URL 與 Timeout 讀取 `~/.pedelec/settings.json` 的 `providerSettings.ollama`，缺少欄位時使用內建預設值。
- Ollama API key 只從 process env 的 `OLLAMA_API_KEY` 讀取。使用本機 Ollama 時仍需提供任意非空值，例如 `ollama`。
- `TAVILY_API_KEY` 為選填 process env；有 key 時才會提供 `web.search`。
- Desktop 更新 Ollama settings 會 retire 目前 generation，不會立刻 spawn 下一支 process。

可選 `.env.local` 僅用於 runtime limits，例如：

```dotenv
PEDELEC_AGENT_PROVIDER=ollama
PEDELEC_AGENT_MAX_TRANSCRIPT_BYTES=1048576
PEDELEC_AGENT_MAX_TOOL_ROUNDS=8
PEDELEC_CLI_PATH=
PEDELEC_CORE_RUNTIME_FILE=
PEDELEC_AGENT_PEDELEC_CLI_TIMEOUT_MS=60000
```

使用 Ollama Cloud 時，`~/.pedelec/settings.json` 的 `providerSettings.ollama.baseUrl` 應為 `https://ollama.com`，不可包含 `/api`。

## 限制

- 只支援 Ollama provider。
- stdout 只輸出 JSON-RPC；不要把 diagnostics 寫進 stdout。
- 不會修改檔案。
- 不會讀取 sandbox 以外的路徑。
- `bash` 是受限 command runner，只允許 `pedelec-cli --thread-id <pedelec_thread_id> tool-spec` 與 `pedelec-cli --thread-id <pedelec_thread_id> tool-call`，不開放任意 shell。
