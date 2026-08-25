#!/usr/bin/env python3
"""Reproducible token-efficiency, BEIR retrieval, and historical-task tooling."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import random
import re
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

RUN_SCHEMA = "synaptic.agent-token-run/v1"
REPORT_SCHEMA = "synaptic.agent-token-report/v1"


def die(message: str) -> None:
    raise SystemExit(message)


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def number(value, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        die(f"{name} must be a non-negative number")
    return float(value)


def percentile(values: list[float], p: float) -> float:
    values = sorted(values)
    if not values:
        return 0.0
    position = (len(values) - 1) * p
    lo = math.floor(position)
    hi = math.ceil(position)
    if lo == hi:
        return values[lo]
    return values[lo] * (hi - position) + values[hi] * (position - lo)


def task_tokens(task: dict) -> float:
    return sum(
        number(task.get(key, 0), f"{task.get('task_id', '?')}.{key}")
        for key in ("input_tokens", "output_tokens", "index_tokens")
    )


def load_run(path: Path, condition: str) -> tuple[dict, dict[str, dict]]:
    run = read_json(path)
    if run.get("schema") != RUN_SCHEMA:
        die(f"{path}: schema must be {RUN_SCHEMA!r}")
    if run.get("condition") != condition:
        die(f"{path}: condition must be {condition!r}")
    tasks: dict[str, dict] = {}
    for task in run.get("tasks", []):
        task_id = task.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            die(f"{path}: every task needs a non-empty task_id")
        if task_id in tasks:
            die(f"{path}: duplicate task_id {task_id!r}")
        if not isinstance(task.get("resolved"), bool):
            die(f"{path}: {task_id}.resolved must be boolean")
        task_tokens(task)
        tasks[task_id] = task
    if not tasks:
        die(f"{path}: no tasks")
    return run, tasks


def summarize(tasks: list[dict]) -> dict:
    total = sum(task_tokens(task) for task in tasks)
    resolved = sum(task["resolved"] for task in tasks)
    return {
        "tasks": len(tasks),
        "resolved": resolved,
        "pass_at_1": resolved / len(tasks),
        "input_tokens": sum(number(t.get("input_tokens", 0), "input_tokens") for t in tasks),
        "output_tokens": sum(number(t.get("output_tokens", 0), "output_tokens") for t in tasks),
        "index_tokens": sum(number(t.get("index_tokens", 0), "index_tokens") for t in tasks),
        "total_tokens": total,
        "tokens_per_task": total / len(tasks),
        "tokens_per_resolved_task": total / resolved if resolved else None,
        "cost_usd": sum(number(t.get("cost_usd", 0), "cost_usd") for t in tasks),
        "wall_seconds": sum(number(t.get("wall_seconds", 0), "wall_seconds") for t in tasks),
    }


def exact_mcnemar(baseline_only: int, synaptic_only: int) -> float:
    discordant = baseline_only + synaptic_only
    if not discordant:
        return 1.0
    tail = sum(math.comb(discordant, i) for i in range(min(baseline_only, synaptic_only) + 1))
    return min(1.0, 2.0 * tail / (2**discordant))


def agent_report(
    baseline_path: Path,
    synaptic_path: Path,
    bootstrap_repetitions: int,
    margin: float,
) -> dict:
    baseline_run, baseline = load_run(baseline_path, "baseline")
    synaptic_run, synaptic = load_run(synaptic_path, "synaptic")
    if baseline.keys() != synaptic.keys():
        missing_b = sorted(synaptic.keys() - baseline.keys())
        missing_s = sorted(baseline.keys() - synaptic.keys())
        die(f"paired task sets differ; missing baseline={missing_b}, missing synaptic={missing_s}")
    for key in ("dataset", "dataset_revision", "model", "agent", "agent_revision"):
        left = baseline_run.get("metadata", {}).get(key)
        right = synaptic_run.get("metadata", {}).get(key)
        if left is not None and right is not None and left != right:
            die(f"uncontrolled comparison: metadata.{key} differs ({left!r} vs {right!r})")

    task_ids = sorted(baseline)
    pairs = [(baseline[task_id], synaptic[task_id]) for task_id in task_ids]
    base_summary = summarize([pair[0] for pair in pairs])
    syn_summary = summarize([pair[1] for pair in pairs])
    if base_summary["total_tokens"] <= 0:
        die("baseline token total must be positive")

    savings = 1.0 - syn_summary["total_tokens"] / base_summary["total_tokens"]
    paired_savings = [
        1.0 - task_tokens(syn) / task_tokens(base)
        for base, syn in pairs
        if task_tokens(base) > 0
    ]
    pass_delta = syn_summary["pass_at_1"] - base_summary["pass_at_1"]
    rng = random.Random(0x5A71C)
    sampled_savings: list[float] = []
    sampled_pass_delta: list[float] = []
    for _ in range(bootstrap_repetitions):
        sample = [pairs[rng.randrange(len(pairs))] for _ in pairs]
        base_tokens = sum(task_tokens(pair[0]) for pair in sample)
        syn_tokens = sum(task_tokens(pair[1]) for pair in sample)
        if base_tokens > 0:
            sampled_savings.append(1.0 - syn_tokens / base_tokens)
        sampled_pass_delta.append(
            sum(int(syn["resolved"]) - int(base["resolved"]) for base, syn in sample)
            / len(sample)
        )

    baseline_only = sum(base["resolved"] and not syn["resolved"] for base, syn in pairs)
    synaptic_only = sum(syn["resolved"] and not base["resolved"] for base, syn in pairs)
    savings_ci = [percentile(sampled_savings, 0.025), percentile(sampled_savings, 0.975)]
    pass_ci = [percentile(sampled_pass_delta, 0.025), percentile(sampled_pass_delta, 0.975)]
    return {
        "schema": REPORT_SCHEMA,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "metadata": {
            "baseline": baseline_run.get("metadata", {}),
            "synaptic": synaptic_run.get("metadata", {}),
        },
        "baseline": base_summary,
        "synaptic": syn_summary,
        "comparison": {
            "aggregate_token_savings": savings,
            "median_paired_token_savings": statistics.median(paired_savings),
            "token_savings_bootstrap_95_ci": savings_ci,
            "pass_at_1_delta": pass_delta,
            "pass_at_1_delta_bootstrap_95_ci": pass_ci,
            "noninferiority_margin": margin,
            "quality_noninferior": pass_ci[0] >= -margin,
            "mcnemar": {
                "baseline_only": baseline_only,
                "synaptic_only": synaptic_only,
                "exact_two_sided_p": exact_mcnemar(baseline_only, synaptic_only),
            },
        },
        "tasks": [
            {
                "task_id": task_id,
                "baseline_resolved": baseline[task_id]["resolved"],
                "synaptic_resolved": synaptic[task_id]["resolved"],
                "baseline_tokens": task_tokens(baseline[task_id]),
                "synaptic_tokens": task_tokens(synaptic[task_id]),
                "token_savings": 1.0 - task_tokens(synaptic[task_id]) / task_tokens(baseline[task_id])
                if task_tokens(baseline[task_id])
                else None,
            }
            for task_id in task_ids
        ],
    }


def pct(value: float | None) -> str:
    return "n/a" if value is None else f"{value * 100:.2f}%"


def count(value: float | None) -> str:
    return "n/a" if value is None else f"{value:,.0f}"


def agent_markdown(report: dict) -> str:
    base, syn, comp = report["baseline"], report["synaptic"], report["comparison"]
    ci = comp["token_savings_bootstrap_95_ci"]
    pass_ci = comp["pass_at_1_delta_bootstrap_95_ci"]
    return f"""# Synaptic agent token benchmark

