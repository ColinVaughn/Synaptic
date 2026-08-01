param(
    [string]$Synaptic = ".\target\release\synaptic.exe",
    [string]$RepositoryRoot = ".",
    [string]$RepositoryClaim = "https://github.com/ColinVaughn/Synaptic"
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$exePath = (Resolve-Path -LiteralPath $Synaptic).Path
$graphPath = (Resolve-Path -LiteralPath (Join-Path $repoRoot "synaptic-out\graph.json")).Path
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "synaptic-memory-live-" + [guid]::NewGuid().ToString("N")
)
$peerRepo = Join-Path $tempRoot "peer"
$bundlePath = Join-Path $tempRoot "team-memory.json"
$null = New-Item -ItemType Directory -Path $peerRepo -Force

function Assert-LastExit([string]$Operation) {
    if ($LASTEXITCODE -ne 0) {
        throw "$Operation failed with exit code $LASTEXITCODE"
    }
}

# ProcessStartInfo.ArgumentList is absent on Windows PowerShell 5.1. Apply the
# Windows command-line quoting rules so this validation also works from paths
# containing spaces.
function ConvertTo-NativeArgument([string]$Value) {
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }
    $builder = [Text.StringBuilder]::new()
    $null = $builder.Append('"')
    $backslashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') {
            $backslashes += 1
        }
        elseif ($character -eq '"') {
            $null = $builder.Append(('\' * (($backslashes * 2) + 1)))
            $null = $builder.Append('"')
            $backslashes = 0
        }
        else {
            $null = $builder.Append(('\' * $backslashes))
            $null = $builder.Append($character)
            $backslashes = 0
        }
    }
    $null = $builder.Append(('\' * ($backslashes * 2)))
    $null = $builder.Append('"')
    return $builder.ToString()
}

function Start-Mcp([string[]]$ServerArguments) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $exePath
    $startInfo.Arguments = (
        $ServerArguments |
            ForEach-Object { ConvertTo-NativeArgument $_ }
    ) -join " "
    $startInfo.WorkingDirectory = $repoRoot
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.CreateNoWindow = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "failed to start MCP process"
    }
    return $process
}

function Write-Mcp($Process, $Payload) {
    $line = $Payload | ConvertTo-Json -Depth 30 -Compress
    $Process.StandardInput.WriteLine($line)
    $Process.StandardInput.Flush()
}

function Read-Mcp($Process) {
    $line = $Process.StandardOutput.ReadLine()
    if ([string]::IsNullOrWhiteSpace($line)) {
        $stderr = $Process.StandardError.ReadToEnd()
        throw "MCP closed without a response: $stderr"
    }
    return $line | ConvertFrom-Json
}

function Initialize-Mcp($Process, [string]$ClientName) {
    Write-Mcp $Process ([ordered]@{
        jsonrpc = "2.0"
        id = 1
        method = "initialize"
        params = [ordered]@{
            protocolVersion = "2025-11-25"
            capabilities = @{}
            clientInfo = [ordered]@{ name = $ClientName; version = "1.0" }
        }
    })
    $response = Read-Mcp $Process
    if ($response.result.protocolVersion -ne "2025-11-25") {
        throw "MCP protocol negotiation failed"
    }
    Write-Mcp $Process ([ordered]@{
        jsonrpc = "2.0"
        method = "notifications/initialized"
    })
    return $response
}

$script:requestId = 10

function Invoke-Mcp($Process, [string]$ToolName, $Arguments) {
    $script:requestId += 1
    Write-Mcp $Process ([ordered]@{
        jsonrpc = "2.0"
        id = $script:requestId
        method = "tools/call"
        params = [ordered]@{ name = $ToolName; arguments = $Arguments }
    })
    return Read-Mcp $Process
}

function Get-McpTools($Process) {
    $script:requestId += 1
    Write-Mcp $Process ([ordered]@{
        jsonrpc = "2.0"
        id = $script:requestId
        method = "tools/list"
        params = @{}
    })
    return Read-Mcp $Process
}

function Stop-Mcp($Process) {
    if ($null -eq $Process) {
        return
    }
    try {
        $Process.StandardInput.Close()
    }
    catch {
        # The process may already have exited after a failed assertion.
    }
    if (-not $Process.WaitForExit(3000)) {
        $Process.Kill()
        $Process.WaitForExit()
    }
    $Process.Dispose()
}

$operator = $null
$reviewer = $null
try {
    $exportOutput = (
        & $exePath memory export `
            --root $repoRoot `
            --output $bundlePath `
            --principal reviewer `
            --repository-claim $RepositoryClaim |
            Out-String
    ).Trim()
    Assert-LastExit "memory export"

    $syncOutput = (
        & $exePath memory sync `
            --root $peerRepo `
            --bundle $bundlePath `
            --principal reviewer `
            --repository-claim $RepositoryClaim |
            Out-String
    ).Trim()
    Assert-LastExit "memory sync"

    $peerStore = Join-Path $peerRepo ".synaptic\memory"
    if (-not (Test-Path -LiteralPath $peerStore)) {
        throw "peer store was not created"
    }

    $operator = Start-Mcp @(
        "serve",
        "--graph", $graphPath,
        "--source-root", $repoRoot,
        "--immutable-graph",
        "--allow-memory-write",
        "--memory-principal", "validator",
        "--memory-repository-claim", $RepositoryClaim,
        "--memory-allow-private"
    )
    $operatorInit = Initialize-Mcp $operator "repository-memory-live-writer"
    $operatorTools = Get-McpTools $operator
    $writerTool = $operatorTools.result.tools |
        Where-Object { $_.name -eq "record_change_outcome" }
    if ($null -eq $writerTool) {
        throw "writable MCP did not advertise record_change_outcome"
    }

    # One uninterrupted token prevents the full-text search from also matching
    # unrelated public records that merely mention common words like "MCP".
    $token = "memoryprivacy" + [guid]::NewGuid().ToString("N")
    $recordArguments = [ordered]@{
        idempotency_key = $token
        title = "Final repository-memory validation $token"
        summary = "Release MCP validation passed after the complete repository-memory TDD suite."
        outcome = "succeeded"
        source_uri = "agent://validation/$token"
        affected_symbols = @(
            "MemoryStore",
            "ingest_artifact_file",
            "refresh_repository_memory"
        )
        verification_status = "passed"
        verification_commands = @(
            "cargo test --workspace",
            "cargo build --release -p synaptic"
        )
        scope = "private"
    }
    $firstWrite = Invoke-Mcp $operator "record_change_outcome" $recordArguments
    $retryWrite = Invoke-Mcp $operator "record_change_outcome" $recordArguments
    $changedArguments = [ordered]@{}
    foreach ($entry in $recordArguments.GetEnumerator()) {
        $changedArguments[$entry.Key] = $entry.Value
    }
    $changedArguments.summary = "Changed payload must conflict with the immutable prior outcome."
    $conflictWrite = Invoke-Mcp $operator "record_change_outcome" $changedArguments
    if ($firstWrite.result.structuredContent.write -ne "created") {
        throw "first write was not created"
    }
    if ($retryWrite.result.structuredContent.write -ne "already_present") {
        throw "retry was not idempotent"
    }
    if ($conflictWrite.result.isError -ne $true) {
        throw "changed payload did not conflict"
    }

    $predict = Invoke-Mcp $operator "predict_impact" ([ordered]@{
        files = @(
            "crates/synaptic-memory/src/store.rs",
            "crates/synaptic-memory/src/artifact.rs"
        )
    })
    $working = Invoke-Mcp $operator "working_changes_impact" ([ordered]@{
        base = "HEAD"
        limit = 20
        verbose = $false
    })
    $decision = Invoke-Mcp $operator "explain_decision" ([ordered]@{
        subject = "MemoryStore"
        limit = 5
    })
    if ([int]$predict.result.structuredContent.memory_evidence.total -lt 1) {
        throw "predict_impact returned no memory evidence"
    }
    if ([int]$working.result.structuredContent.memory_evidence.total -lt 1) {
        throw "working_changes_impact returned no memory evidence"
    }
    if ([int]$decision.result.structuredContent.total -lt 1) {
        throw "explain_decision returned no decision"
    }
    $decisionText = [string]$decision.result.content[0].text
    if (-not $decisionText.Contains("docs/adr/001-repository-memory-overlay.md")) {
        throw "explain_decision did not cite the ADR source"
    }
    Stop-Mcp $operator
    $operator = $null

    $reviewer = Start-Mcp @(
        "serve",
        "--graph", $graphPath,
        "--source-root", $repoRoot,
        "--immutable-graph",
        "--memory-peer", $peerStore,
        "--memory-principal", "reviewer",
        "--memory-repository-claim", $RepositoryClaim
    )
    $reviewerInit = Initialize-Mcp $reviewer "repository-memory-live-reviewer"
    $reviewerTools = Get-McpTools $reviewer
    $reviewerWriter = $reviewerTools.result.tools |
        Where-Object { $_.name -eq "record_change_outcome" }
    if ($null -ne $reviewerWriter) {
        throw "read-only MCP advertised record_change_outcome"
    }
    $federatedDecision = Invoke-Mcp $reviewer "search_memory" ([ordered]@{
        query = "temporal overlay"
        symbol = "MemoryStore"
        kinds = @("architecture_decision")
        limit = 5
    })
    $privateSearch = Invoke-Mcp $reviewer "search_memory" ([ordered]@{
        query = $token
        limit = 5
    })
    if ([int]$federatedDecision.result.structuredContent.total -ne 1) {
        throw "federation did not deduplicate identical records"
    }
    if ([int]$privateSearch.result.structuredContent.total -ne 0) {
        throw "private record leaked to reviewer"
    }

    [ordered]@{
        protocol_version = $operatorInit.result.protocolVersion
        writable_tool_count = $operatorTools.result.tools.Count
        writer_read_only_hint = $writerTool.annotations.readOnlyHint
        first_write = $firstWrite.result.structuredContent.write
        idempotent_retry = $retryWrite.result.structuredContent.write
        immutable_conflict_rejected = $conflictWrite.result.isError
        private_owner = $firstWrite.result.structuredContent.record.owner
        predict_evidence_total = $predict.result.structuredContent.memory_evidence.total
        predict_evidence_subjects = $predict.result.structuredContent.memory_evidence.subjects
        working_changes_evidence_total = $working.result.structuredContent.memory_evidence.total
        decision_total = $decision.result.structuredContent.total
        decision_cites_adr = $decisionText.Contains(
            "docs/adr/001-repository-memory-overlay.md"
        )
        exported = $exportOutput
        synced = $syncOutput
        readonly_tool_count = $reviewerTools.result.tools.Count
        readonly_writer_absent = ($null -eq $reviewerWriter)
        federated_decision_total = $federatedDecision.result.structuredContent.total
        private_visible_to_reviewer = $privateSearch.result.structuredContent.total
        reviewer_protocol_version = $reviewerInit.result.protocolVersion
    } | ConvertTo-Json -Depth 12
}
finally {
    Stop-Mcp $operator
    Stop-Mcp $reviewer
    $resolvedTempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $resolvedTarget = [IO.Path]::GetFullPath($tempRoot)
    $isValidationTemp = (
        $resolvedTarget.StartsWith(
            $resolvedTempBase,
            [StringComparison]::OrdinalIgnoreCase
        ) -and
        (Split-Path $resolvedTarget -Leaf).StartsWith("synaptic-memory-live-")
    )
    if ($isValidationTemp) {
        Remove-Item -LiteralPath $resolvedTarget -Recurse -Force -ErrorAction SilentlyContinue
    }
}
