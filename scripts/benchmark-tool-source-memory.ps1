param(
    [string]$SynapticExe = "target\release\synaptic.exe",
    [string]$CacheRoot = "synaptic-out\bench-tools-2026-07-30",
    [string]$OutputRoot = "synaptic-out\eval\tool-source-memory-2026-07-30",
    [int]$CommitsPerRepo = 50,
    [int]$CasesPerRepo = 10,
    [int]$EvalRepetitions = 5,
    [string]$RepositoryFilter = ""
)

$ErrorActionPreference = "Stop"
$workspace = (Resolve-Path ".").Path
$exe = (Resolve-Path $SynapticExe).Path
$cache = (Resolve-Path $CacheRoot).Path
$output = [System.IO.Path]::GetFullPath((Join-Path $workspace $OutputRoot))

if (-not $cache.StartsWith($workspace, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Benchmark cache must remain under the workspace: $cache"
}
if (-not $output.StartsWith($workspace, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Benchmark output must remain under the workspace: $output"
}

[System.IO.Directory]::CreateDirectory($output) | Out-Null
[System.IO.Directory]::CreateDirectory((Join-Path $output "manifests")) | Out-Null
[System.IO.Directory]::CreateDirectory((Join-Path $output "reports")) | Out-Null
[System.IO.Directory]::CreateDirectory((Join-Path $output "logs")) | Out-Null

# The CodeQL workload overflows Rust's default spawned-thread stack. Keep the
# same Rayon parallelism while making worker stack capacity explicit.
$env:RUST_MIN_STACK = "67108864"
$env:SYNAPTIC_MAX_NODES = "0"
$env:SYNAPTIC_MAX_GRAPH_MB = "0"

$repositories = @(
    [ordered]@{
        name = "sourcegraph-public-snapshot"
        sha = "c864f15af264f0f456a6d8a83290b5c940715349"
    },
    [ordered]@{
        name = "cody-public-snapshot"
        sha = "8e20ac6c1460c08b0db581c0204658112a246eda"
    },
    [ordered]@{
        name = "codeql"
        sha = "7bb0034f4328613ae34acde826c4c5ceafbef5ee"
    },
    [ordered]@{
        name = "joern"
        sha = "80ef1868dbe0ab23566f99dba279026a286c2019"
    },
    [ordered]@{
        name = "aider"
        sha = "5dc9490bb35f9729ef2c95d00a19ccd30c26339c"
    },
    [ordered]@{
        name = "graphify"
        sha = "4fe11092ccbe9f543608f140c790f68d5d83cae4"
    }
)
if (-not [string]::IsNullOrWhiteSpace($RepositoryFilter)) {
    $selected = [System.Collections.Generic.HashSet[string]]::new(
        [string[]]($RepositoryFilter.Split(",") | ForEach-Object { $_.Trim() }),
        [System.StringComparer]::OrdinalIgnoreCase
    )
    $repositories = @($repositories | Where-Object { $selected.Contains([string]$_.name) })
}

$stopWords = [System.Collections.Generic.HashSet[string]]::new(
    [string[]]@(
        "add", "adds", "added", "and", "are", "but", "change", "changes",
        "fix", "fixes", "fixed", "for", "from", "into", "merge", "more",
        "not", "remove", "removes", "removed", "the", "this", "use", "with"
    ),
    [System.StringComparer]::OrdinalIgnoreCase
)

function Write-Utf8NoBom {
    param([string]$Path, [string]$Content)
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Content, $encoding)
}

function Assert-Exit {
    param([string]$Operation, [int]$Code, [string]$Log)
    if ($Code -ne 0) {
        $tail = if (Test-Path $Log) {
            (Get-Content -LiteralPath $Log -Tail 30) -join [Environment]::NewLine
        } else {
            "(no log)"
        }
        throw "$Operation failed with exit code ${Code}:`n$tail"
    }
}

function Measure-Synaptic {
    param(
        [string]$Operation,
        [string[]]$Arguments,
        [string]$Log
    )
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    & $exe @Arguments *> $Log
    $code = $LASTEXITCODE
    $watch.Stop()
    Assert-Exit -Operation $Operation -Code $code -Log $Log
    return $watch.Elapsed.TotalSeconds
}

function Median {
    param([double[]]$Values)
    if ($Values.Count -eq 0) {
        return 0.0
    }
    $sorted = @($Values | Sort-Object)
    $middle = [Math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 1) {
        return [double]$sorted[$middle]
    }
    return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2.0
}

function Query-Text {
    param([string]$Subject)
    $tokens = [regex]::Matches($Subject.ToLowerInvariant(), "[a-z0-9_]{3,}") |
        ForEach-Object { $_.Value } |
        Where-Object { -not $stopWords.Contains($_) } |
        Select-Object -Unique -First 4
    if (@($tokens).Count -eq 0) {
        return $Subject
    }
    return (@($tokens) -join " ")
}

$results = [System.Collections.Generic.List[object]]::new()
$failures = [System.Collections.Generic.List[object]]::new()

foreach ($repository in $repositories) {
    $name = [string]$repository.name
    $repo = [System.IO.Path]::GetFullPath((Join-Path $cache $name))
    if (-not $repo.StartsWith($cache, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Repository escaped benchmark cache: $repo"
    }
    if (-not (Test-Path (Join-Path $repo ".git"))) {
        $failures.Add([ordered]@{ name = $name; error = "checkout missing at $repo" })
        continue
    }

    try {
        $actualSha = (& git -C $repo rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0 -or $actualSha -ne [string]$repository.sha) {
            throw "expected $($repository.sha), found $actualSha"
        }

        $memoryRoot = [System.IO.Path]::GetFullPath((Join-Path $repo ".synaptic\memory"))
        if (-not $memoryRoot.StartsWith($repo, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Memory root escaped repository: $memoryRoot"
        }
        if (Test-Path $memoryRoot) {
            Remove-Item -LiteralPath $memoryRoot -Recurse -Force
        }

        $repoLogRoot = Join-Path $output "logs\$name"
        [System.IO.Directory]::CreateDirectory($repoLogRoot) | Out-Null
        $extractSeconds = Measure-Synaptic `
            -Operation "$name extract" `
            -Arguments @("extract", $repo, "--directed", "--no-store") `
            -Log (Join-Path $repoLogRoot "extract.log")

        $graph = Join-Path $repo "synaptic-out\graph.json"
        if (-not (Test-Path $graph)) {
            throw "extract did not write $graph"
        }

        $commits = @(& git -C $repo rev-list --first-parent "--max-count=$CommitsPerRepo" HEAD)
        if ($LASTEXITCODE -ne 0 -or $commits.Count -eq 0) {
            throw "could not enumerate commits"
        }

        # `memory ingest` automatically loads `<root>/synaptic-out/graph.json`
        # when present. Stage the graph aside so this phase measures Git episode
        # persistence/path lineage rather than reparsing a large graph 50 times.
        # The graph is restored for the single semantic/document refresh below.
        $stagedGraph = "$graph.memory-benchmark-staged"
        if (Test-Path $stagedGraph) {
            Remove-Item -LiteralPath $stagedGraph -Force
        }
        Move-Item -LiteralPath $graph -Destination $stagedGraph
        $ingestWatch = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            foreach ($commit in $commits) {
                $log = Join-Path $repoLogRoot "ingest.log"
                & $exe memory ingest $commit --root $repo *>> $log
                Assert-Exit -Operation "$name ingest $commit" -Code $LASTEXITCODE -Log $log
            }
        } finally {
            $ingestWatch.Stop()
            if (Test-Path $stagedGraph) {
                Move-Item -LiteralPath $stagedGraph -Destination $graph -Force
            }
        }

        $refreshSeconds = Measure-Synaptic `
            -Operation "$name memory refresh" `
            -Arguments @("memory", "refresh", "--root", $repo, "--graph", $graph) `
            -Log (Join-Path $repoLogRoot "refresh.log")

        $stride = [Math]::Max(1, [Math]::Floor($commits.Count / $CasesPerRepo))
        $cases = [System.Collections.Generic.List[object]]::new()
        for ($index = 0; $index -lt $commits.Count -and $cases.Count -lt $CasesPerRepo; $index += $stride) {
            $commit = [string]$commits[$index]
            $subject = (& git -C $repo log -1 "--format=%s" $commit | Out-String).Trim()
            # `--first-parent` histories can be merge-only (CodeQL is one).
            # Match Synaptic's merge-aware Git ingestion so those revisions
            # still contribute changed-path localization cases.
            $paths = @(& git -C $repo diff-tree --root --no-commit-id --name-only -r -m $commit)
            $path = $paths | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -First 1
            if ([string]::IsNullOrWhiteSpace($subject) -or [string]::IsNullOrWhiteSpace($path)) {
                continue
            }
            $cases.Add([ordered]@{
                name = "$name $($commit.Substring(0, 12))"
                query = Query-Text $subject
                symbol = $path.Replace("\", "/")
                expected_sources = @("git:$commit")
            })
        }
        if ($cases.Count -eq 0) {
            throw "could not construct memory-localization cases"
        }

        $manifest = [ordered]@{
            schema = "synaptic.memory-benchmark/v1"
            cases = $cases
        }
        $manifestPath = Join-Path $output "manifests\$name.json"
        Write-Utf8NoBom -Path $manifestPath -Content ($manifest | ConvertTo-Json -Depth 8)

        $compactSeconds = Measure-Synaptic `
            -Operation "$name memory compact" `
            -Arguments @("memory", "compact", "--root", $repo, "--json") `
            -Log (Join-Path $repoLogRoot "compact.log")

        $evalTimes = [System.Collections.Generic.List[double]]::new()
        $evalJson = $null
        for ($run = 1; $run -le $EvalRepetitions; $run++) {
            $watch = [System.Diagnostics.Stopwatch]::StartNew()
            $raw = (& $exe memory eval --root $repo --manifest $manifestPath --json | Out-String)
            $code = $LASTEXITCODE
            $watch.Stop()
            if ($code -ne 0) {
                throw "$name memory eval run $run failed with exit code $code"
            }
            $evalTimes.Add($watch.Elapsed.TotalMilliseconds)
            $evalJson = $raw | ConvertFrom-Json
        }

        $statusRaw = (& $exe memory status --root $repo --json | Out-String)
        if ($LASTEXITCODE -ne 0) {
            throw "$name memory status failed"
        }
        $status = $statusRaw | ConvertFrom-Json
        $reportPath = Join-Path $output "reports\$name.json"
        Write-Utf8NoBom -Path $reportPath -Content ($evalJson | ConvertTo-Json -Depth 10)

        $results.Add([ordered]@{
            name = $name
            sha = $actualSha
            commits_ingested = $commits.Count
            cases = $cases.Count
            records = $status.records
            records_by_kind = $status.by_kind
            extract_seconds = $extractSeconds
            ingest_seconds = $ingestWatch.Elapsed.TotalSeconds
            refresh_seconds = $refreshSeconds
            compact_seconds = $compactSeconds
            eval_median_ms = Median ([double[]]$evalTimes)
            eval_times_ms = @($evalTimes)
            recall_at_1 = $evalJson.recall_at_1
            recall_at_5 = $evalJson.recall_at_5
            mean_reciprocal_rank = $evalJson.mean_reciprocal_rank
            mean_candidate_fraction = $evalJson.mean_candidate_fraction
            misses = @($evalJson.misses)
            manifest = $manifestPath
            report = $reportPath
        })
    } catch {
        $failures.Add([ordered]@{ name = $name; error = $_.Exception.Message })
    }
}

$summary = [ordered]@{
    schema = "synaptic.tool-source-memory-benchmark/v1"
    generated_at = [DateTimeOffset]::Now.ToString("o")
    environment = [ordered]@{
        os = [Environment]::OSVersion.VersionString
        logical_cpus = [Environment]::ProcessorCount
        rust_min_stack = $env:RUST_MIN_STACK
        synaptic_max_nodes = $env:SYNAPTIC_MAX_NODES
        synaptic_max_graph_mb = $env:SYNAPTIC_MAX_GRAPH_MB
        synaptic_version = (& $exe --version | Out-String).Trim()
        synaptic_head = (& git -C $workspace rev-parse HEAD | Out-String).Trim()
    }
    settings = [ordered]@{
        commits_per_repo = $CommitsPerRepo
        cases_per_repo = $CasesPerRepo
        eval_repetitions = $EvalRepetitions
    }
    results = $results
    failures = $failures
}

$summaryPath = Join-Path $output "summary.json"
Write-Utf8NoBom -Path $summaryPath -Content ($summary | ConvertTo-Json -Depth 12)
$summary | ConvertTo-Json -Depth 12

if ($failures.Count -gt 0) {
    exit 1
}
