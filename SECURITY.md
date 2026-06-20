# Security Policy

## Supported Versions

CodeGraph is pre-1.0 and ships from a single version line (currently `0.2.x`).
Security fixes land in the latest release; there are no separate maintenance
branches for older versions.

| Version            | Supported          |
| ------------------ | ------------------ |
| latest `0.2.x`     | :white_check_mark: |
| older pre-releases | :x:                |

## Reporting a Vulnerability

Please report security issues privately. Do **not** open a public GitHub issue
for a suspected vulnerability.

- Preferred: open a private report through GitHub Security Advisories
  ("Security" tab -> "Report a vulnerability") on this repository.
- Include enough detail to reproduce: affected version/commit, a minimal repro,
  and the impact you observed.

There is no guaranteed response SLA for this project, but reports are reviewed on
a best-effort basis and fixes are released in the latest version line.

## Scope notes

- The `codegraph serve` MCP server is read-only by default. The command-running
  `speculate` tool is exposed only with the explicit `--allow-exec` opt-in; treat
  enabling it as granting the server permission to run this project's
  test/build commands.
- Over HTTP, the server enforces a `Host`/`Origin` allowlist (DNS-rebinding
  protection) for loopback/specific binds and an optional constant-time API-key
  check; a wildcard bind (`0.0.0.0`) intentionally disables the allowlist. See
  the [MCP Server](wiki/MCP-Server.md) and [Commands](wiki/Commands.md) docs.
