$counter = 0
$promptCounter = 0
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8NoBom
$supportsLoad = $env:FAKE_ACP_LOAD -eq 'true'
$authMethods = if ($env:FAKE_ACP_AUTH -eq 'cursor_login') { @(@{ id = 'cursor_login'; name = 'Cursor Login' }) } else { @() }
$modes = @{
    currentModeId = 'agent-mode'
    availableModes = @(
        @{ id = 'ask-mode'; name = 'Ask' },
        @{ id = 'plan-mode'; name = 'Plan' },
        @{ id = 'agent-mode'; name = 'Agent'; description = 'Full tool access' }
    )
}
while ($null -ne ($line = [Console]::In.ReadLine())) {
    [System.IO.File]::AppendAllText($env:FAKE_ACP_LOG, $line + [Environment]::NewLine, $utf8NoBom)
    $request = $line | ConvertFrom-Json
    if ($request.method -eq 'initialize') {
        $result = @{
            protocolVersion = 1
            agentCapabilities = @{ loadSession = $supportsLoad }
            authMethods = $authMethods
        }
    } elseif ($request.method -eq 'session/new') {
        $counter++
        $result = @{ sessionId = "acp-session-$counter"; configOptions = @(@{ id = 'provider-model'; category = 'model'; type = 'select'; currentValue = 'fake/default'; options = @(@{ value = 'fake/default'; name = 'Fake Default' }, @{ value = 'fake/selected'; name = 'Fake Selected' }) }); modes = $modes }
    } elseif ($request.method -eq 'session/load') {
        $result = @{ configOptions = @(@{ id = 'provider-model'; category = 'model'; type = 'select'; currentValue = 'fake/default'; options = @(@{ value = 'fake/default'; name = 'Fake Default' }, @{ value = 'fake/selected'; name = 'Fake Selected' }) }); modes = $modes }
        if (-not [string]::IsNullOrWhiteSpace($env:FAKE_ACP_LOAD_SESSION_ID)) {
            $result.sessionId = $env:FAKE_ACP_LOAD_SESSION_ID
        }
    } elseif ($request.method -eq 'session/prompt') {
        $promptCounter++
        $promptTotal = 10
        if ($env:FAKE_ACP_USAGE_SEQUENCE -eq '10,20' -and $promptCounter -ge 2) {
            $promptTotal = 20
        }
        $sessionId = $request.params.sessionId
        if ($request.params.prompt[0].text -eq 'malformed') {
            [Console]::Out.WriteLine('not-json')
            [Console]::Out.Flush()
            exit 7
        }
        if ($request.params.prompt[0].text -eq 'wait-for-cancel') {
            @{
                jsonrpc = '2.0'
                method = 'session/update'
                params = @{
                    sessionId = $sessionId
                    update = @{
                        sessionUpdate = 'agent_message_chunk'
                        messageId = 'cancel-message'
                        content = @{ type = 'text'; text = 'before cancel' }
                    }
                }
            } | ConvertTo-Json -Compress -Depth 12 | Write-Output
            [Console]::Out.Flush()
            $cancel = [Console]::In.ReadLine()
            [System.IO.File]::AppendAllText($env:FAKE_ACP_LOG, $cancel + [Environment]::NewLine, $utf8NoBom)
            $result = @{ stopReason = 'cancelled' }
            if ($null -ne $request.id) {
                @{
                    jsonrpc = '2.0'
                    id = $request.id
                    result = $result
                } | ConvertTo-Json -Compress -Depth 12 | Write-Output
                [Console]::Out.Flush()
            }
            continue
        }
        if ($request.params.prompt[0].text -eq 'empty-success') {
    $result = @{ stopReason = 'end_turn' }
    if ($env:FAKE_ACP_USAGE -eq '1') {
        $result.usage = @{
            inputTokens = 4
            outputTokens = 3
            thoughtTokens = 1
            cachedReadTokens = 2
            cachedWriteTokens = 0
            totalTokens = $promptTotal
        }
    }
            if ($null -ne $request.id) {
                @{
                    jsonrpc = '2.0'
                    id = $request.id
                    result = $result
                } | ConvertTo-Json -Compress -Depth 12 | Write-Output
                [Console]::Out.Flush()
            }
            continue
        }
        [Console]::Error.WriteLine('fake ACP diagnostic')
        [Console]::Error.Flush()
        if ($request.params.prompt[0].text -notlike 'concurrent-*') {
            @{
            jsonrpc = '2.0'
            method = 'session/update'
            params = @{
                sessionId = $sessionId
                update = @{
                    sessionUpdate = 'agent_message_chunk'
                    messageId = 'message-1'
                    content = @{ type = 'text'; text = 'hello ' }
                }
            }
        } | ConvertTo-Json -Compress -Depth 12 | Write-Output
        @{
            jsonrpc = '2.0'
            id = 'permission-1'
            method = 'session/request_permission'
            params = @{
                sessionId = $sessionId
                toolCall = @{
                    toolCallId = 'tool-1'
                    locations = @(@{ path = $env:FAKE_ACP_WORKSPACE })
                }
                options = @(
                    @{ optionId = 'provider-allow'; name = 'Allow'; kind = 'allow_once' },
                    @{ optionId = 'provider-reject'; name = 'Reject'; kind = 'reject_once' }
                )
            }
            } | ConvertTo-Json -Compress -Depth 12 | Write-Output
            [Console]::Out.Flush()
            $permissionResponse = [Console]::In.ReadLine()
            [System.IO.File]::AppendAllText($env:FAKE_ACP_LOG, $permissionResponse + [Environment]::NewLine, $utf8NoBom)
        }
        @{
            jsonrpc = '2.0'
            method = 'session/update'
            params = @{
                sessionId = $sessionId
                update = @{
                    sessionUpdate = 'agent_message_chunk'
                    messageId = 'message-1'
                    content = @{ type = 'text'; text = 'world' }
                }
            }
        } | ConvertTo-Json -Compress -Depth 12 | Write-Output
        @{
            jsonrpc = '2.0'
            method = 'session/update'
            params = @{
                sessionId = $sessionId
                update = @{ sessionUpdate = 'usage_update'; used = 4; size = 100 }
            }
        } | ConvertTo-Json -Compress -Depth 12 | Write-Output
        [Console]::Out.Flush()
        $result = @{ stopReason = 'end_turn' }
        if ($env:FAKE_ACP_USAGE -eq '1') {
            $result.usage = @{
                inputTokens = 4
                outputTokens = 3
                thoughtTokens = 1
                cachedReadTokens = 2
                cachedWriteTokens = 0
                totalTokens = $promptTotal
            }
        }
    } else {
        if ($null -eq $request.id) { continue }
        $result = @{}
    }
    if ($null -ne $request.id) {
        @{
            jsonrpc = '2.0'
            id = $request.id
            result = $result
        } | ConvertTo-Json -Compress -Depth 12 | Write-Output
        [Console]::Out.Flush()
    }
}