Paired tasks: **{base['tasks']}**. Token totals include provider input, output, and separately declared indexing tokens. Cache-token fields are diagnostic subsets and are not double-counted.

| Condition | Resolved | Pass@1 | Total tokens | Tokens / task | Tokens / resolved |
|---|---:|---:|---:|---:|---:|
| Baseline | {base['resolved']}/{base['tasks']} | {pct(base['pass_at_1'])} | {base['total_tokens']:,.0f} | {base['tokens_per_task']:,.0f} | {count(base['tokens_per_resolved_task'])} |
| Synaptic | {syn['resolved']}/{syn['tasks']} | {pct(syn['pass_at_1'])} | {syn['total_tokens']:,.0f} | {syn['tokens_per_task']:,.0f} | {count(syn['tokens_per_resolved_task'])} |

- aggregate token savings: **{pct(comp['aggregate_token_savings'])}** (paired bootstrap 95% CI {pct(ci[0])} to {pct(ci[1])})
- median per-task savings: **{pct(comp['median_paired_token_savings'])}**
- Pass@1 delta: **{pct(comp['pass_at_1_delta'])}** (95% CI {pct(pass_ci[0])} to {pct(pass_ci[1])})
- quality non-inferior at the predeclared {pct(comp['noninferiority_margin'])} margin: **{str(comp['quality_noninferior']).lower()}**
- paired exact McNemar p: **{comp['mcnemar']['exact_two_sided_p']:.4f}**

