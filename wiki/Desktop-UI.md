# Desktop UI

`synaptic-ui` is an optional native setup app for people who prefer not to configure a repository
graph, federated workspace, or MCP integration in the terminal. The CLI remains fully supported,
and the UI runs the same `synaptic` commands. On first launch it follows the operating system's
light or dark preference; choosing another theme saves that choice for later launches.

The **Tools** view exposes the complete CLI rather than a smaller desktop-only subset. Its
persistent catalog can be searched or filtered by capability. Selecting a tool opens guided
controls generated from the installed engine's help: nested actions first, followed by labeled
required fields, optional settings, defaults, choices, and a command preview. **Help** shows
the current help for the selected action. **Command** keeps direct command entry available for
unusual or newer engine options.

## Update or remove the app

Open **App** and choose **Add to applications** to install for the current user. Synaptic then
appears in Windows Start, `~/Applications` on macOS, or the Linux desktop application menu. This
does not require administrator access and does not pin the app automatically.

The same screen checks the latest GitHub Release. **Download and install** selects the archive for
the current platform, verifies its published SHA-256 checksum, replaces the desktop executable,
and updates the bundled CLI executables when they are beside it. Restart the app when prompted.

**Uninstall** removes the registered desktop app and its application-menu entry. It leaves
repositories, generated graphs, Synaptic settings, and any separately installed CLI in place.

## Install and launch

Release archives include `synaptic-ui` beside `synaptic`. From source, install both onto your
PATH:

```sh
cargo install --path bin/synaptic
cargo install --path bin/synaptic-ui
synaptic-ui
```

Downloading only `synaptic-ui` also works. On first launch, the app notices that its command
tools are missing, downloads the archive for the current operating system, verifies the published
checksum, and installs `synaptic` beside itself. Setup stays on one progress screen and continues
automatically when the command tools are ready. If the app's folder is read-only or the download
fails, the same screen explains what happened and offers **Try again**; technical details remain
collapsed unless opened.

The same Rust frontend builds natively on Windows, Linux, and macOS. Executables are compiled
per operating system; a Windows `.exe` is not a universal binary for the other platforms.

## Workflow

1. Choose **Single / monorepo** to extract one root directly, or **Federated workspace** to
   compose selected sources. You can also drop a folder anywhere in the window.
2. In single mode, review the detected packages and build with `synaptic extract .`.
3. In federated mode, select monorepo packages and nearby Git repositories. The UI writes
   `synaptic-workspace.toml` and runs `synaptic workspace build`.
4. Choose an assistant and select **Install & connect**. The UI runs the normal
   `synaptic install <host>` setup; Codex desktop uses its required global MCP entry.

Existing manifests are loaded rather than discarded. Declared Git, local-path, and subgraph
repositories remain available in the selection list. The built graph is written to
`synaptic-out/graph.json` as usual.

The UI looks for `synaptic` beside its own executable first, then on `PATH`. If neither is
available, it installs the verified release beside itself. Development or custom installations can
override that lookup with `SYNAPTIC_BIN`.

Repository discovery is local and bounded to three directory levels and 50 eligible Git
repositories. A build can use the network only when the selected manifest already requires a
remote Git or subgraph source.

## Command execution

The app launches `synaptic` directly without a command shell, so characters such as `;`, `|`, and
`$` are passed as literal arguments rather than executed by PowerShell, `cmd`, or a POSIX shell.
Quoted arguments and Windows paths are supported. stdout and stderr stream into the output panel.
Long-running commands such as `watch` and `serve` remain active until they finish or you select
**Stop**; commands that prompt or use stdio can receive lines through **Send input**. The command
controls scroll independently so the output dock remains visible at every supported window size.
