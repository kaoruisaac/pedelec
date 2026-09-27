$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8

function Append-Utf8Line([string]$Path, [string]$Text) {
    [System.IO.File]::AppendAllText($Path, $Text + [Environment]::NewLine, $utf8)
}

$sessionId = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    if ($args[$i] -eq '--resume' -and ($i + 1) -lt $args.Count) {
        $sessionId = $args[$i + 1]
        break
    }
}
if ($env:FAKE_CLAUDE_FORCE_SESSION) {
    $sessionId = $env:FAKE_CLAUDE_FORCE_SESSION
}
if ([string]::IsNullOrWhiteSpace($sessionId)) {
    $sessionId = 'claude-session-fresh'
}

if ($env:FAKE_CLAUDE_START_LOG) {
    $argsText = [string]::Join(' ', [string[]]$args)
    $argsText = $argsText -replace '[\r\n]+', ' '
    Append-Utf8Line $env:FAKE_CLAUDE_START_LOG ("pid={0} args={1}" -f $PID, $argsText)
}

$cjkGreeting = ([char]0x54C8).ToString() + ([char]0x56C9).ToString()
$turn = 0
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $turn++
    if ($env:FAKE_CLAUDE_LOG) {
        Append-Utf8Line $env:FAKE_CLAUDE_LOG $line
    }
    if ($env:FAKE_CLAUDE_MALFORMED -eq '1') {
        [Console]::Out.WriteLine('{not-json')
        [Console]::Out.Flush()
        continue
    }

    $emitSession = $sessionId
    if ($env:FAKE_CLAUDE_DRIFT_SESSION -eq '1' -and $turn -gt 1) {
        $emitSession = 'claude-session-drift'
    }

    $init = @{
        type = 'system'
        subtype = 'init'
        session_id = $emitSession
        model = 'claude-test'
        claude_code_version = '2.1.258'
    } | ConvertTo-Json -Compress -Depth 10
    [Console]::Out.WriteLine($init)
    [Console]::Out.Flush()

    if ($line -like '*CRASH_ACTIVE*') {
        [Console]::Error.WriteLine('fake Claude active crash')
        [Console]::Error.Flush()
        exit 91
    }

    $thinkingOnly = $line -like '*THINKING_ONLY*'
    $nestedText = $line -like '*NESTED_TEXT*'
    $multiText = $line -like '*MULTI_TEXT*'
    $artifactMode = if ($line -like '*ARTIFACT_TOOL*') { 'tool' }
        elseif ($line -like '*ARTIFACT_MIXED*') { 'mixed' }
        elseif ($line -like '*ARTIFACT_DUP*') { 'duplicate' }
        elseif ($line -like '*ARTIFACT_BAD*') { 'bad' }
        elseif ($line -like '*ARTIFACT_USER_INPUT*') { 'user-input' }
        elseif ($line -like '*ARTIFACT_MULTI*') { 'multi' }
        else { '' }

    if (-not $thinkingOnly) {
        $thinkingDelta = @{
            type = 'stream_event'
            event = @{
                type = 'content_block_delta'
                delta = @{ type = 'thinking_delta'; thinking = 'hidden' }
            }
        } | ConvertTo-Json -Compress -Depth 10
        [Console]::Out.WriteLine($thinkingDelta)
        $textDelta = @{
            type = 'stream_event'
            event = @{
                type = 'content_block_delta'
                delta = @{ type = 'text_delta'; text = ($cjkGreeting + '-' + $turn) }
            }
        } | ConvertTo-Json -Compress -Depth 10
        [Console]::Out.WriteLine($textDelta)
    }

    if ($nestedText) {
        $nested = '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"secret","nested":{"text":"should-not-capture"}}]}}'
        [Console]::Out.WriteLine($nested)
    } else {
        $thinkingAssistant = '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hidden-thought","signature":"sig"}]}}'
        [Console]::Out.WriteLine($thinkingAssistant)
    }

    if ($multiText) {
        $assistant = '{"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"thinking","thinking":"skip"},{"type":"text","text":"two"}]}}'
        [Console]::Out.WriteLine($assistant)
    } elseif (-not $thinkingOnly) {
        $assistant = ('{"type":"assistant","message":{"content":[{"type":"text","text":"final-' + $turn + '"}]}}')
        [Console]::Out.WriteLine($assistant)
    }

    $image = '{"type":"image","id":"image-1","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}}'
    $jpegImage = '{"type":"image","id":"image-jpeg","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/2Q=="}}'
    switch ($artifactMode) {
        'tool' {
            [Console]::Out.WriteLine('{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-9","content":[{"type":"text","text":"generated"},' + $image + ']}]}}')
        }
        'mixed' {
            [Console]::Out.WriteLine('{"type":"assistant","message":{"content":[{"type":"text","text":"with image"},' + $jpegImage + ']}}')
        }
        'duplicate' {
            [Console]::Out.WriteLine('{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":' + $image + '}}')
            [Console]::Out.WriteLine('{"type":"assistant","message":{"content":[{"type":"text","text":"with image"},' + $image + ']}}')
        }
        'bad' {
            [Console]::Out.WriteLine('{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-bad","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"%%%"}}]}]}}')
        }
        'user-input' {
            [Console]::Out.WriteLine('{"type":"user","message":{"content":[' + $image + ']}}')
        }
        'multi' {
            [Console]::Out.WriteLine('{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-multi","content":[{"type":"image","id":"multi-1","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}},{"type":"image","id":"multi-2","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQQUAAAAASUVORK5CYII="}}]}]}}')
        }
    }

    if ($env:FAKE_CLAUDE_STDERR -eq '1') {
        [Console]::Error.WriteLine('fake Claude diagnostic')
        [Console]::Error.Flush()
    }
    if ($env:FAKE_CLAUDE_DELAY_MS) {
        Start-Sleep -Milliseconds ([int]$env:FAKE_CLAUDE_DELAY_MS)
    }

    $fail = ($line -like '*FAIL_RESULT*') -or (
        $env:FAKE_CLAUDE_FAIL_PREPARE -eq '1' -and $line -like '*Session Preparation*'
    )
    $result = @{
        type = 'result'
        subtype = $(if ($fail) { 'error' } else { 'success' })
        is_error = $fail
        terminal_reason = $(if ($fail) { 'error' } else { 'completed' })
        session_id = $emitSession
        result = 'should-not-emit'
    }
    $isPrepare = $line -like '*Session Preparation*'
    if (-not $isPrepare -or $env:FAKE_CLAUDE_PREPARE_USAGE -eq '1') {
        $result.usage = @{ input_tokens = (20 + $turn); output_tokens = (2 * $turn) }
        $result.modelUsage = @{ 'claude-test' = @{ inputTokens = (20 + $turn); outputTokens = (2 * $turn) } }
    }
    $result = $result | ConvertTo-Json -Compress -Depth 10
    [Console]::Out.WriteLine($result)
    [Console]::Out.Flush()
    if ($line -like '*EXIT_AFTER_RESULT*') {
        exit 92
    }
}