This result applies only to the pinned dataset, model, agent, prompts, tool versions, and token-accounting policy recorded with the run.
"""


def usage_parts(usage: dict) -> tuple[float, float, float, float]:
    def first(*keys):
        for key in keys:
            value = usage.get(key)
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                return float(value)
        return 0.0

    input_tokens = first("total_input_tokens", "prompt_tokens", "input_tokens", "tokens_sent")
    output_tokens = first("output_tokens", "completion_tokens", "tokens_received")
    details = usage.get("prompt_tokens_details") or usage.get("input_tokens_details") or {}
    if not isinstance(details, dict):
        details = {}
    cache_read_extra = first("cache_read_input_tokens", "cache_read_tokens")
    cache_write = first("cache_creation_input_tokens", "cache_write_tokens")
    cache_read = cache_read_extra or first("cached_tokens") or float(
        details.get("cached_tokens", 0) or 0
    )
    # Anthropic reports cache reads/writes beside uncached input; OpenAI's
    # `cached_tokens` is already a subset of input/prompt tokens.
    if "prompt_tokens" not in usage and "total_input_tokens" not in usage:
        input_tokens += cache_read_extra + cache_write
    return input_tokens, output_tokens, cache_read, cache_write


def trajectory_usage(data: dict) -> tuple[float, float, float, float]:
    stats = data.get("info", {}).get("model_stats", {})
    direct = usage_parts(stats) if isinstance(stats, dict) else (0.0, 0.0, 0.0, 0.0)
    if direct[0] or direct[1]:
        return direct
    totals = [0.0, 0.0, 0.0, 0.0]
    for message in data.get("messages", []):
        extra = message.get("extra", {}) if isinstance(message, dict) else {}
        response = extra.get("response", {}) if isinstance(extra, dict) else {}
        candidates = [
            message.get("usage"),
            extra.get("usage"),
            extra.get("token_usage"),
            response.get("usage") if isinstance(response, dict) else None,
        ]
        usage = next((item for item in candidates if isinstance(item, dict)), None)
        if usage:
            for i, value in enumerate(usage_parts(usage)):
                totals[i] += value
    return tuple(totals)  # type: ignore[return-value]


def trajectory_wall_seconds(data: dict) -> float:
    timestamps = [
        message.get("extra", {}).get("timestamp")
        for message in data.get("messages", [])
        if isinstance(message, dict) and isinstance(message.get("extra"), dict)
    ]
    timestamps = [float(value) for value in timestamps if isinstance(value, (int, float))]
    return max(timestamps) - min(timestamps) if len(timestamps) > 1 else 0.0


def resolutions(path: Path) -> dict[str, bool]:
    files = list(path.rglob("*.json")) if path.is_dir() else [path]
    out: dict[str, bool] = {}
    for file in files:
        try:
            data = read_json(file)
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(data, dict) and "resolved_ids" in data:
            for task_id in data.get("resolved_ids", []):
                out[str(task_id)] = True
            for key in ("unresolved_ids", "error_ids", "empty_patch_ids", "incomplete_ids"):
                for task_id in data.get(key, []):
                    out.setdefault(str(task_id), False)
        elif isinstance(data, dict):
            for task_id, result in data.items():
                if isinstance(result, bool):
                    out[str(task_id)] = result
                elif isinstance(result, dict) and isinstance(result.get("resolved"), bool):
                    out[str(task_id)] = result["resolved"]
    if not out:
        die(f"no SWE-bench resolved verdicts found under {path}")
    return out


def normalize_mini_swe(args) -> None:
    verdicts = resolutions(args.evaluation)
    tasks = []
    for path in sorted(args.trajectories.rglob("*.traj.json")):
        data = read_json(path)
        task_id = str(data.get("instance_id") or path.name.removesuffix(".traj.json"))
        if task_id not in verdicts:
            continue
        input_tokens, output_tokens, cache_read, cache_write = trajectory_usage(data)
        if not input_tokens and not output_tokens:
            die(f"{path}: trajectory contains no provider token usage")
        stats = data.get("info", {}).get("model_stats", {})
        tasks.append(
            {
                "task_id": task_id,
                "resolved": verdicts[task_id],
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_read_tokens": cache_read,
                "cache_write_tokens": cache_write,
                "index_tokens": args.index_tokens_per_task,
                "cost_usd": stats.get("instance_cost", 0) if isinstance(stats, dict) else 0,
                "wall_seconds": trajectory_wall_seconds(data),
            }
        )
    if not tasks:
        die("no trajectories matched the evaluation verdicts")
    write_json(
        args.out,
        {
            "schema": RUN_SCHEMA,
            "condition": args.condition,
            "metadata": {
                "dataset": args.dataset,
                "dataset_revision": args.dataset_revision,
                "model": args.model,
                "agent": args.agent,
                "agent_revision": args.agent_revision,
                "condition_config_sha256": config_digest(args.condition_config),
            },
            "tasks": tasks,
        },
    )


def config_digest(paths: list[Path]) -> str:
    digest = hashlib.sha256()
    for path in paths:
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def mini_swe_overlay(args) -> None:
    binary = args.binary.resolve()
    if not binary.is_file():
        die(f"Synaptic benchmark binary does not exist: {binary}")
    binary_bytes = binary.read_bytes()
    if binary_bytes[:4] != b"\x7fELF":
        die("mini-SWE Docker treatment requires a Linux ELF Synaptic binary")
    binary_sha256 = hashlib.sha256(binary_bytes).hexdigest()
    mount = f"type=bind,source={binary},target=/usr/local/bin/synaptic,readonly"
    overlay = f'''# synaptic_binary_sha256: {binary_sha256}
agent:
  system_template: |
    You are a helpful assistant that can interact with a computer shell to solve programming tasks.
    This is the Synaptic treatment condition. Before broad code exploration, run
    `synaptic extract /testbed --directed --no-store`, then use
    `synaptic query "<intent>" --graph /testbed/synaptic-out/graph.json --json`
    to localize relevant symbols and files. Inspect source and run tests before editing;
    Synaptic is retrieval evidence, not authority. Do not add synaptic-out to the patch.
environment:
  run_args:
    - "--rm"
    - "--mount"
    - {json.dumps(mount)}
'''
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(overlay, encoding="utf-8")


def count_with(executable: Path, *, text: str | None = None, files: list[Path] | None = None) -> int:
    completed = subprocess.run(
        [str(executable), *(str(path) for path in files or [])],
        input=text,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    if completed.returncode:
        die(completed.stderr.strip() or f"{executable} failed")
    try:
        return int(completed.stdout.strip())
    except ValueError:
        die(f"{executable} returned a non-integer token count")


def response_source_files(result: dict) -> list[str]:
    return sorted(
        {
            normalize_path(str(node["source_file"]))
            for group in ("nodes", "seeds")
            for node in result.get(group, [])
            if node.get("source_file")
        }
    )


def measure_context(
    synaptic: Path,
    tokcount: Path,
    graph_path: Path,
    repo_root: Path,
    queries: list[str],
    max_nodes: int,
) -> dict:
    rows = []
    for query in queries:
        completed = subprocess.run(
            [
                str(synaptic),
                "query",
                query,
                "--graph",
                str(graph_path),
                "--max-nodes",
                str(max_nodes),
                "--json",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if completed.returncode:
            die(f"query {query!r} failed: {completed.stderr.strip()}")
        result = json.loads(completed.stdout)
        sources = response_source_files(result)
        files = [repo_root / source for source in sources]
        missing = [source for source, path in zip(sources, files) if not path.is_file()]
        if missing:
            die(f"query {query!r} returned missing source files: {missing}")
        response_tokens = count_with(tokcount, text=completed.stdout)
        file_tokens = count_with(tokcount, files=files)
        if not file_tokens:
            die(f"query {query!r} returned no readable source context")
        rows.append(
            {
                "query": query,
                "nodes": len(result.get("nodes", [])),
                "files": len(files),
                "response_tokens": response_tokens,
                "file_tokens": file_tokens,
                "savings": 1 - response_tokens / file_tokens,
                "reduction_x": file_tokens / response_tokens,
                "source_files": sources,
            }
        )
    response_total = sum(row["response_tokens"] for row in rows)
    file_total = sum(row["file_tokens"] for row in rows)
    graph = read_json(graph_path)
    return {
        "graph_nodes": len(graph.get("nodes", [])),
        "graph_edges": len(graph.get("links", graph.get("edges", []))),
        "queries": rows,
        "totals": {
            "response_tokens": response_total,
            "file_tokens": file_total,
            "savings": 1 - response_total / file_total,
            "reduction_x": file_total / response_total,
        },
    }


def context_markdown(report: dict, title: str = "Synaptic context-token benchmark") -> str:
    rows = report["queries"]
    totals = report["totals"]
    table = "\n".join(
        f"| {row['query']} | {row['response_tokens']:,} | {row['file_tokens']:,} | {pct(row['savings'])} | {row['reduction_x']:.2f}x |"
        for row in rows
    )
    return f"""# {title}

