$ErrorActionPreference = 'Stop'
$sessionCounter = 0
$initMode = $env:FAKE_PEDELEC_AGENT_INIT_MODE
if ([string]::IsNullOrWhiteSpace($initMode)) { $initMode = 'ok' }
$openMode = $env:FAKE_PEDELEC_AGENT_OPEN_MODE
if ([string]::IsNullOrWhiteSpace($openMode)) { $openMode = 'ok' }
$turnMode = $env:FAKE_PEDELEC_AGENT_TURN_MODE
if ([string]::IsNullOrWhiteSpace($turnMode)) { $turnMode = 'ok' }
$closeMode = $env:FAKE_PEDELEC_AGENT_CLOSE_MODE
if ([string]::IsNullOrWhiteSpace($closeMode)) { $closeMode = 'ok' }
$provider = $env:FAKE_PEDELEC_AGENT_PROVIDER
if ([string]::IsNullOrWhiteSpace($provider)) { $provider = 'ollama' }
$serverName = $env:FAKE_PEDELEC_AGENT_SERVER_NAME
if ([string]::IsNullOrWhiteSpace($serverName)) { $serverName = 'pedelec-agent' }
$pendingTurn = $null

function Write-Frame($obj) {
    ($obj | ConvertTo-Json -Compress -Depth 12)
    [Console]::Out.Flush()
}

function Write-Result($id, $result) {
    Write-Frame @{ jsonrpc = '2.0'; id = $id; result = $result }
}

function Write-RpcError($id, $appCode, $message, $details) {
    Write-Frame @{
        jsonrpc = '2.0'
        id = $id
        error = @{
            code = -32000
            message = $message
            data = @{
                code = $appCode
                message = $message
                details = $details
            }
        }
    }
}

function Write-Notice($method, $params) {
    Write-Frame @{ jsonrpc = '2.0'; method = $method; params = $params }
}

while ($null -ne ($line = [Console]::In.ReadLine())) {
    if (-not [string]::IsNullOrWhiteSpace($env:FAKE_PEDELEC_AGENT_LOG)) {
        Add-Content -LiteralPath $env:FAKE_PEDELEC_AGENT_LOG -Value $line
    }
    $request = $line | ConvertFrom-Json
    if ($null -eq $request.method -or $null -eq $request.id) {
        continue
    }
    $method = [string]$request.method
    $id = $request.id
    if ($method -eq 'initialize') {
        if ($initMode -eq 'malformed') {
            Write-Result $id 'not-an-object'
            continue
        }
        $protocolVersion = 1
        if ($initMode -eq 'bad-version') { $protocolVersion = 99 }
        $multipleSessions = $true
        if ($initMode -eq 'no-multiple-sessions') { $multipleSessions = $false }
        $name = $serverName
        if ($initMode -eq 'wrong-name') { $name = 'not-pedelec-agent' }
        Write-Result $id @{
            protocolVersion = $protocolVersion
            serverInfo = @{ name = $name; version = 'test' }
            provider = $provider
            capabilities = @{
                multipleSessions = $multipleSessions
                assistantDelta = $true
                usage = $true
            }
        }
        continue
    }
    if ($method -eq 'session/open') {
        $requested = $request.params.sessionId
        if ($openMode -eq 'resume-mismatch') {
            Write-Result $id @{
                sessionId = 'agent-session-wrong'
                resumed = $true
                alreadyAttached = $false
                modelCapabilities = @{ tools = $true; vision = $false }
            }
            continue
        }
        if ($null -eq $requested -or [string]::IsNullOrWhiteSpace([string]$requested)) {
            $sessionCounter++
            $sessionId = "agent-session-$sessionCounter"
            $resumed = $false
        } else {
            $sessionId = [string]$requested
            $resumed = $true
        }
        Write-Result $id @{
            sessionId = $sessionId
            resumed = $resumed
            alreadyAttached = $false
            modelCapabilities = @{ tools = $true; vision = $false }
        }
        continue
    }
    if ($method -eq 'turn/start') {
        $threadId = [string]$request.params.threadId
        $sessionId = [string]$request.params.sessionId
        $turnId = [string]$request.params.turnId
        if ($turnMode -eq 'busy') {
            Write-RpcError $id 'SESSION_BUSY' 'Session already has an active turn.' @{ turnId = 'other-turn' }
            continue
        }
        if ($turnMode -eq 'timeout') {
            Start-Sleep -Seconds 30
            continue
        }
        $noticeSession = $sessionId
        $noticeTurn = $turnId
        if ($turnMode -eq 'unknown-session') { $noticeSession = 'unknown-session' }
        if ($turnMode -eq 'wrong-turn') { $noticeTurn = 'wrong-turn' }
        $startedParams = @{ threadId = $threadId; sessionId = $noticeSession; turnId = $noticeTurn }
        if ($turnMode -eq 'malformed') {
            $startedParams = @{ threadId = $threadId; turnId = $turnId }
        }
        Write-Notice 'turn/started' $startedParams
        if ($turnMode -eq 'ok') {
            Write-Notice 'turn/assistant_delta' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; text = 'hello' }
            Write-Notice 'turn/assistant_message' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; text = 'hello world' }
            Write-Notice 'turn/usage' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; usage = @{ inputTokens = 1; outputTokens = 2; totalTokens = 3 } }
            Write-Notice 'turn/tool_call' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; toolCallId = 'tool-1'; name = 'read_file' }
            Write-Notice 'turn/completed' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; status = 'completed' }
        }
        if ($turnMode -eq 'multi-round') {
            Write-Notice 'turn/assistant_delta' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; text = 'thinking ' }
            Write-Notice 'turn/tool_call' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; toolCallId = 'tool-1'; name = 'read_file' }
            Write-Notice 'turn/tool_result' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; toolCallId = 'tool-1'; ok = $true }
            Write-Notice 'turn/usage' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; usage = @{ inputTokens = 2; outputTokens = 1; totalTokens = 3 } }
            Write-Notice 'turn/assistant_delta' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; text = 'final' }
            Write-Notice 'turn/assistant_message' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; text = 'final answer' }
            Write-Notice 'turn/usage' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; usage = @{ inputTokens = 4; outputTokens = 3; totalTokens = 7 } }
            Write-Notice 'turn/completed' @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId; status = 'completed' }
        }
        if ($turnMode -eq 'pending') {
            $pendingTurn = @{ threadId = $threadId; sessionId = $sessionId; turnId = $turnId }
        }
        $alreadyStarted = $turnMode -eq 'already-started'
        Write-Result $id @{
            turnId = $turnId
            accepted = $true
            alreadyStarted = $alreadyStarted
        }
        continue
    }
    if ($method -eq 'session/close') {
        if ($closeMode -eq 'timeout') {
            Start-Sleep -Seconds 30
            continue
        }
        if ($closeMode -eq 'malformed') {
            Write-Result $id 'not-closed'
            continue
        }
        Write-Result $id @{ closed = $true }
        if ($null -ne $pendingTurn) {
            $stale = $pendingTurn
            $pendingTurn = $null
            Write-Notice 'turn/assistant_delta' @{ threadId = $stale.threadId; sessionId = $stale.sessionId; turnId = $stale.turnId; text = 'stale' }
            Write-Notice 'turn/completed' @{ threadId = $stale.threadId; sessionId = $stale.sessionId; turnId = $stale.turnId; status = 'completed' }
        }
        continue
    }
    if ($method -eq 'shutdown') {
        Write-Result $id @{ ok = $true }
        continue
    }
    Write-Result $id @{}
}
