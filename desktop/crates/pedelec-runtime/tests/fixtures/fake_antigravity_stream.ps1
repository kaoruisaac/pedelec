$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8

function Append-Utf8Line([string]$Path, [string]$Text) {
    [System.IO.File]::AppendAllText($Path, $Text + [Environment]::NewLine, $utf8)
}

$conversationId = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    if ($args[$i] -eq '--conversation' -and ($i + 1) -lt $args.Count) {
        $conversationId = $args[$i + 1]
        break
    }
}
if ([string]::IsNullOrWhiteSpace($conversationId)) {
    $conversationId = 'agy-conversation-fresh'
    $fresh = $true
} else {
    $fresh = $false
}

$agentPath = Join-Path (Get-Location) '.agents/agents/pedelec-runtime/agent.md'
$agentExists = Test-Path -LiteralPath $agentPath
if ($env:FAKE_AGY_START_LOG) {
    $argsText = [string]::Join(' ', [string[]]$args)
    Append-Utf8Line $env:FAKE_AGY_START_LOG ("pid={0} agent={1} args={2}" -f $PID, $agentExists.ToString().ToLowerInvariant(), $argsText)
}
if ($env:FAKE_AGY_REQUIRE_AGENT_FILE -eq '1' -and -not $agentExists) {
    [Console]::Error.WriteLine('missing pedelec-runtime custom agent')
    [Console]::Error.Flush()
    exit 90
}

# Build the CJK payload from code points so Windows PowerShell 5.x source-file
# decoding cannot corrupt the fixture before Pedelec reads provider stdout.
$cjkGreeting = ([char]0x54C8).ToString() + ([char]0x56C9).ToString()
$turn = 0
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $turn++
    if ($env:FAKE_AGY_LOG) {
        Append-Utf8Line $env:FAKE_AGY_LOG $line
    }
    if ($env:FAKE_AGY_MALFORMED -eq '1') {
        [Console]::Out.WriteLine('{not-json')
        [Console]::Out.Flush()
        continue
    }
    $request = $line | ConvertFrom-Json
    if ($fresh -and $turn -eq 1) {
        $init = @{ event = 'init'; conversation_id = $conversationId } | ConvertTo-Json -Compress -Depth 10
        [Console]::Out.WriteLine($init)
        [Console]::Out.Flush()
    }
    if ($request.message.content -like '*CRASH_ACTIVE*') {
        [Console]::Error.WriteLine('fake Antigravity active crash')
        [Console]::Error.Flush()
        exit 91
    }
    $delta = @{
        event = 'step_update'
        step_type = 'agent_response'
        text_delta = ($cjkGreeting + '-' + $turn)
        usage = @{ input_tokens = (10 + $turn); output_tokens = $turn }
    } | ConvertTo-Json -Compress -Depth 10
    [Console]::Out.WriteLine($delta)
    if ($env:FAKE_AGY_STDERR -eq '1') {
        [Console]::Error.WriteLine('fake Antigravity diagnostic')
        [Console]::Error.Flush()
    }
    if ($env:FAKE_AGY_DELAY_MS) {
        Start-Sleep -Milliseconds ([int]$env:FAKE_AGY_DELAY_MS)
    }
    $failPrepare = $request.message.content -like '*FAIL_PREPARE*' -and $request.message.content -like '*[[]Session Preparation[]]*'
    $status = if ($request.message.content -like '*FAIL_RESULT*' -or $failPrepare) { 'ERROR' } else { 'SUCCESS' }
    $result = @{
        event = 'result'
        result = @{
            status = $status
            conversation_id = $conversationId
            response = ('final-' + $turn)
            error = if ($status -eq 'SUCCESS') { $null } else { @{ code = 'fake_failure'; message = 'requested failure' } }
        }
    }
    $isPrepare = $request.message.content -like '*Session Preparation*'
    if (-not $isPrepare -or $env:FAKE_AGY_PREPARE_USAGE -eq '1') {
        $result.result.usage = @{
            input_tokens = (20 + $turn)
            output_tokens = (2 * $turn)
            total_tokens = (25 + (($turn - 1) * 3))
        }
    }
    $result = $result | ConvertTo-Json -Compress -Depth 10
    [Console]::Out.WriteLine($result)
    [Console]::Out.Flush()
    if ($request.message.content -like '*EXIT_AFTER_RESULT*') {
        exit 92
    }
}