Exact `cl100k_base` counts on {len(rows)} fixed queries; each baseline is the unique complete source files referenced by the ranked response.

| Query | Response | Referenced files | Savings | Reduction |
|---|---:|---:|---:|---:|
{table}
| **Total** | **{totals['response_tokens']:,}** | **{totals['file_tokens']:,}** | **{pct(totals['savings'])}** | **{totals['reduction_x']:.2f}x** |
"""


def context_benchmark(args) -> None:
    report = {
        "schema": "synaptic.context-token-report/v1",
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "git_commit": git(args.repo_root, "rev-parse", "HEAD").strip(),
        "dirty": bool(git(args.repo_root, "status", "--porcelain").strip()),
        "tokenizer": "cl100k_base",
        "max_nodes": args.max_nodes,
        **measure_context(
            args.synaptic,
            args.tokcount,
            args.graph,
            args.repo_root,
            args.query,
            args.max_nodes,
        ),
    }
    markdown = context_markdown(report)
    write_json(args.out / "report.json", report)
    (args.out / "report.md").write_text(markdown, encoding="utf-8")
    print(markdown, end="")


def context_corpus(args) -> None:
    source = read_json(args.cases)
    cases = source.get("cases", []) if isinstance(source, dict) else []
    if not cases:
        die("context corpus needs at least one case")
    repos = []
    for case in cases:
        name, family, commit, queries = (
            case.get("name"),
            case.get("family"),
            case.get("commit"),
            case.get("queries"),
        )
        if not all(isinstance(value, str) and value for value in (name, family, commit)):
            die("every context corpus case needs name, family, and commit")
        if not isinstance(queries, list) or not queries or not all(isinstance(q, str) and q for q in queries):
            die(f"{name}: queries must be a non-empty string list")
        repo_root = (args.cache / name).resolve()
        if not repo_root.is_dir():
            die(f"{name}: cached repository does not exist at {repo_root}")
        actual_commit = git(repo_root, "rev-parse", "HEAD").strip()
        if actual_commit != commit:
            die(f"{name}: expected {commit}, found {actual_commit}")
        if args.extract:
            completed = subprocess.run(
                [str(args.synaptic), "extract", str(repo_root), "--directed", "--no-store"],
                capture_output=True,
                text=True,
                encoding="utf-8",
            )
            if completed.returncode:
                die(f"{name}: extraction failed: {completed.stderr.strip()}")
        graph_path = repo_root / "synaptic-out" / "graph.json"
        if not graph_path.is_file():
            die(f"{name}: graph missing; pass --extract")
        repos.append(
            {
                "name": name,
                "family": family,
                "git_commit": commit,
                **measure_context(
                    args.synaptic,
                    args.tokcount,
                    graph_path,
                    repo_root,
                    queries,
                    args.max_nodes,
                ),
            }
        )
    response_total = sum(repo["totals"]["response_tokens"] for repo in repos)
    file_total = sum(repo["totals"]["file_tokens"] for repo in repos)
    report = {
        "schema": "synaptic.context-token-corpus-report/v1",
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "tokenizer": "cl100k_base",
        "max_nodes": args.max_nodes,
        "repositories": repos,
        "totals": {
            "repositories": len(repos),
            "queries": sum(len(repo["queries"]) for repo in repos),
            "response_tokens": response_total,
            "file_tokens": file_total,
            "savings": 1 - response_total / file_total,
            "reduction_x": file_total / response_total,
        },
    }
    rows = "\n".join(
        f"| {repo['name']} | {repo['family']} | {repo['graph_nodes']:,} | {repo['totals']['response_tokens']:,} | {repo['totals']['file_tokens']:,} | {pct(repo['totals']['savings'])} | {repo['totals']['reduction_x']:.2f}x |"
        for repo in repos
    )
    totals = report["totals"]
    markdown = f"""# Synaptic multi-repository context-token benchmark

Exact `cl100k_base` counts on {totals['queries']} fixed queries across {totals['repositories']} pinned repositories.

| Repository | Family | Nodes | Response | Referenced files | Savings | Reduction |
|---|---|---:|---:|---:|---:|---:|
{rows}
| **Total** | | | **{response_total:,}** | **{file_total:,}** | **{pct(totals['savings'])}** | **{totals['reduction_x']:.2f}x** |
"""
    write_json(args.out / "report.json", report)
    (args.out / "report.md").write_text(markdown, encoding="utf-8")
    print(markdown, end="")


def json_lines(path: Path):
    with path.open(encoding="utf-8") as file:
        for line_number, line in enumerate(file, 1):
            if line.strip():
                try:
                    yield json.loads(line)
                except json.JSONDecodeError as error:
                    die(f"{path}:{line_number}: {error}")


def normalize_path(value: str) -> str:
    return value.replace("\\", "/").removeprefix("./")


def corpus_paths(path: Path | None) -> dict[str, list[str]]:
    if path is None:
        return {}
    result: dict[str, list[str]] = {}
    for document in json_lines(path):
        metadata = document.get("metadata", {}) if isinstance(document.get("metadata"), dict) else {}
        candidate = next(
            (
                value
                for value in (
                    document.get("file_path"),
                    document.get("path"),
                    metadata.get("file_path"),
                    metadata.get("path"),
                    document.get("title"),
                )
                if isinstance(value, str) and value
            ),
            None,
        )
        doc_id = document.get("_id", document.get("id"))
        if candidate and doc_id is not None:
            result.setdefault(normalize_path(candidate), []).append(str(doc_id))
    return result


def beir_run(args) -> None:
    path_map = corpus_paths(args.corpus)
    rows = []
    for query in json_lines(args.queries):
        query_id = str(query.get("_id", query.get("id", "")))
        text = query.get("text") or query.get("query") or query.get("title")
        if not query_id or not isinstance(text, str):
            die("each BEIR query needs _id/id and text/query/title")
        command = [
            str(args.synaptic),
            "query",
            text,
            "--graph",
            str(args.graph),
            "--max-nodes",
            str(args.max_nodes),
            "--json",
        ]
        if args.repo:
            command.extend(["--repo", args.repo])
        completed = subprocess.run(command, capture_output=True, text=True, encoding="utf-8")
        if completed.returncode:
            die(f"query {query_id} failed: {completed.stderr.strip()}")
        result = json.loads(completed.stdout)
        seen: set[str] = set()
        rank = 0
        for node in result.get("nodes", []):
            source = normalize_path(str(node.get("source_file", "")))
            if not source or source in seen:
                continue
            seen.add(source)
            doc_ids = path_map.get(source, [source])
            for doc_id in doc_ids:
                rank += 1
                rows.append(f"{query_id} Q0 {doc_id} {rank} {float(node.get('score', 0)):.8f} synaptic")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(rows) + "\n", encoding="utf-8")


def qrels(path: Path) -> dict[str, dict[str, float]]:
    result: dict[str, dict[str, float]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if not fields or fields[0].lower() in {"query-id", "query_id"}:
            continue
        if len(fields) == 3:
            query_id, doc_id, relevance = fields
        elif len(fields) >= 4:
            query_id, _, doc_id, relevance = fields[:4]
        else:
            die(f"bad qrels row: {line}")
        result.setdefault(query_id, {})[doc_id] = float(relevance)
    return result


def trec_run(path: Path) -> dict[str, list[str]]:
    result: dict[str, list[tuple[int, str]]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if len(fields) < 4:
            die(f"bad TREC run row: {line}")
        result.setdefault(fields[0], []).append((int(fields[3]), fields[2]))
    return {query_id: [doc for _, doc in sorted(rows)] for query_id, rows in result.items()}


def beir_report(qrels_path: Path, run_path: Path, cutoffs: list[int]) -> dict:
    truth, run = qrels(qrels_path), trec_run(run_path)
    metrics = {f"recall@{k}": [] for k in cutoffs}
    metrics |= {f"precision@{k}": [] for k in cutoffs}
    metrics |= {f"mrr@{k}": [] for k in cutoffs}
    metrics |= {f"ndcg@{k}": [] for k in cutoffs}
    for query_id, grades in truth.items():
        relevant = {doc for doc, grade in grades.items() if grade > 0}
        if not relevant:
            continue
        ranking = run.get(query_id, [])
        ideal = sorted((grade for grade in grades.values() if grade > 0), reverse=True)
        for k in cutoffs:
            top = ranking[:k]
            hits = sum(doc in relevant for doc in top)
            metrics[f"recall@{k}"].append(hits / len(relevant))
            metrics[f"precision@{k}"].append(hits / k)
            first = next((rank for rank, doc in enumerate(top, 1) if doc in relevant), None)
            metrics[f"mrr@{k}"].append(1 / first if first else 0.0)
            dcg = sum((2 ** grades.get(doc, 0) - 1) / math.log2(rank + 1) for rank, doc in enumerate(top, 1))
            idcg = sum((2**grade - 1) / math.log2(rank + 1) for rank, grade in enumerate(ideal[:k], 1))
            metrics[f"ndcg@{k}"].append(dcg / idcg if idcg else 0.0)
    if not any(metrics.values()):
        die("qrels contain no relevant queries")
    return {
        "schema": "synaptic.beir-report/v1",
        "queries": len(next(iter(metrics.values()))),
        "metrics": {key: sum(values) / len(values) for key, values in metrics.items()},
    }


def beir_markdown(report: dict) -> str:
    rows = "\n".join(f"| {key} | {value:.4f} |" for key, value in report["metrics"].items())
    return f"# Synaptic BEIR retrieval benchmark\n\nQueries: **{report['queries']}**\n\n| Metric | Score |\n|---|---:|\n{rows}\n"


def git(repo: Path, *args: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, encoding="utf-8"
    )
    if completed.returncode:
        die(completed.stderr.strip() or f"git {' '.join(args)} failed")
    return completed.stdout


TEST_PATH = re.compile(r"(^|/)(tests?|__tests__)(/|$)|(^|/)test_|(_test|\.test|\.spec)\.", re.I)


def history_candidates(args) -> None:
    commits = git(args.repo, "rev-list", "--first-parent", "--no-merges", f"--max-count={args.limit}", args.revision).split()
    tasks = []
    for commit in commits:
        parents = git(args.repo, "show", "-s", "--format=%P", commit).split()
        if not parents:
            continue
        changed = [normalize_path(path) for path in git(args.repo, "diff-tree", "--no-commit-id", "--name-only", "-r", commit).splitlines() if path]
        tasks.append(
            {
                "fix_commit": commit,
                "base_commit": parents[0],
                "problem_statement": git(args.repo, "show", "-s", "--format=%B", commit).strip(),
                "changed_files": changed,
                "test_files": [path for path in changed if TEST_PATH.search(path)],
                "fail_to_pass": [],
                "pass_to_pass": [],
                "test_command": "",
            }
        )
    write_json(args.out, {"schema": "synaptic.historical-candidates/v1", "tasks": tasks})


def repository_slug(repo: Path) -> str:
    remote = git(repo, "remote", "get-url", "origin").strip()
    match = re.search(r"(?:github\.com[:/])([^/]+/[^/]+?)(?:\.git)?$", remote)
    return match.group(1) if match else f"local/{repo.resolve().name}"


def swebench_dataset(args) -> None:
    source = read_json(args.cases)
    tasks = source if isinstance(source, list) else source.get("tasks", [])
    slug = args.repository or repository_slug(args.repo)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", encoding="utf-8", newline="\n") as output:
        for index, task in enumerate(tasks, 1):
            fix = task.get("fix_commit")
            problem = task.get("problem_statement")
            if not fix or not problem or not task.get("fail_to_pass"):
                die(f"case {index}: fix_commit, problem_statement, and fail_to_pass are required")
            base = task.get("base_commit") or f"{fix}^"
            base = git(args.repo, "rev-parse", base).strip()
            changed = [normalize_path(path) for path in git(args.repo, "diff", "--name-only", base, fix).splitlines() if path]
            test_files = set(task.get("test_files") or [path for path in changed if TEST_PATH.search(path)])
            source_files = [path for path in changed if path not in test_files]
            patch = git(args.repo, "diff", "--binary", base, fix, "--", *source_files) if source_files else ""
            test_patch = git(args.repo, "diff", "--binary", base, fix, "--", *sorted(test_files)) if test_files else ""
            if not patch:
                die(f"case {index}: no non-test patch after splitting changed files")
            instance_id = task.get("instance_id") or f"{slug.replace('/', '__')}__{str(fix)[:12]}"
            record = {
                "repo": slug,
                "instance_id": instance_id,
                "base_commit": base,
                "patch": patch,
                "test_patch": test_patch,
                "problem_statement": problem,
                "hints_text": task.get("hints_text", ""),
                "created_at": git(args.repo, "show", "-s", "--format=%cI", fix).strip(),
                "version": task.get("version", ""),
                "FAIL_TO_PASS": json.dumps(task["fail_to_pass"]),
                "PASS_TO_PASS": json.dumps(task.get("pass_to_pass", [])),
                "environment_setup_commit": task.get("environment_setup_commit", base),
                "gold_patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
            }
            output.write(json.dumps(record) + "\n")


def self_test() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        assert response_source_files(
            {
                "nodes": [{"source_file": "src/a.py"}],
                "seeds": [{"source_file": "src/b.py"}, {"source_file": "src/a.py"}],
            }
        ) == ["src/a.py", "src/b.py"]
        metadata = {"dataset": "smoke", "model": "same", "agent": "same"}
        baseline = {
            "schema": RUN_SCHEMA,
            "condition": "baseline",
            "metadata": metadata,
            "tasks": [
                {"task_id": "a", "resolved": True, "input_tokens": 80, "output_tokens": 20},
                {"task_id": "b", "resolved": False, "input_tokens": 80, "output_tokens": 20},
            ],
        }
        synaptic = {
            "schema": RUN_SCHEMA,
            "condition": "synaptic",
            "metadata": metadata,
            "tasks": [
                {"task_id": "a", "resolved": True, "input_tokens": 40, "output_tokens": 10},
                {"task_id": "b", "resolved": True, "input_tokens": 40, "output_tokens": 10},
            ],
        }
        write_json(root / "baseline.json", baseline)
        write_json(root / "synaptic.json", synaptic)
        report = agent_report(root / "baseline.json", root / "synaptic.json", 1000, 0.05)
        assert report["comparison"]["aggregate_token_savings"] == 0.5
        assert report["synaptic"]["resolved"] == 2
        (root / "qrels.tsv").write_text("query-id\tcorpus-id\tscore\nq\ta\t1\n", encoding="utf-8")
        (root / "run.txt").write_text("q Q0 a 1 1.0 synaptic\n", encoding="utf-8")
        retrieval = beir_report(root / "qrels.tsv", root / "run.txt", [1, 5])
        assert retrieval["metrics"]["recall@1"] == 1.0
        assert exact_mcnemar(0, 1) == 1.0
        usage = trajectory_usage(
            {
                "messages": [
                    {
                        "extra": {
                            "response": {
                                "usage": {
                                    "prompt_tokens": 90,
                                    "completion_tokens": 10,
                                    "prompt_tokens_details": {"cached_tokens": 40},
                                }
                            }
                        }
                    }
                ]
            }
        )
        assert usage == (90, 10, 40, 0)
        assert usage_parts(
            {"input_tokens": 100, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 40}}
        ) == (100, 5, 40, 0)
        assert usage_parts(
            {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 90}
        ) == (100, 5, 90, 0)
        elf = root / "synaptic"
        elf.write_bytes(b"\x7fELFsmoke")
        mini_swe_overlay(argparse.Namespace(binary=elf, out=root / "overlay.yaml"))
        overlay = (root / "overlay.yaml").read_text(encoding="utf-8")
        assert "/usr/local/bin/synaptic" in overlay
        assert hashlib.sha256(elf.read_bytes()).hexdigest() in overlay
        assert len(config_digest([root / "overlay.yaml"])) == 64

        repo = root / "repo"
        repo.mkdir()
        for command in (
            ("init", "-q"),
            ("config", "user.name", "Benchmark"),
            ("config", "user.email", "benchmark@example.invalid"),
        ):
            subprocess.run(["git", "-C", str(repo), *command], check=True, capture_output=True)
        (repo / "tests").mkdir()
        (repo / "source.py").write_text("VALUE = 1\n", encoding="utf-8")
        (repo / "tests" / "test_source.py").write_text("assert True\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-qm", "base"], check=True)
        (repo / "source.py").write_text("VALUE = 2\n", encoding="utf-8")
        (repo / "tests" / "test_source.py").write_text("assert VALUE == 2\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-qm", "fix value"], check=True)
        fix = git(repo, "rev-parse", "HEAD").strip()
        write_json(
            root / "cases.json",
            {
                "tasks": [
                    {
                        "fix_commit": fix,
                        "problem_statement": "Correct the exported value.",
                        "fail_to_pass": ["tests/test_source.py"],
                        "pass_to_pass": [],
                    }
                ]
            },
        )
        swebench_dataset(
            argparse.Namespace(
                repo=repo,
                cases=root / "cases.json",
                repository="owner/repo",
                out=root / "dataset.jsonl",
            )
        )
        dataset = next(json_lines(root / "dataset.jsonl"))
        assert "source.py" in dataset["patch"] and "test_source.py" not in dataset["patch"]
        assert "test_source.py" in dataset["test_patch"]
    print("benchmark-token-savings self-test passed")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    agent = commands.add_parser("agent-report", help="Compare paired normalized agent runs")
    agent.add_argument("--baseline", type=Path, required=True)
    agent.add_argument("--synaptic", type=Path, required=True)
    agent.add_argument("--out", type=Path, default=Path("synaptic-out/eval/agent-tokens"))
    agent.add_argument("--bootstrap", type=int, default=10_000)
    agent.add_argument("--noninferiority-margin", type=float, default=0.05)
    agent.add_argument("--require-noninferior", action="store_true")
    agent.add_argument("--min-token-savings", type=float)

    normalize = commands.add_parser("normalize-mini-swe", help="Normalize mini-SWE-agent trajectories")
    normalize.add_argument("--trajectories", type=Path, required=True)
    normalize.add_argument("--evaluation", type=Path, required=True)
    normalize.add_argument("--condition", choices=("baseline", "synaptic"), required=True)
    normalize.add_argument("--out", type=Path, required=True)
    normalize.add_argument("--dataset", required=True)
    normalize.add_argument("--dataset-revision", required=True)
    normalize.add_argument("--model", required=True)
    normalize.add_argument("--agent", default="mini-swe-agent")
    normalize.add_argument("--agent-revision", required=True)
    normalize.add_argument("--condition-config", type=Path, action="append", required=True)
    normalize.add_argument("--index-tokens-per-task", type=float, default=0)

    overlay = commands.add_parser("mini-swe-overlay", help="Mount Synaptic in mini-SWE Docker tasks")
    overlay.add_argument("--binary", type=Path, required=True)
    overlay.add_argument("--out", type=Path, required=True)

    context = commands.add_parser("context-benchmark", help="Measure query context against source files")
    context.add_argument("--synaptic", type=Path, required=True)
    context.add_argument("--tokcount", type=Path, required=True)
    context.add_argument("--graph", type=Path, required=True)
    context.add_argument("--repo-root", type=Path, default=Path("."))
    context.add_argument("--query", action="append", required=True)
    context.add_argument("--max-nodes", type=int, default=30)
    context.add_argument("--out", type=Path, required=True)

    corpus = commands.add_parser("context-corpus", help="Measure context across pinned repositories")
    corpus.add_argument("--synaptic", type=Path, required=True)
    corpus.add_argument("--tokcount", type=Path, required=True)
    corpus.add_argument("--cases", type=Path, required=True)
    corpus.add_argument("--cache", type=Path, default=Path("synaptic-out/bench"))
    corpus.add_argument("--max-nodes", type=int, default=30)
    corpus.add_argument("--extract", action="store_true")
    corpus.add_argument("--out", type=Path, required=True)

    run = commands.add_parser("beir-run", help="Create a TREC run from BEIR queries")
    run.add_argument("--synaptic", type=Path, required=True)
    run.add_argument("--graph", type=Path, required=True)
    run.add_argument("--queries", type=Path, required=True)
    run.add_argument("--corpus", type=Path)
    run.add_argument("--max-nodes", type=int, default=30)
    run.add_argument("--repo")
    run.add_argument("--out", type=Path, required=True)

    evaluate = commands.add_parser("beir-eval", help="Score a TREC run against BEIR qrels")
    evaluate.add_argument("--qrels", type=Path, required=True)
    evaluate.add_argument("--run", type=Path, required=True)
    evaluate.add_argument("--cutoffs", default="1,5,10")
    evaluate.add_argument("--out", type=Path, default=Path("synaptic-out/eval/beir"))

    candidates = commands.add_parser("history-candidates", help="Create historical cases to curate")
    candidates.add_argument("--repo", type=Path, default=Path("."))
    candidates.add_argument("--revision", default="HEAD")
    candidates.add_argument("--limit", type=int, default=100)
    candidates.add_argument("--out", type=Path, required=True)

    dataset = commands.add_parser("swebench-dataset", help="Materialize curated cases as SWE-bench JSONL")
    dataset.add_argument("--repo", type=Path, default=Path("."))
    dataset.add_argument("--cases", type=Path, required=True)
    dataset.add_argument("--repository")
    dataset.add_argument("--out", type=Path, required=True)

    commands.add_parser("self-test", help="Run the dependency-free smoke check")
    return root


def main() -> int:
    args = parser().parse_args()
    if args.command == "agent-report":
        if args.bootstrap < 100:
            die("--bootstrap must be at least 100")
        if not 0 <= args.noninferiority_margin < 1:
            die("--noninferiority-margin must be in [0, 1)")
        report = agent_report(args.baseline, args.synaptic, args.bootstrap, args.noninferiority_margin)
        markdown = agent_markdown(report)
        write_json(args.out / "report.json", report)
        (args.out / "report.md").write_text(markdown, encoding="utf-8")
        print(markdown, end="")
        comparison = report["comparison"]
        if args.require_noninferior and not comparison["quality_noninferior"]:
            return 2
        if args.min_token_savings is not None and comparison["token_savings_bootstrap_95_ci"][0] < args.min_token_savings:
            return 2
    elif args.command == "normalize-mini-swe":
        normalize_mini_swe(args)
    elif args.command == "mini-swe-overlay":
        mini_swe_overlay(args)
    elif args.command == "context-benchmark":
        context_benchmark(args)
    elif args.command == "context-corpus":
        context_corpus(args)
    elif args.command == "beir-run":
        beir_run(args)
    elif args.command == "beir-eval":
        cutoffs = sorted({int(value) for value in args.cutoffs.split(",") if int(value) > 0})
        report = beir_report(args.qrels, args.run, cutoffs)
        markdown = beir_markdown(report)
        write_json(args.out / "report.json", report)
        (args.out / "report.md").write_text(markdown, encoding="utf-8")
        print(markdown, end="")
    elif args.command == "history-candidates":
        history_candidates(args)
    elif args.command == "swebench-dataset":
        swebench_dataset(args)
    else:
        self_test()
    return 0


if __name__ == "__main__":
    sys.exit(main())
