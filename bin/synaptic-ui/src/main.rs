#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2,
};
use synaptic_upgrade::{Release, github, target, updater, version_is_newer};
use synaptic_workspace::coordinate::{Coordinate, Ecosystem};
use synaptic_workspace::discover::{Member, discover_members, members_from_globs};
use synaptic_workspace::manifest::{
    RepoMember, WorkspaceManifest, WorkspaceMeta, load_manifest, write_manifest,
};
use synaptic_workspace::scan::{ScanOptions, discover_sibling_repos, relative_path};

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    panel_active: Color32,
    paper: Color32,
    muted: Color32,
    faint: Color32,
    border: Color32,
    mint: Color32,
    red: Color32,
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(22, 23, 21),
    panel: Color32::from_rgb(28, 30, 27),
    panel_active: Color32::from_rgb(37, 41, 34),
    paper: Color32::from_rgb(232, 229, 220),
    muted: Color32::from_rgb(159, 163, 154),
    faint: Color32::from_rgb(95, 101, 94),
    border: Color32::from_rgb(54, 59, 53),
    mint: Color32::from_rgb(142, 185, 102),
    red: Color32::from_rgb(226, 102, 92),
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(246, 246, 242),
    panel: Color32::from_rgb(237, 239, 233),
    panel_active: Color32::from_rgb(227, 232, 222),
    paper: Color32::from_rgb(36, 39, 34),
    muted: Color32::from_rgb(99, 105, 96),
    faint: Color32::from_rgb(137, 144, 135),
    border: Color32::from_rgb(199, 204, 197),
    mint: Color32::from_rgb(72, 112, 43),
    red: Color32::from_rgb(176, 63, 54),
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    Single,
    Federated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeMode {
    Dark,
    Light,
}

const THEME_STORAGE_KEY: &str = "theme";

impl ThemeMode {
    fn from_storage(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    fn from_system(theme: Option<egui::Theme>) -> Self {
        if theme == Some(egui::Theme::Light) {
            Self::Light
        } else {
            Self::Dark
        }
    }

    fn storage_value(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppView {
    Setup,
    Commands,
    App,
}

#[derive(Clone)]
enum CandidateSource {
    Member(String),
    Repo(RepoMember),
}

#[derive(Clone)]
struct Candidate {
    name: String,
    location: String,
    coordinate: Option<Coordinate>,
    selected: bool,
    source: CandidateSource,
}

impl Candidate {
    fn is_member(&self) -> bool {
        matches!(self.source, CandidateSource::Member(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateFilter {
    All,
    Selected,
    Workspace,
    Nearby,
}

struct Host {
    label: &'static str,
    platform: &'static str,
    global: bool,
}

const HOSTS: &[Host] = &[
    Host {
        label: "Codex desktop",
        platform: "codex",
        global: true,
    },
    Host {
        label: "Codex CLI",
        platform: "codex",
        global: false,
    },
    Host {
        label: "Claude Code",
        platform: "claude",
        global: false,
    },
    Host {
        label: "Cursor",
        platform: "cursor",
        global: false,
    },
    Host {
        label: "GitHub Copilot",
        platform: "copilot",
        global: false,
    },
    Host {
        label: "Gemini",
        platform: "gemini",
        global: false,
    },
    Host {
        label: "OpenCode",
        platform: "opencode",
        global: false,
    },
    Host {
        label: "AGENTS.md",
        platform: "agents",
        global: false,
    },
    Host {
        label: "Kilo Code",
        platform: "kilo",
        global: false,
    },
];

struct ToolSpec {
    name: &'static str,
    group: &'static str,
    summary: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GuideArgKind {
    Positional,
    Flag,
    Value,
}

struct GuideArgument {
    name: String,
    label: String,
    help: String,
    kind: GuideArgKind,
    required: bool,
    multiple: bool,
    default: Option<String>,
    choices: Vec<String>,
    enabled: bool,
    value: String,
}

struct GuideChoice {
    name: String,
    summary: String,
}

struct CommandGuide {
    path: Vec<String>,
    about: String,
    usage: String,
    subcommands: Vec<GuideChoice>,
    arguments: Vec<GuideArgument>,
    options: Vec<GuideArgument>,
}

impl CommandGuide {
    fn command_args(&self) -> Result<Vec<OsString>, String> {
        let mut args: Vec<OsString> = self.path.iter().cloned().map(Into::into).collect();
        for argument in &self.arguments {
            append_guide_value(&mut args, argument, false)?;
        }
        for option in &self.options {
            append_guide_value(&mut args, option, true)?;
        }
        Ok(args)
    }
}

const CLI_TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "extract",
        group: "Build",
        summary: "Create a code graph from this project.",
    },
    ToolSpec {
        name: "update",
        group: "Build",
        summary: "Refresh the graph after files change.",
    },
    ToolSpec {
        name: "watch",
        group: "Build",
        summary: "Keep the graph up to date while you work.",
    },
    ToolSpec {
        name: "workspace",
        group: "Build",
        summary: "Create and manage a graph made from several repositories.",
    },
    ToolSpec {
        name: "merge-graphs",
        group: "Build",
        summary: "Combine saved graphs.",
    },
    ToolSpec {
        name: "migrate",
        group: "Build",
        summary: "Move an older graph into the current storage format.",
    },
    ToolSpec {
        name: "cache",
        group: "Build",
        summary: "View or clear saved extraction data.",
    },
    ToolSpec {
        name: "query",
        group: "Explore",
        summary: "Find code related to a question or topic.",
    },
    ToolSpec {
        name: "path",
        group: "Explore",
        summary: "Show how two parts of the code connect.",
    },
    ToolSpec {
        name: "explain",
        group: "Explore",
        summary: "Show a code item and what surrounds it.",
    },
    ToolSpec {
        name: "affected",
        group: "Explore",
        summary: "See what could be affected by a change.",
    },
    ToolSpec {
        name: "references",
        group: "Explore",
        summary: "Find where a symbol is used.",
    },
    ToolSpec {
        name: "hazards",
        group: "Explore",
        summary: "Find code that may be hard to trace at runtime.",
    },
    ToolSpec {
        name: "search",
        group: "Explore",
        summary: "Run a precise search over code structure.",
    },
    ToolSpec {
        name: "export",
        group: "Explore",
        summary: "Save the graph in another format or send it elsewhere.",
    },
    ToolSpec {
        name: "prs",
        group: "Explore",
        summary: "Review a pull request and see its likely impact.",
    },
    ToolSpec {
        name: "diff",
        group: "Change",
        summary: "Compare the architecture between two versions.",
    },
    ToolSpec {
        name: "refactor",
        group: "Change",
        summary: "Plan a refactor and check it before applying.",
    },
    ToolSpec {
        name: "predict",
        group: "Change",
        summary: "Estimate the effect of a proposed change.",
    },
    ToolSpec {
        name: "contract",
        group: "Change",
        summary: "Create and verify rules for a change.",
    },
    ToolSpec {
        name: "speculate",
        group: "Change",
        summary: "Try a proposed change in a temporary copy.",
    },
    ToolSpec {
        name: "eval",
        group: "Change",
        summary: "Compare earlier predictions with what actually changed.",
    },
    ToolSpec {
        name: "sql",
        group: "Audit",
        summary: "Check SQL or get advice on a query.",
    },
    ToolSpec {
        name: "audit",
        group: "Audit",
        summary: "Check whether a project is ready to ship.",
    },
    ToolSpec {
        name: "vuln",
        group: "Audit",
        summary: "Check dependencies for known security problems.",
    },
    ToolSpec {
        name: "api",
        group: "Audit",
        summary: "Review and maintain API dependencies.",
    },
    ToolSpec {
        name: "ingest",
        group: "Integrate",
        summary: "Add information from outside sources to the graph.",
    },
    ToolSpec {
        name: "serve",
        group: "Integrate",
        summary: "Make the graph available to an assistant through MCP.",
    },
    ToolSpec {
        name: "memory",
        group: "Integrate",
        summary: "Search and manage saved knowledge about the project.",
    },
    ToolSpec {
        name: "global",
        group: "Integrate",
        summary: "Manage a graph that connects several repositories.",
    },
    ToolSpec {
        name: "install",
        group: "Integrate",
        summary: "Connect Synaptic to an assistant.",
    },
    ToolSpec {
        name: "uninstall",
        group: "Integrate",
        summary: "Remove an assistant connection.",
    },
    ToolSpec {
        name: "hook",
        group: "System",
        summary: "Set up or remove Synaptic's Git hooks.",
    },
    ToolSpec {
        name: "merge-driver",
        group: "System",
        summary: "Resolve graph changes during a Git merge.",
    },
    ToolSpec {
        name: "skill",
        group: "System",
        summary: "Check or rebuild the files assistants use.",
    },
    ToolSpec {
        name: "self-update",
        group: "System",
        summary: "Check for or install Synaptic updates.",
    },
];

const TOOL_GROUPS: &[&str] = &[
    "All",
    "Build",
    "Explore",
    "Change",
    "Audit",
    "Integrate",
    "System",
];

struct RunReport {
    title: String,
    ok: bool,
    output: String,
}

enum TaskEvent {
    Output(String),
    Finished { ok: bool, stopped: bool },
}

enum TaskControl {
    Input(String),
    Stop,
}

struct RunningTask {
    events: Receiver<TaskEvent>,
    controls: Sender<TaskControl>,
}

enum LifecycleEvent {
    Checked(Result<Release, String>),
    Installed(Result<String, String>),
    CommandToolsInstalled(Result<(), String>),
    AppRegistered(Result<PathBuf, String>),
}

enum LifecycleStatus {
    Idle,
    Checking,
    Available(Release),
    Installing,
    Current,
    Restart(String),
    Error(String),
}

enum CommandToolsStatus {
    Ready,
    Missing,
    Installing,
    Error(String),
}

enum AppInstallStatus {
    Portable,
    Installing,
    Installed(PathBuf),
    Error(String),
}

struct SynapticUi {
    view: AppView,
    mode: SetupMode,
    theme: ThemeMode,
    root: String,
    scan_root: String,
    workspace_name: String,
    default_branch: String,
    candidates: Vec<Candidate>,
    search: String,
    filter: CandidateFilter,
    host: usize,
    notice: Option<(bool, String)>,
    task: Option<RunningTask>,
    task_name: String,
    task_output: String,
    task_input: String,
    last_run: Option<RunReport>,
    mcp_connected: bool,
    tool_search: String,
    tool_group: usize,
    selected_tool: usize,
    command_input: String,
    guide: Option<CommandGuide>,
    guide_error: Option<String>,
    advanced_mode: bool,
    lifecycle: LifecycleStatus,
    lifecycle_events: Option<Receiver<LifecycleEvent>>,
    uninstall_confirm: bool,
    command_tools: CommandToolsStatus,
    theme_initialized: bool,
    app_install: AppInstallStatus,
}

impl SynapticUi {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let scan_root = root.parent().unwrap_or(&root).to_path_buf();
        let saved_theme = cc
            .storage
            .and_then(|storage| storage.get_string(THEME_STORAGE_KEY))
            .and_then(|value| ThemeMode::from_storage(&value));
        let theme = saved_theme.unwrap_or(ThemeMode::Dark);
        let command_tools = if synaptic_available() {
            CommandToolsStatus::Ready
        } else {
            CommandToolsStatus::Missing
        };
        let app_install = registered_desktop_app()
            .map(AppInstallStatus::Installed)
            .unwrap_or(AppInstallStatus::Portable);
        configure_style(&cc.egui_ctx, theme);
        let mut app = Self {
            view: AppView::Setup,
            mode: if root.join("synaptic-workspace.toml").is_file() {
                SetupMode::Federated
            } else {
                SetupMode::Single
            },
            theme,
            workspace_name: path_name(&root),
            root: root.display().to_string(),
            scan_root: scan_root.display().to_string(),
            default_branch: "main".into(),
            candidates: Vec::new(),
            search: String::new(),
            filter: CandidateFilter::All,
            host: 0,
            notice: None,
            task: None,
            task_name: String::new(),
            task_output: String::new(),
            task_input: String::new(),
            last_run: None,
            mcp_connected: false,
            tool_search: String::new(),
            tool_group: 0,
            selected_tool: 0,
            command_input: "extract .".into(),
            guide: None,
            guide_error: None,
            advanced_mode: false,
            lifecycle: LifecycleStatus::Idle,
            lifecycle_events: None,
            uninstall_confirm: false,
            command_tools,
            theme_initialized: saved_theme.is_some(),
            app_install,
        };
        app.discover();
        if matches!(app.command_tools, CommandToolsStatus::Ready) {
            app.select_tool(0);
        } else {
            app.install_command_tools();
        }
        app
    }

    fn root_path(&self) -> PathBuf {
        PathBuf::from(self.root.trim())
    }

    fn graph_exists(&self) -> bool {
        self.root_path().join("synaptic-out/graph.json").is_file()
    }

    fn discover(&mut self) {
        match self.discover_inner() {
            Ok(count) => {
                self.notice = Some((
                    true,
                    if self.mode == SetupMode::Single {
                        let packages = self
                            .candidates
                            .iter()
                            .filter(|item| item.is_member())
                            .count();
                        format!("Repository ready. Found {packages} package roots.")
                    } else {
                        format!("Found {count} eligible source groups.")
                    },
                ));
            }
            Err(message) => self.notice = Some((false, message)),
        }
    }

    fn discover_inner(&mut self) -> Result<usize, String> {
        let root = checked_dir(&self.root, "Workspace root")?;
        let scan_root = checked_dir(&self.scan_root, "Search area")?;
        self.root = root.display().to_string();
        self.scan_root = scan_root.display().to_string();

        let existing = load_manifest(&root).map_err(|error| error.to_string())?;
        if let Some(manifest) = &existing {
            if !manifest.workspace.name.is_empty() {
                self.workspace_name.clone_from(&manifest.workspace.name);
            }
            self.default_branch
                .clone_from(&manifest.workspace.default_branch);
        } else {
            self.workspace_name = path_name(&root);
            self.default_branch = "main".into();
        }

        let existing_members = match &existing {
            Some(manifest) => members_from_globs(&root, &manifest.workspace.members)
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        let selected_paths: HashSet<PathBuf> = existing_members
            .iter()
            .map(|member| canonical(&member.path))
            .collect();
        let mut members = discover_members(&root);
        merge_members(&mut members, existing_members);
        if members.is_empty() {
            members.push(Member {
                tag: path_name(&root),
                path: root.clone(),
                coordinate: None,
            });
        }

        let had_manifest = existing.is_some();
        let mut candidates: Vec<Candidate> = members
            .into_iter()
            .map(|member| {
                let relative = relative_path(&root, &member.path);
                Candidate {
                    name: member.tag,
                    location: relative.clone(),
                    coordinate: member.coordinate,
                    selected: !had_manifest || selected_paths.contains(&canonical(&member.path)),
                    source: CandidateSource::Member(relative),
                }
            })
            .collect();

        let mut repo_paths = HashSet::new();
        if let Some(manifest) = existing {
            for repo in manifest.repos {
                if let Some(path) = &repo.path {
                    repo_paths.insert(canonical(&root.join(path)));
                }
                candidates.push(Candidate {
                    name: repo.name.clone(),
                    location: repo_location(&repo),
                    coordinate: repo.coordinate.clone(),
                    selected: true,
                    source: CandidateSource::Repo(repo),
                });
            }
        }

        let scan =
            discover_sibling_repos(&scan_root, &ScanOptions { depth: 3, max: 50 }, Some(&root));
        for repo in scan.repos {
            if !repo_paths.insert(canonical(&repo.path)) {
                continue;
            }
            let relative = relative_path(&root, &repo.path);
            candidates.push(Candidate {
                name: repo.name.clone(),
                location: relative.clone(),
                coordinate: repo.coordinate,
                selected: false,
                source: CandidateSource::Repo(RepoMember {
                    name: repo.name,
                    tag: None,
                    coordinate: None,
                    git: None,
                    rev: None,
                    subgraph: None,
                    path: Some(relative),
                }),
            });
        }

        candidates.sort_by(|a, b| {
            b.is_member()
                .cmp(&a.is_member())
                .then_with(|| a.name.cmp(&b.name))
        });
        self.candidates = candidates;
        Ok(self.candidates.len())
    }

    fn save_manifest(&mut self) -> Result<(), String> {
        let root = checked_dir(&self.root, "Workspace root")?;
        let selected: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.selected)
            .collect();
        if selected.is_empty() {
            return Err("Select at least one source group before building.".into());
        }
        let name = self.workspace_name.trim();
        if name.is_empty() {
            return Err("Workspace name cannot be empty.".into());
        }

        let manifest = manifest_from_selection(name, &self.default_branch, selected);
        write_manifest(&root, &manifest).map_err(|error| error.to_string())
    }

    fn build(&mut self) {
        if self.mode == SetupMode::Federated
            && let Err(message) = self.save_manifest()
        {
            self.notice = Some((false, message));
            return;
        }
        let (title, args) = build_command(self.mode);
        self.run_command(title, args);
    }

    fn install(&mut self) {
        let host = &HOSTS[self.host];
        let mut args = vec!["install".into(), host.platform.into()];
        if host.global {
            args.push("--global".into());
        }
        self.run_command(&format!("Connecting {}", host.label), args);
    }

    fn run_command(&mut self, title: &str, args: Vec<OsString>) {
        if self.task.is_some() {
            return;
        }
        let root = self.root_path();
        let binary = synaptic_binary();
        let command_line = command_text(&args);
        self.task_output = format!("$ synaptic {command_line}\n\n");
        self.task = Some(spawn_process(binary, root, args));
        self.task_name = title.to_string();
        self.notice = None;
    }

    fn run_entered_command(&mut self) {
        match parse_command_line(&self.command_input) {
            Ok(mut args) => {
                if matches!(
                    args.first().and_then(|arg| arg.to_str()),
                    Some("synaptic" | "synaptic.exe" | "syn" | "syn.exe")
                ) {
                    args.remove(0);
                }
                if args.is_empty() {
                    self.notice = Some((
                        false,
                        "Enter a Synaptic command or choose one from the catalog.".into(),
                    ));
                    return;
                }
                let title = format!("Running {}", args[0].to_string_lossy());
                self.run_command(&title, args);
            }
            Err(message) => self.notice = Some((false, message)),
        }
    }

    fn select_tool(&mut self, index: usize) {
        self.selected_tool = index;
        self.advanced_mode = false;
        let name = CLI_TOOLS[index].name;
        self.command_input = name.into();
        self.load_guide(vec![name.into()]);
    }

    fn load_guide(&mut self, path: Vec<String>) {
        match load_command_guide(&path, &self.root_path()) {
            Ok(guide) => {
                self.command_input = command_text(
                    &guide
                        .command_args()
                        .unwrap_or_else(|_| path.iter().cloned().map(Into::into).collect()),
                );
                self.guide = Some(guide);
                self.guide_error = None;
            }
            Err(message) => {
                self.guide = None;
                self.guide_error = Some(message);
            }
        }
    }

    fn select_subcommand(&mut self, name: &str) {
        let mut path = self
            .guide
            .as_ref()
            .map(|guide| guide.path.clone())
            .unwrap_or_default();
        path.push(name.into());
        self.load_guide(path);
    }

    fn guide_back(&mut self) {
        let Some(mut path) = self.guide.as_ref().map(|guide| guide.path.clone()) else {
            return;
        };
        if path.len() > 1 {
            path.pop();
            self.load_guide(path);
        }
    }

    fn run_guided_command(&mut self) {
        let args = self
            .guide
            .as_ref()
            .ok_or_else(|| "The guided command definition is unavailable.".to_string())
            .and_then(CommandGuide::command_args);
        match args {
            Ok(args) => {
                let title = format!("Running {}", command_text(&args));
                self.run_command(&title, args);
            }
            Err(message) => self.notice = Some((false, message)),
        }
    }

    fn run_tool_help(&mut self) {
        let path = self
            .guide
            .as_ref()
            .map(|guide| guide.path.clone())
            .unwrap_or_else(|| vec![CLI_TOOLS[self.selected_tool].name.into()]);
        let mut args: Vec<OsString> = path.iter().cloned().map(Into::into).collect();
        args.push("--help".into());
        let label = if path.is_empty() {
            "Synaptic".into()
        } else {
            path.join(" ")
        };
        self.run_command(&format!("Showing {label} help"), args);
    }

    fn send_task_input(&mut self) {
        if self.task_input.is_empty() {
            return;
        }
        if let Some(task) = &self.task {
            let input = std::mem::take(&mut self.task_input);
            let _ = task.controls.send(TaskControl::Input(input));
        }
    }

    fn stop_task(&mut self) {
        if let Some(task) = &self.task {
            let _ = task.controls.send(TaskControl::Stop);
        }
    }

    fn poll_task(&mut self) {
        let events: Vec<TaskEvent> = self
            .task
            .as_ref()
            .map(|task| task.events.try_iter().take(128).collect())
            .unwrap_or_default();
        let mut finished = None;
        for event in events {
            match event {
                TaskEvent::Output(output) => append_output(&mut self.task_output, &output),
                TaskEvent::Finished { ok, stopped } => finished = Some((ok, stopped)),
            }
        }
        if let Some((ok, stopped)) = finished {
            let report = RunReport {
                title: self.task_name.clone(),
                ok,
                output: self.task_output.trim_end().to_string(),
            };
            if report.ok && report.title.starts_with("Connecting ") {
                self.mcp_connected = true;
            }
            self.notice = Some((
                report.ok || stopped,
                if stopped {
                    format!("{} stopped.", report.title)
                } else if report.ok {
                    format!("{} finished.", report.title)
                } else {
                    format!("{} failed. See command output.", report.title)
                },
            ));
            self.last_run = Some(report);
            self.task = None;
            self.task_name.clear();
        }
    }

    fn check_for_updates(&mut self) {
        if self.lifecycle_events.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = github::latest_release().map_err(|error| error.to_string());
            let _ = sender.send(LifecycleEvent::Checked(result));
        });
        self.lifecycle = LifecycleStatus::Checking;
        self.lifecycle_events = Some(receiver);
    }

    fn install_command_tools(&mut self) {
        if self.lifecycle_events.is_some() {
            return;
        }
        let Some(triple) = target::current_target() else {
            self.command_tools = CommandToolsStatus::Error(
                "Synaptic does not publish command tools for this platform yet.".into(),
            );
            return;
        };
        let destination = match std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        {
            Some(path) => path,
            None => {
                self.command_tools = CommandToolsStatus::Error(
                    "The app could not find its installation folder.".into(),
                );
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = github::latest_release()
                .map_err(|error| error.to_string())
                .and_then(|release| {
                    updater::install_cli(&release, triple, &destination)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                });
            let _ = sender.send(LifecycleEvent::CommandToolsInstalled(result));
        });
        self.command_tools = CommandToolsStatus::Installing;
        self.lifecycle_events = Some(receiver);
    }

    fn register_app(&mut self) {
        if self.lifecycle_events.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(LifecycleEvent::AppRegistered(register_desktop_app()));
        });
        self.app_install = AppInstallStatus::Installing;
        self.lifecycle_events = Some(receiver);
    }

    fn open_registered_app(&mut self, ctx: &egui::Context) {
        let AppInstallStatus::Installed(path) = &self.app_install else {
            return;
        };
        match Command::new(path).current_dir(self.root_path()).spawn() {
            Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(error) => self.app_install = AppInstallStatus::Error(error.to_string()),
        }
    }

    fn install_update(&mut self) {
        let LifecycleStatus::Available(release) = &self.lifecycle else {
            return;
        };
        let Some(triple) = target::current_target() else {
            self.lifecycle = LifecycleStatus::Error(format!(
                "No download is published for this platform. See {}",
                synaptic_upgrade::releases_url()
            ));
            return;
        };
        let archive = target::archive_name(triple);
        if updater::find_asset(release, &archive).is_none() {
            self.lifecycle = LifecycleStatus::Error(format!(
                "Version {} does not include {archive}.",
                release.version
            ));
            return;
        }
        let release = release.clone();
        let version = release.version.trim_start_matches('v').to_string();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = updater::apply_update(&release, triple)
                .map(|()| version)
                .map_err(|error| error.to_string());
            let _ = sender.send(LifecycleEvent::Installed(result));
        });
        self.lifecycle = LifecycleStatus::Installing;
        self.lifecycle_events = Some(receiver);
    }

    fn poll_lifecycle(&mut self) {
        let event = self
            .lifecycle_events
            .as_ref()
            .and_then(|events| match events.try_recv() {
                Ok(event) => Some(event),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    let error = "The task stopped unexpectedly.".to_string();
                    Some(
                        if matches!(self.command_tools, CommandToolsStatus::Installing) {
                            LifecycleEvent::CommandToolsInstalled(Err(error))
                        } else if matches!(self.app_install, AppInstallStatus::Installing) {
                            LifecycleEvent::AppRegistered(Err(error))
                        } else {
                            LifecycleEvent::Installed(Err(error))
                        },
                    )
                }
            });
        let Some(event) = event else {
            return;
        };
        self.lifecycle_events = None;
        let event = match event {
            LifecycleEvent::CommandToolsInstalled(result) => {
                match result {
                    Ok(()) if synaptic_available() => {
                        self.command_tools = CommandToolsStatus::Ready;
                        self.notice = Some((
                            true,
                            "Synaptic is ready. The command tools were installed.".into(),
                        ));
                        self.select_tool(0);
                    }
                    Ok(()) => {
                        self.command_tools = CommandToolsStatus::Error(
                            "The download finished, but the command tools could not start.".into(),
                        );
                    }
                    Err(error) => self.command_tools = CommandToolsStatus::Error(error),
                }
                return;
            }
            LifecycleEvent::AppRegistered(result) => {
                self.app_install = match result {
                    Ok(path) => {
                        self.notice = Some((
                            true,
                            format!("Synaptic was added to {}.", application_menu_name()),
                        ));
                        AppInstallStatus::Installed(path)
                    }
                    Err(error) => AppInstallStatus::Error(error),
                };
                return;
            }
            event => event,
        };
        self.lifecycle = match event {
            LifecycleEvent::Checked(Ok(release))
                if version_is_newer(env!("CARGO_PKG_VERSION"), &release.version) =>
            {
                LifecycleStatus::Available(release)
            }
            LifecycleEvent::Checked(Ok(_)) => LifecycleStatus::Current,
            LifecycleEvent::Checked(Err(error)) | LifecycleEvent::Installed(Err(error)) => {
                LifecycleStatus::Error(error)
            }
            LifecycleEvent::Installed(Ok(version)) => LifecycleStatus::Restart(version),
            LifecycleEvent::CommandToolsInstalled(_) | LifecycleEvent::AppRegistered(_) => {
                unreachable!()
            }
        };
    }

    fn restart(&mut self, ctx: &egui::Context) {
        let result = std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|path| {
                Command::new(path)
                    .current_dir(self.root_path())
                    .spawn()
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(error) => self.lifecycle = LifecycleStatus::Error(error),
        }
    }

    fn remove_app(&mut self, ctx: &egui::Context) {
        match uninstall_desktop_app() {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(error) => {
                self.lifecycle = LifecycleStatus::Error(error);
                self.uninstall_confirm = false;
            }
        }
    }

    fn choose_root(&mut self) {
        let current = self.root_path();
        if let Some(path) = rfd::FileDialog::new().set_directory(current).pick_folder() {
            self.root = path.display().to_string();
            self.scan_root = path.parent().unwrap_or(&path).display().to_string();
            self.discover();
        }
    }

    fn choose_scan_root(&mut self) {
        let current = PathBuf::from(&self.scan_root);
        if let Some(path) = rfd::FileDialog::new().set_directory(current).pick_folder() {
            self.scan_root = path.display().to_string();
            self.discover();
        }
    }

    fn selected_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.selected)
            .count()
    }

    fn graph_source_count(&self) -> usize {
        if self.mode == SetupMode::Single {
            1
        } else {
            self.selected_count()
        }
    }

    fn can_build(&self) -> bool {
        self.task.is_none() && self.graph_source_count() > 0
    }

    fn side_rail(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        egui::Panel::left("steps")
            .exact_size(214.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(22, 26)),
            )
            .show(ui, |ui| {
                ui.label(RichText::new("Synaptic").strong().size(17.0).color(p.paper));
                ui.label(RichText::new("Desktop").size(10.0).color(p.muted));
                ui.add_space(28.0);
                ui.selectable_value(&mut self.view, AppView::Setup, "Set up");
                ui.selectable_value(&mut self.view, AppView::Commands, "Tools");
                ui.selectable_value(&mut self.view, AppView::App, "App");
                ui.add_space(30.0);
                match self.view {
                    AppView::Setup => {
                        step(
                            ui,
                            "01",
                            if self.mode == SetupMode::Single {
                                "Choose repository"
                            } else {
                                "Choose sources"
                            },
                            !self.candidates.is_empty(),
                        );
                        rail_line(ui, true);
                        step(ui, "02", "Build graph", self.graph_exists());
                        rail_line(ui, self.graph_exists());
                        step(ui, "03", "Connect MCP", self.mcp_connected);
                    }
                    AppView::Commands => {
                        ui.label(RichText::new("Available tools").size(9.0).color(p.faint));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!(
                                "{} tools in {} categories",
                                CLI_TOOLS.len(),
                                TOOL_GROUPS.len() - 1
                            ))
                            .size(11.0)
                            .color(p.muted),
                        );
                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(14.0);
                        let tool = &CLI_TOOLS[self.selected_tool];
                        ui.label(
                            RichText::new(group_label(tool.group))
                                .size(9.0)
                                .color(p.muted),
                        );
                        ui.label(
                            RichText::new(tool_label(tool.name))
                                .strong()
                                .size(15.0)
                                .color(p.paper),
                        );
                        ui.label(RichText::new(tool.summary).size(10.5).color(p.muted));
                    }
                    AppView::App => {
                        ui.label(RichText::new("Desktop app").size(9.0).color(p.faint));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(p.paper),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Install, update, or remove the desktop app.")
                                .size(10.5)
                                .color(p.muted),
                        );
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .monospace()
                            .size(10.0)
                            .color(p.faint),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(match self.view {
                            AppView::Commands => format!("{} tools", CLI_TOOLS.len()),
                            AppView::App => "Desktop app settings".to_string(),
                            AppView::Setup if self.mode == SetupMode::Single => {
                                "Single repository mode".to_string()
                            }
                            AppView::Setup => format!(
                                "{} of {} groups selected",
                                self.selected_count(),
                                self.candidates.len()
                            ),
                        })
                        .size(11.0)
                        .color(p.muted),
                    );
                });
            });
    }

    fn header(&mut self, ui: &mut egui::Ui, show_graph: bool) {
        let p = palette(ui);
        let previous_theme = self.theme;
        ui.horizontal(|ui| {
            if self.view == AppView::Setup {
                ui.label(
                    RichText::new(if self.mode == SetupMode::Single {
                        "Repository setup"
                    } else {
                        "Workspace setup"
                    })
                    .size(10.0)
                    .color(p.muted),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.selectable_value(&mut self.theme, ThemeMode::Light, "Light");
                ui.selectable_value(&mut self.theme, ThemeMode::Dark, "Dark");
                ui.label(RichText::new("Theme").size(9.0).color(p.faint));
            });
        });
        if self.theme != previous_theme {
            configure_style(ui.ctx(), self.theme);
        }
        if self.view == AppView::Commands {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Tools").color(p.paper));
                ui.label(
                    RichText::new(format!("{} available", CLI_TOOLS.len()))
                        .size(9.5)
                        .color(p.faint),
                );
            });
            ui.label(
                RichText::new("Run any Synaptic task without writing a command.")
                    .size(11.5)
                    .color(p.muted),
            );
            ui.add_space(10.0);
            ui.separator();
            return;
        }
        if self.view == AppView::App {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Synaptic").color(p.paper));
                ui.label(
                    RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                        .size(9.5)
                        .color(p.faint),
                );
            });
            ui.label(
                RichText::new("Install, update, or remove the desktop app.")
                    .size(11.5)
                    .color(p.muted),
            );
            ui.add_space(10.0);
            ui.separator();
            return;
        }
        ui.add_space(7.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading(
                    RichText::new(if self.mode == SetupMode::Single {
                        "Map one repository."
                    } else {
                        "Choose what belongs together."
                    })
                    .color(p.paper),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if self.mode == SetupMode::Single {
                        "Build a graph directly from one project, then connect it to your assistant."
                    } else {
                        "Select local packages and nearby repositories. Synaptic federates them into one graph."
                    })
                    .size(13.0)
                    .color(p.muted),
                );
            });
            if show_graph {
                ui.with_layout(Layout::right_to_left(Align::Center), graph_mark);
            }
        });
        ui.add_space(16.0);
        ui.separator();
    }

    fn mode_switcher(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("SETUP MODE")
                            .monospace()
                            .size(9.0)
                            .color(p.faint),
                    );
                    ui.selectable_value(&mut self.mode, SetupMode::Single, "Single / monorepo");
                    ui.selectable_value(
                        &mut self.mode,
                        SetupMode::Federated,
                        "Federated workspace",
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(if self.mode == SetupMode::Single {
                            "Extracts this root directly as one graph."
                        } else {
                            "Writes a workspace manifest and builds selected sources."
                        })
                        .size(10.5)
                        .color(p.muted),
                    );
                });
            });
    }

    fn compact_progress(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Synaptic").strong().color(p.paper));
            ui.separator();
            ui.selectable_value(&mut self.view, AppView::Setup, "Set up");
            ui.selectable_value(&mut self.view, AppView::Commands, "Tools");
            ui.selectable_value(&mut self.view, AppView::App, "App");
        });
        ui.add_space(14.0);
    }

    fn workspace_toolbar(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.mode == SetupMode::Single {
                    "REPOSITORY ROOT"
                } else {
                    "WORKSPACE ROOT"
                })
                .monospace()
                .size(9.5)
                .color(p.muted),
            );
            ui.label(
                RichText::new("Drop a folder anywhere in the window")
                    .size(10.0)
                    .color(p.faint),
            );
        });
        ui.add_space(4.0);
        let path_width = (ui.available_width() - 216.0).max(180.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                [path_width, 36.0],
                egui::TextEdit::singleline(&mut self.root)
                    .font(egui::TextStyle::Monospace)
                    .margin(Vec2::new(10.0, 8.0)),
            );
            if secondary_button(ui, "Choose").clicked() {
                self.choose_root();
            }
            if secondary_button(ui, "Refresh").clicked() {
                self.discover();
            }
        });
        if self.mode == SetupMode::Federated {
            egui::CollapsingHeader::new(
                RichText::new("Nearby discovery settings")
                    .size(10.5)
                    .color(p.muted),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 118.0).max(180.0);
                    ui.add_sized(
                        [width, 34.0],
                        egui::TextEdit::singleline(&mut self.scan_root)
                            .font(egui::TextStyle::Monospace)
                            .margin(Vec2::new(9.0, 7.0)),
                    );
                    if secondary_button(ui, "Change area").clicked() {
                        self.choose_scan_root();
                    }
                });
                ui.label(
                    RichText::new("Scans 3 levels, up to 50 Git repositories.")
                        .size(9.5)
                        .color(p.faint),
                );
            });
        }
    }

    fn source_browser(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let federated = self.mode == SetupMode::Federated;
        ui.horizontal(|ui| {
            ui.label(RichText::new("01").monospace().size(10.0).color(p.mint));
            ui.label(
                RichText::new(if federated {
                    "Source groups"
                } else {
                    "Repository contents"
                })
                .strong()
                .size(18.0)
                .color(p.paper),
            );
            let found = if federated {
                self.candidates.len()
            } else {
                self.candidates
                    .iter()
                    .filter(|item| item.is_member())
                    .count()
            };
            ui.label(
                RichText::new(format!("{found} found"))
                    .monospace()
                    .size(9.5)
                    .color(p.muted),
            );
        });
        ui.add_space(7.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                [240.0, 32.0],
                egui::TextEdit::singleline(&mut self.search).hint_text("Filter sources…"),
            );
            if federated {
                for (filter, label) in [
                    (CandidateFilter::All, "All"),
                    (CandidateFilter::Selected, "Selected"),
                    (CandidateFilter::Workspace, "Workspace"),
                    (CandidateFilter::Nearby, "Nearby"),
                ] {
                    ui.selectable_value(&mut self.filter, filter, label);
                }
            } else {
                ui.label(
                    RichText::new("All detected packages are extracted as one graph.")
                        .size(10.5)
                        .color(p.muted),
                );
            }
        });

        let visible: Vec<usize> = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                candidate_visible(
                    candidate,
                    if federated {
                        self.filter
                    } else {
                        CandidateFilter::Workspace
                    },
                    &self.search,
                )
                .then_some(index)
            })
            .collect();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if federated {
                    format!(
                        "{} SHOWN · {} SELECTED",
                        visible.len(),
                        self.selected_count()
                    )
                } else {
                    format!("{} PACKAGES SHOWN · ONE REPOSITORY GRAPH", visible.len())
                })
                .monospace()
                .size(9.5)
                .color(p.muted),
            );
            if federated {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if quiet_button(ui, "Clear shown").clicked() {
                        for &index in &visible {
                            self.candidates[index].selected = false;
                        }
                    }
                    if quiet_button(ui, "Select shown").clicked() {
                        for &index in &visible {
                            self.candidates[index].selected = true;
                        }
                    }
                });
            }
        });
        ui.add_space(4.0);

        let columns = if ui.available_width() >= 1040.0 {
            3
        } else if ui.available_width() >= 650.0 {
            2
        } else {
            1
        };
        let height = ui.available_height().max(180.0);
        egui::ScrollArea::vertical()
            .id_salt("source-browser")
            .max_height(height)
            .show(ui, |ui| {
                if visible.is_empty() {
                    ui.add_space(24.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("No source groups match this view.").color(p.muted));
                    });
                } else {
                    ui.columns(columns, |column| {
                        for (position, &index) in visible.iter().enumerate() {
                            candidate_row(
                                &mut column[position % columns],
                                &mut self.candidates[index],
                                federated,
                            );
                        }
                    });
                }
            });
    }

    fn command_center(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Project folder").size(9.0).color(p.faint));
                    let width = (ui.available_width() - 94.0).max(180.0);
                    ui.add_sized(
                        [width, 32.0],
                        egui::TextEdit::singleline(&mut self.root)
                            .font(egui::TextStyle::Monospace)
                            .margin(Vec2::new(9.0, 6.0)),
                    );
                    if secondary_button(ui, "Choose").clicked() {
                        self.choose_root();
                    }
                });
            });
        ui.add_space(10.0);
        let height = ui.available_height().max(1.0);
        if ui.available_width() >= 760.0 {
            let catalog_width = if ui.available_width() >= 1180.0 {
                360.0
            } else {
                310.0
            };
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(catalog_width, height),
                    Layout::top_down(Align::Min),
                    |ui| self.tool_catalog(ui, height),
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), height),
                    Layout::top_down(Align::Min),
                    |ui| self.command_editor(ui, height),
                );
            });
        } else {
            egui::CollapsingHeader::new(
                RichText::new(format!("Browse {} tools", CLI_TOOLS.len()))
                    .strong()
                    .color(p.paper),
            )
            .default_open(true)
            .show(ui, |ui| self.tool_catalog(ui, 330.0));
            ui.add_space(10.0);
            self.command_editor(ui, 480.0);
        }
    }

    fn tool_catalog(&mut self, ui: &mut egui::Ui, height: f32) {
        let p = palette(ui);
        ui.set_min_height(height);
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_height((height - 24.0).max(180.0));
                let query = self.tool_search.trim().to_lowercase();
                let group = TOOL_GROUPS[self.tool_group];
                let visible_count = CLI_TOOLS
                    .iter()
                    .filter(|tool| tool_visible(tool, group, &query))
                    .count();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("All tools")
                            .strong()
                            .size(10.0)
                            .color(p.paper),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{visible_count} shown"))
                                .monospace()
                                .size(9.0)
                                .color(p.faint),
                        );
                    });
                });
                ui.add_sized(
                    [ui.available_width(), 31.0],
                    egui::TextEdit::singleline(&mut self.tool_search).hint_text("Search tools"),
                );
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    for (index, name) in TOOL_GROUPS.iter().enumerate() {
                        ui.selectable_value(&mut self.tool_group, index, group_label(name));
                    }
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(2.0);
                let group = TOOL_GROUPS[self.tool_group];
                let mut chosen = None;
                egui::ScrollArea::vertical()
                    .id_salt("tool-catalog")
                    .auto_shrink([false, false])
                    .max_height((height - 145.0).max(140.0))
                    .show(ui, |ui| {
                        for (index, tool) in CLI_TOOLS
                            .iter()
                            .enumerate()
                            .filter(|(_, tool)| tool_visible(tool, group, &query))
                        {
                            if tool_row(ui, tool, self.selected_tool == index).clicked() {
                                chosen = Some(index);
                            }
                        }
                    });
                if let Some(index) = chosen {
                    self.select_tool(index);
                }
            });
    }

    fn command_editor(&mut self, ui: &mut egui::Ui, height: f32) {
        let p = palette(ui);
        let tool_group = CLI_TOOLS[self.selected_tool].group;
        let tool_summary = CLI_TOOLS[self.selected_tool].summary;
        let path = self
            .guide
            .as_ref()
            .map(|guide| guide.path.clone())
            .unwrap_or_else(|| vec![CLI_TOOLS[self.selected_tool].name.into()]);
        let command_name = path.last().map(String::as_str).unwrap_or("help");
        let command_title = if path.len() == 1 {
            tool_label(command_name)
        } else {
            friendly_label(command_name)
        };
        let summary = self
            .guide
            .as_ref()
            .map(|guide| guide.about.as_str())
            .filter(|summary| !summary.is_empty())
            .unwrap_or(tool_summary)
            .to_string();
        ui.set_min_height(height);
        let (controls_height, output_height) = command_pane_heights(height);
        egui::ScrollArea::vertical()
            .id_salt("command-controls")
            .auto_shrink([false, false])
            .min_scrolled_height(controls_height)
            .max_height(controls_height)
            .show(ui, |ui| {
                let mut back_requested = false;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(group_label(tool_group))
                                .size(8.5)
                                .color(p.muted),
                        );
                        ui.label(
                            RichText::new(&command_title)
                                .strong()
                                .size(22.0)
                                .color(p.paper),
                        );
                        if self.advanced_mode {
                            ui.label(
                                RichText::new(format!("synaptic {}", path.join(" ")))
                                    .monospace()
                                    .size(9.0)
                                    .color(p.faint),
                            );
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(if self.task.is_some() {
                                "Running"
                            } else {
                                "Ready"
                            })
                            .size(9.0)
                            .color(if self.task.is_some() { p.mint } else { p.faint }),
                        );
                        if path.len() > 1 && quiet_button(ui, "‹ Back").clicked() {
                            back_requested = true;
                        }
                    });
                });
                if back_requested {
                    self.guide_back();
                    return;
                }
                ui.label(RichText::new(&summary).size(12.0).color(p.muted));
                ui.add_space(12.0);
                let previous_mode = self.advanced_mode;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Options").size(9.0).color(p.faint));
                    ui.selectable_value(&mut self.advanced_mode, false, "Form");
                    ui.selectable_value(&mut self.advanced_mode, true, "Command");
                });
                if self.advanced_mode
                    && !previous_mode
                    && let Some(guide) = &self.guide
                    && let Ok(args) = guide.command_args()
                {
                    self.command_input = command_text(&args);
                }

                let mut chosen_subcommand = None;
                if self.advanced_mode {
                    egui::Frame::new()
                        .fill(p.panel)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(4)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("$ synaptic")
                                        .strong()
                                        .monospace()
                                        .size(11.0)
                                        .color(p.mint),
                                );
                                ui.add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::TextEdit::singleline(&mut self.command_input)
                                        .font(egui::TextStyle::Monospace)
                                        .hint_text("query \"authentication flow\" --max-nodes 40"),
                                );
                            });
                        });
                    ui.label(
                        RichText::new("Runs exactly as entered, without a shell.")
                            .size(9.0)
                            .color(p.faint),
                    );
                } else if let Some(message) = &self.guide_error {
                    ui.label(RichText::new(message).size(10.5).color(p.red));
                    ui.label(
                        RichText::new("Use Command to type it manually.")
                            .size(9.5)
                            .color(p.muted),
                    );
                } else if let Some(guide) = self.guide.as_mut() {
                    if !guide.subcommands.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Choose an action")
                                    .strong()
                                    .size(9.0)
                                    .color(p.paper),
                            );
                            ui.label(
                                RichText::new(format!("{} actions", guide.subcommands.len()))
                                    .monospace()
                                    .size(8.5)
                                    .color(p.faint),
                            );
                        });
                        for choice in &guide.subcommands {
                            if guide_choice_row(ui, choice).clicked() {
                                chosen_subcommand = Some(choice.name.clone());
                            }
                        }
                    } else {
                        if guide.arguments.is_empty()
                            && !guide.options.iter().any(|option| option.required)
                        {
                            ui.label(RichText::new("No setup needed.").size(10.5).color(p.muted));
                        }
                        for argument in &mut guide.arguments {
                            guide_argument_ui(ui, argument);
                        }
                        for option in guide.options.iter_mut().filter(|option| option.required) {
                            guide_argument_ui(ui, option);
                        }
                        let optional = guide
                            .options
                            .iter()
                            .filter(|option| !option.required)
                            .count();
                        if optional > 0 {
                            egui::CollapsingHeader::new(
                                RichText::new(format!("Optional settings ({optional})"))
                                    .strong()
                                    .color(p.paper),
                            )
                            .show(ui, |ui| {
                                for option in
                                    guide.options.iter_mut().filter(|option| !option.required)
                                {
                                    guide_argument_ui(ui, option);
                                }
                            });
                        }
                        if let Ok(args) = guide.command_args() {
                            self.command_input = command_text(&args);
                        }
                        egui::CollapsingHeader::new(
                            RichText::new("Command preview").size(9.0).color(p.faint),
                        )
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("$ synaptic {}", self.command_input))
                                        .monospace()
                                        .size(10.0)
                                        .color(p.paper),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&guide.usage);
                        });
                    }
                }
                if let Some(name) = chosen_subcommand {
                    self.select_subcommand(&name);
                    return;
                }

                let guided_ready = self.guide.as_ref().is_some_and(|guide| {
                    guide.subcommands.is_empty() && guide.command_args().is_ok()
                });
                let mut run_requested = false;
                let mut help_requested = false;
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if self.task.is_some() {
                        if secondary_button(ui, "Stop").clicked() {
                            self.stop_task();
                        }
                    } else {
                        let can_run = if self.advanced_mode {
                            !self.command_input.trim().is_empty()
                        } else {
                            guided_ready
                        };
                        if ui.add_enabled(can_run, primary_button(p, "Run")).clicked() {
                            run_requested = true;
                        }
                        if secondary_button(ui, "Help").clicked() {
                            help_requested = true;
                        }
                    }
                });
                if run_requested {
                    if self.advanced_mode {
                        self.run_entered_command();
                    } else {
                        self.run_guided_command();
                    }
                }
                if help_requested {
                    self.run_tool_help();
                }
                if let Some((ok, message)) = &self.notice {
                    ui.label(RichText::new(message).size(10.0).color(if *ok {
                        p.muted
                    } else {
                        p.red
                    }));
                }
                if self.task.is_some() {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [(ui.available_width() - 98.0).max(160.0), 32.0],
                            egui::TextEdit::singleline(&mut self.task_input)
                                .hint_text("Send input to the running command"),
                        );
                        if secondary_button(ui, "Send input").clicked() {
                            self.send_task_input();
                        }
                    });
                }
            });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Output").strong().size(10.0).color(p.paper));
            if self.task.is_some() {
                ui.spinner();
                ui.label(RichText::new(&self.task_name).size(10.0).color(p.mint));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.task_output.is_empty() && quiet_button(ui, "Clear").clicked() {
                    self.task_output.clear();
                    self.last_run = None;
                }
                if let Some(run) = &self.last_run {
                    ui.label(
                        RichText::new(if run.ok { "Finished" } else { "Failed" })
                            .size(9.0)
                            .color(if run.ok { p.mint } else { p.red }),
                    );
                }
            });
        });
        let output_body_height = (output_height - 34.0).max(100.0);
        egui::Frame::new()
            .fill(if ui.visuals().dark_mode {
                Color32::from_rgb(16, 17, 16)
            } else {
                Color32::from_rgb(251, 251, 248)
            })
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_height((output_body_height - 20.0).max(80.0));
                let output = if self.task_output.is_empty() {
                    "Results and messages will appear here."
                } else {
                    &self.task_output
                };
                egui::ScrollArea::vertical()
                    .id_salt("command-output")
                    .stick_to_bottom(self.task.is_some())
                    .auto_shrink([false, false])
                    .max_height((output_body_height - 20.0).max(80.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(output).monospace().size(10.0).color(p.paper),
                            )
                            .selectable(true)
                            .wrap(),
                        );
                    });
            });
    }

    fn command_tools_page(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let mut install_requested = false;
        let previous_theme = self.theme;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.selectable_value(&mut self.theme, ThemeMode::Light, "Light");
            ui.selectable_value(&mut self.theme, ThemeMode::Dark, "Dark");
            ui.label(RichText::new("Theme").size(9.0).color(p.faint));
        });
        if self.theme != previous_theme {
            configure_style(ui.ctx(), self.theme);
        }

        ui.add_space((ui.available_height() * 0.1).clamp(24.0, 86.0));
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Synaptic").strong().size(17.0).color(p.paper));
            ui.add_space(18.0);
            ui.heading(
                RichText::new("Getting Synaptic ready")
                    .size(30.0)
                    .color(p.paper),
            );
            ui.label(
                RichText::new(
                    "The desktop app is installed. It also needs the command tools that build graphs and connect assistants.",
                )
                .size(12.5)
                .color(p.muted),
            );
            ui.add_space(24.0);

            egui::Frame::new()
                .fill(p.panel)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(4)
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(560.0));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Desktop app").size(11.5).color(p.paper));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new("Ready").size(10.0).color(p.muted));
                        });
                    });
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Command tools").size(11.5).color(p.paper));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(match self.command_tools {
                                    CommandToolsStatus::Installing => "Installing",
                                    CommandToolsStatus::Error(_) => "Needs attention",
                                    CommandToolsStatus::Missing => "Not installed",
                                    CommandToolsStatus::Ready => "Ready",
                                })
                                .size(10.0)
                                .color(match self.command_tools {
                                    CommandToolsStatus::Error(_) => p.red,
                                    _ => p.muted,
                                }),
                            );
                        });
                    });
                    ui.add_space(18.0);

                    match &self.command_tools {
                        CommandToolsStatus::Installing => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    RichText::new("Downloading and verifying command tools…")
                                        .size(11.0)
                                        .color(p.muted),
                                );
                            });
                            ui.label(
                                RichText::new("This usually takes less than a minute.")
                                    .size(9.5)
                                    .color(p.faint),
                            );
                        }
                        CommandToolsStatus::Missing => {
                            if ui
                                .add(primary_button(p, "Download command tools"))
                                .clicked()
                            {
                                install_requested = true;
                            }
                        }
                        CommandToolsStatus::Error(error) => {
                            ui.label(
                                RichText::new("Setup didn't finish.")
                                    .strong()
                                    .size(11.5)
                                    .color(p.paper),
                            );
                            ui.label(
                                RichText::new(
                                    "Check your internet connection and try again. If it still fails, move the app to a folder you can write to.",
                                )
                                .size(10.5)
                                .color(p.muted),
                            );
                            ui.add_space(10.0);
                            if ui.add(primary_button(p, "Try again")).clicked() {
                                install_requested = true;
                            }
                            egui::CollapsingHeader::new(
                                RichText::new("Error details").size(9.5).color(p.faint),
                            )
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(error)
                                        .monospace()
                                        .size(9.0)
                                        .color(p.red),
                                );
                            });
                        }
                        CommandToolsStatus::Ready => {}
                    }
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Downloaded from the official GitHub Release. Published checksums are verified before anything is installed.",
                        )
                        .size(9.5)
                        .color(p.faint),
                    );
                });
        });
        if install_requested {
            self.install_command_tools();
        }
    }

    fn app_page(&mut self, ui: &mut egui::Ui) {
        ui.add_space(18.0);
        if ui.available_width() >= 820.0 {
            ui.columns(2, |columns| {
                self.update_section(&mut columns[0]);
                self.installation_section(&mut columns[1]);
            });
        } else {
            self.update_section(ui);
            ui.add_space(14.0);
            self.installation_section(ui);
        }
    }

    fn update_section(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let mut check = false;
        let mut install = false;
        let mut restart = false;
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.heading(RichText::new("Updates").size(20.0).color(p.paper));
                ui.label(
                    RichText::new(format!(
                        "Installed version: {}",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .size(11.0)
                    .color(p.muted),
                );
                ui.add_space(12.0);
                match &self.lifecycle {
                    LifecycleStatus::Idle => {
                        ui.label(
                            RichText::new("Check GitHub for a newer desktop build.")
                                .size(11.5)
                                .color(p.muted),
                        );
                        ui.add_space(10.0);
                        if ui.add(primary_button(p, "Check for updates")).clicked() {
                            check = true;
                        }
                    }
                    LifecycleStatus::Checking => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Checking GitHub…")
                                    .size(11.5)
                                    .color(p.muted),
                            );
                        });
                    }
                    LifecycleStatus::Available(release) => {
                        ui.label(
                            RichText::new(format!(
                                "Version {} is available.",
                                release.version.trim_start_matches('v')
                            ))
                            .strong()
                            .size(13.0)
                            .color(p.paper),
                        );
                        if !release.notes.trim().is_empty() {
                            ui.add_space(8.0);
                            ui.label(RichText::new("What changed").size(10.0).color(p.faint));
                            egui::ScrollArea::vertical()
                                .id_salt("release-notes")
                                .max_height(150.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(release.notes.trim())
                                                .size(10.5)
                                                .color(p.muted),
                                        )
                                        .wrap(),
                                    );
                                });
                        }
                        ui.add_space(10.0);
                        if ui
                            .add(primary_button(p, "Download and install"))
                            .clicked()
                        {
                            install = true;
                        }
                        if quiet_button(ui, "Check again").clicked() {
                            check = true;
                        }
                    }
                    LifecycleStatus::Installing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Downloading, verifying, and installing…")
                                    .size(11.5)
                                    .color(p.muted),
                            );
                        });
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Keep Synaptic open until this finishes.")
                                .size(10.0)
                                .color(p.faint),
                        );
                    }
                    LifecycleStatus::Current => {
                        ui.label(
                            RichText::new("This is the latest version.")
                                .strong()
                                .size(12.0)
                                .color(p.paper),
                        );
                        ui.add_space(10.0);
                        if secondary_button(ui, "Check again").clicked() {
                            check = true;
                        }
                    }
                    LifecycleStatus::Restart(version) => {
                        ui.label(
                            RichText::new(format!("Version {version} is installed."))
                                .strong()
                                .size(12.0)
                                .color(p.paper),
                        );
                        ui.label(
                            RichText::new("Restart Synaptic to finish.")
                                .size(10.5)
                                .color(p.muted),
                        );
                        ui.add_space(10.0);
                        if ui.add(primary_button(p, "Restart now")).clicked() {
                            restart = true;
                        }
                    }
                    LifecycleStatus::Error(error) => {
                        ui.label(RichText::new(error).size(10.5).color(p.red));
                        ui.add_space(10.0);
                        if secondary_button(ui, "Try again").clicked() {
                            check = true;
                        }
                    }
                }
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Downloads come from the GitHub Release published by the release workflow. Published checksums are verified before installation.",
                    )
                    .size(9.5)
                    .color(p.faint),
                );
            });
        if check {
            self.check_for_updates();
        } else if install {
            self.install_update();
        } else if restart {
            self.restart(ui.ctx());
        }
    }

    fn installation_section(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let mut remove = false;
        let mut register = false;
        let mut open = false;
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(4)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.heading(RichText::new("Desktop app").size(20.0).color(p.paper));
                match &self.app_install {
                    AppInstallStatus::Portable => {
                        ui.label(
                            RichText::new(format!(
                                "Add Synaptic to {} so it is easy to find after closing it.",
                                application_menu_name()
                            ))
                            .size(11.5)
                            .color(p.muted),
                        );
                        ui.add_space(12.0);
                        if ui.add(primary_button(p, "Add to applications")).clicked() {
                            register = true;
                        }
                        ui.label(
                            RichText::new("Installs only for your account. No administrator access is needed.")
                                .size(9.5)
                                .color(p.faint),
                        );
                    }
                    AppInstallStatus::Installing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Adding Synaptic to your applications…")
                                    .size(11.5)
                                    .color(p.muted),
                            );
                        });
                    }
                    AppInstallStatus::Installed(path) => {
                        ui.label(
                            RichText::new(format!("Available in {}.", application_menu_name()))
                                .strong()
                                .size(12.0)
                                .color(p.paper),
                        );
                        ui.label(
                            RichText::new(path.display().to_string())
                                .monospace()
                                .size(9.5)
                                .color(p.faint),
                        );
                        ui.add_space(10.0);
                        if secondary_button(ui, "Open installed app").clicked() {
                            open = true;
                        }
                    }
                    AppInstallStatus::Error(error) => {
                        ui.label(
                            RichText::new("Synaptic could not be added to your applications.")
                                .size(11.5)
                                .color(p.red),
                        );
                        ui.add_space(8.0);
                        if secondary_button(ui, "Try again").clicked() {
                            register = true;
                        }
                        egui::CollapsingHeader::new("Error details").show(ui, |ui| {
                            ui.label(RichText::new(error).monospace().size(9.0).color(p.red));
                        });
                    }
                }

                ui.add_space(18.0);
                ui.separator();
                ui.add_space(14.0);
                ui.label(RichText::new("Remove Synaptic").strong().size(12.0).color(p.paper));
                ui.label(
                    RichText::new(
                        "Removes the desktop app and its application entry. Projects, graphs, settings, and separately installed CLI tools stay in place.",
                    )
                    .size(10.5)
                    .color(p.muted),
                );
                ui.add_space(12.0);
                if self.uninstall_confirm {
                    egui::Frame::new()
                        .fill(p.bg)
                        .stroke(Stroke::new(1.0, p.red.gamma_multiply(0.7)))
                        .corner_radius(4)
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("Remove Synaptic Desktop from this computer?")
                                    .strong()
                                    .size(11.5)
                                    .color(p.paper),
                            );
                            ui.label(
                                RichText::new("The app will close when removal begins.")
                                    .size(10.0)
                                    .color(p.muted),
                            );
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                if ui.add(danger_button(p, "Remove app")).clicked() {
                                    remove = true;
                                }
                                if secondary_button(ui, "Cancel").clicked() {
                                    self.uninstall_confirm = false;
                                }
                            });
                        });
                } else if secondary_button(ui, "Uninstall…").clicked() {
                    self.uninstall_confirm = true;
                }
            });
        if remove {
            self.remove_app(ui.ctx());
        } else if register {
            self.register_app();
        } else if open {
            self.open_registered_app(ui.ctx());
        }
    }

    fn right_console(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let federated = self.mode == SetupMode::Federated;
        egui::Panel::right("federation-console")
            .exact_size(342.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(22, 24)),
            )
            .show(ui, |ui| {
                ui.label(
                    RichText::new(if federated {
                        "FEDERATION CONSOLE"
                    } else {
                        "REPOSITORY CONSOLE"
                    })
                    .monospace()
                    .size(10.0)
                    .color(p.mint),
                );
                ui.add_space(5.0);
                ui.label(
                    RichText::new(if federated {
                        "One graph across every source."
                    } else {
                        "One repository, one graph."
                    })
                    .strong()
                    .size(19.0)
                    .color(p.paper),
                );
                ui.label(
                    RichText::new(if federated {
                        "Your selection and next actions stay visible here."
                    } else {
                        "Build this root directly, then connect your assistant."
                    })
                    .size(11.0)
                    .color(p.muted),
                );
                ui.add_space(12.0);
                federation_map(
                    ui,
                    self.graph_source_count(),
                    self.graph_exists(),
                    self.mcp_connected,
                );
                ui.horizontal(|ui| {
                    metric(
                        ui,
                        &self.graph_source_count().to_string(),
                        if federated { "selected" } else { "repository" },
                    );
                    ui.separator();
                    metric(
                        ui,
                        if self.graph_exists() {
                            "ready"
                        } else {
                            "not built"
                        },
                        "graph",
                    );
                    ui.separator();
                    metric(
                        ui,
                        if self.mcp_connected {
                            "linked"
                        } else {
                            "pending"
                        },
                        "MCP",
                    );
                });
                if federated {
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new("WORKSPACE NAME")
                            .monospace()
                            .size(9.0)
                            .color(p.muted),
                    );
                    ui.add_sized(
                        [ui.available_width(), 35.0],
                        egui::TextEdit::singleline(&mut self.workspace_name),
                    );
                }
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        self.can_build(),
                        primary_button(
                            p,
                            if federated {
                                "Build federation"
                            } else {
                                "Build repository"
                            },
                        )
                        .min_size(Vec2::new(ui.available_width(), 40.0)),
                    )
                    .clicked()
                {
                    self.build();
                }
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(14.0);
                ui.label(
                    RichText::new("CONNECT MCP")
                        .monospace()
                        .size(9.0)
                        .color(p.muted),
                );
                egui::ComboBox::from_id_salt("console-host")
                    .width(ui.available_width())
                    .selected_text(HOSTS[self.host].label)
                    .show_ui(ui, |ui| {
                        for (index, host) in HOSTS.iter().enumerate() {
                            ui.selectable_value(&mut self.host, index, host.label);
                        }
                    });
                ui.add_space(7.0);
                if ui
                    .add_enabled(
                        self.task.is_none(),
                        secondary_button_widget(p, "Install & connect")
                            .min_size(Vec2::new(ui.available_width(), 38.0)),
                    )
                    .clicked()
                {
                    self.install();
                }
                ui.add_space(14.0);
                self.activity(ui);
            });
    }

    fn bottom_actions(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let federated = self.mode == SetupMode::Federated;
        egui::Panel::bottom("actions")
            .exact_size(72.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(if federated {
                                format!("{} groups selected", self.selected_count())
                            } else {
                                "Single repository".to_string()
                            })
                            .strong()
                            .size(13.0)
                            .color(p.paper),
                        );
                        if self.task.is_some() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new(&self.task_name).size(10.0).color(p.mint));
                            });
                        } else if let Some((ok, message)) = &self.notice {
                            ui.label(RichText::new(message).size(9.5).color(if *ok {
                                p.muted
                            } else {
                                p.red
                            }));
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(self.task.is_none(), primary_button(p, "Connect"))
                            .clicked()
                        {
                            self.install();
                        }
                        egui::ComboBox::from_id_salt("action-host")
                            .width(150.0)
                            .selected_text(HOSTS[self.host].label)
                            .show_ui(ui, |ui| {
                                for (index, host) in HOSTS.iter().enumerate() {
                                    ui.selectable_value(&mut self.host, index, host.label);
                                }
                            });
                        if ui
                            .add_enabled(
                                self.can_build(),
                                secondary_button_widget(
                                    p,
                                    if federated {
                                        "Build federation"
                                    } else {
                                        "Build repository"
                                    },
                                ),
                            )
                            .clicked()
                        {
                            self.build();
                        }
                    });
                });
            });
    }

    fn activity(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        if self.task.is_some() {
            egui::Frame::new()
                .fill(p.panel_active)
                .stroke(Stroke::new(1.0, p.mint.gamma_multiply(0.45)))
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(&self.task_name).color(p.paper));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if quiet_button(ui, "Stop").clicked() {
                                self.stop_task();
                            }
                            ui.label(
                                RichText::new("RUNNING")
                                    .monospace()
                                    .size(10.0)
                                    .color(p.mint),
                            );
                        });
                    });
                });
        }
        if let Some((ok, message)) = &self.notice {
            ui.add_space(8.0);
            ui.label(
                RichText::new(message)
                    .size(12.0)
                    .color(if *ok { p.mint } else { p.red }),
            );
        }
        let output = if self.task.is_some() {
            Some(("Live command output", self.task_output.as_str(), true))
        } else {
            self.last_run
                .as_ref()
                .map(|run| ("Command output", run.output.as_str(), false))
        };
        if let Some((title, output, live)) = output {
            ui.add_space(8.0);
            egui::CollapsingHeader::new(RichText::new(title).monospace().size(11.0).color(p.muted))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(live)
                        .max_height(130.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(output).monospace().size(10.5).color(p.paper),
                                )
                                .selectable(true)
                                .wrap(),
                            );
                        });
                });
        }
    }
}

impl eframe::App for SynapticUi {
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if !self.theme_initialized {
            self.theme = ThemeMode::from_system(input.system_theme);
            self.theme_initialized = true;
            configure_style(ctx, self.theme);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(THEME_STORAGE_KEY, self.theme.storage_value().into());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_task();
        self.poll_lifecycle();
        if self.task.is_some() || self.lifecycle_events.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if !matches!(self.command_tools, CommandToolsStatus::Ready) {
            let p = palette(ui);
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(p.bg)
                        .inner_margin(egui::Margin::symmetric(22, 22)),
                )
                .show(ui, |ui| self.command_tools_page(ui));
            return;
        }
        let dropped = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .next()
        });
        if let Some(path) = dropped {
            let path = if path.is_dir() {
                path
            } else {
                path.parent().unwrap_or(&path).to_path_buf()
            };
            self.root = path.display().to_string();
            self.scan_root = path.parent().unwrap_or(&path).display().to_string();
            self.discover();
        }

        let p = palette(ui);
        let window_width = ui.available_width();
        let show_rail = window_width >= 900.0;
        let show_console = self.view == AppView::Setup && window_width >= 1440.0;
        if show_rail {
            self.side_rail(ui);
        }
        if self.view == AppView::Setup {
            if show_console {
                self.right_console(ui);
            } else {
                self.bottom_actions(ui);
            }
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(if show_rail { 26 } else { 20 }, 22)),
            )
            .show(ui, |ui| {
                if !show_rail {
                    self.compact_progress(ui);
                }
                self.header(
                    ui,
                    self.view == AppView::Setup && !show_console && ui.available_width() >= 900.0,
                );
                match self.view {
                    AppView::Setup => {
                        ui.add_space(12.0);
                        self.mode_switcher(ui);
                        ui.add_space(14.0);
                        self.workspace_toolbar(ui);
                        ui.add_space(14.0);
                        self.source_browser(ui);
                    }
                    AppView::Commands if ui.available_width() < 760.0 => {
                        egui::ScrollArea::vertical()
                            .id_salt("tools-page")
                            .show(ui, |ui| self.command_center(ui));
                    }
                    AppView::Commands => self.command_center(ui),
                    AppView::App => self.app_page(ui),
                }
            });
    }
}

fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

fn configure_style(ctx: &egui::Context, theme: ThemeMode) {
    let egui_theme = if theme == ThemeMode::Dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let p = if theme == ThemeMode::Dark {
        DARK
    } else {
        LIGHT
    };
    let mut style = (*ctx.style_of(egui_theme)).clone();
    style.spacing.item_spacing = Vec2::new(9.0, 8.0);
    style.spacing.button_padding = Vec2::new(13.0, 8.0);
    style.spacing.interact_size.y = 38.0;
    style.visuals.dark_mode = theme == ThemeMode::Dark;
    style.visuals.panel_fill = p.bg;
    style.visuals.window_fill = p.panel;
    style.visuals.extreme_bg_color = if theme == ThemeMode::Dark {
        Color32::from_rgb(16, 17, 16)
    } else {
        Color32::from_rgb(251, 251, 248)
    };
    style.visuals.faint_bg_color = p.panel;
    style.visuals.selection.bg_fill = p.mint.gamma_multiply(0.18);
    style.visuals.selection.stroke = Stroke::new(1.0, p.mint);
    style.visuals.widgets.inactive.bg_fill = p.panel;
    style.visuals.widgets.inactive.weak_bg_fill = p.panel;
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.muted);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    style.visuals.widgets.hovered.bg_fill = p.panel_active;
    style.visuals.widgets.hovered.weak_bg_fill = p.panel_active;
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, p.paper);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
    style.visuals.widgets.active.bg_fill = p.panel_active;
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, p.paper);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.mint);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.muted);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    style.visuals.widgets.inactive.corner_radius = 4.into();
    style.visuals.widgets.hovered.corner_radius = 4.into();
    style.visuals.widgets.active.corner_radius = 4.into();
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(31.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(12.0, FontFamily::Proportional),
    );
    ctx.set_style_of(egui_theme, style);
    ctx.set_theme(egui_theme);
}

fn candidate_visible(candidate: &Candidate, filter: CandidateFilter, query: &str) -> bool {
    let in_filter = match filter {
        CandidateFilter::All => true,
        CandidateFilter::Selected => candidate.selected,
        CandidateFilter::Workspace => candidate.is_member(),
        CandidateFilter::Nearby => !candidate.is_member(),
    };
    let query = query.trim().to_lowercase();
    in_filter
        && (query.is_empty()
            || candidate.name.to_lowercase().contains(&query)
            || candidate.location.to_lowercase().contains(&query))
}

fn command_pane_heights(height: f32) -> (f32, f32) {
    let output = (height * 0.32)
        .clamp(150.0, 260.0)
        .min((height - 190.0).max(150.0));
    ((height - output - 10.0).max(180.0), output)
}

fn tool_visible(tool: &ToolSpec, group: &str, query: &str) -> bool {
    (group == "All" || tool.group == group)
        && (query.is_empty()
            || tool.name.contains(query)
            || tool_label(tool.name).to_lowercase().contains(query)
            || tool.group.to_lowercase().contains(query)
            || group_label(tool.group).to_lowercase().contains(query)
            || tool.summary.to_lowercase().contains(query))
}

fn tool_row(ui: &mut egui::Ui, tool: &ToolSpec, selected: bool) -> egui::Response {
    let p = palette(ui);
    let title = tool_label(tool.name);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    let fill = if selected {
        p.panel_active
    } else if response.hovered() {
        p.bg
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 3.0, fill);
    if selected {
        ui.painter().line_segment(
            [rect.left_top(), rect.left_bottom()],
            Stroke::new(3.0, p.mint),
        );
    }
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + 10.0, rect.bottom()),
            rect.right_bottom(),
        ],
        Stroke::new(1.0, p.border.gamma_multiply(0.55)),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.top() + 8.0),
        egui::Align2::LEFT_TOP,
        &title,
        FontId::proportional(11.5),
        p.paper,
    );
    ui.painter().text(
        egui::pos2(rect.right() - 8.0, rect.top() + 9.0),
        egui::Align2::RIGHT_TOP,
        group_label(tool.group),
        FontId::proportional(8.0),
        p.faint,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        tool.summary,
        FontId::proportional(9.5),
        p.muted,
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            selected,
            format!("{title}: {}", tool.summary),
        )
    });
    response.on_hover_text(tool.summary)
}

fn guide_choice_row(ui: &mut egui::Ui, choice: &GuideChoice) -> egui::Response {
    let p = palette(ui);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        3.0,
        if response.hovered() {
            p.panel_active
        } else {
            p.panel
        },
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + 10.0, rect.bottom()),
            rect.right_bottom(),
        ],
        Stroke::new(1.0, p.border),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.top() + 8.0),
        egui::Align2::LEFT_TOP,
        friendly_label(&choice.name),
        FontId::proportional(12.0),
        p.paper,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        &choice.summary,
        FontId::proportional(9.5),
        p.muted,
    );
    ui.painter().text(
        egui::pos2(rect.right() - 10.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        "›",
        FontId::proportional(18.0),
        p.muted,
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            format!("{}: {}", choice.name, choice.summary),
        )
    });
    response
}

fn guide_argument_ui(ui: &mut egui::Ui, argument: &mut GuideArgument) {
    let p = palette(ui);
    match argument.kind {
        GuideArgKind::Flag => {
            ui.checkbox(&mut argument.enabled, &argument.label)
                .on_hover_text(&argument.help);
            ui.label(RichText::new(&argument.help).size(9.5).color(p.muted));
        }
        GuideArgKind::Positional | GuideArgKind::Value => {
            ui.horizontal(|ui| {
                if argument.kind == GuideArgKind::Value && !argument.required {
                    ui.checkbox(&mut argument.enabled, "");
                }
                ui.label(
                    RichText::new(&argument.label)
                        .strong()
                        .size(11.0)
                        .color(p.paper),
                );
                if argument.required {
                    ui.label(RichText::new("Required").size(8.0).color(p.muted));
                } else if let Some(default) = &argument.default {
                    ui.label(
                        RichText::new(format!("Default: {default}"))
                            .size(8.0)
                            .color(p.faint),
                    );
                }
            });
            let active =
                argument.kind == GuideArgKind::Positional || argument.required || argument.enabled;
            if active && argument.value.is_empty() && !argument.choices.is_empty() {
                argument.value = argument.choices[0].clone();
            }
            if argument.choices.is_empty() {
                ui.add_enabled(
                    active,
                    egui::TextEdit::singleline(&mut argument.value)
                        .font(egui::TextStyle::Monospace)
                        .password(argument.name.contains("password"))
                        .hint_text(if argument.multiple {
                            "Separate multiple values with spaces".to_string()
                        } else {
                            format!("Enter {}", argument.label.to_lowercase())
                        }),
                );
            } else {
                ui.add_enabled_ui(active, |ui| {
                    egui::ComboBox::from_id_salt(("guide-choice", &argument.name))
                        .width(ui.available_width())
                        .selected_text(if argument.value.is_empty() {
                            format!("Choose {}", argument.label.to_lowercase())
                        } else {
                            argument.value.clone()
                        })
                        .show_ui(ui, |ui| {
                            for choice in &argument.choices {
                                ui.selectable_value(&mut argument.value, choice.clone(), choice);
                            }
                        });
                });
            }
            if !argument.help.is_empty() {
                ui.label(RichText::new(&argument.help).size(9.5).color(p.muted));
            }
        }
    }
    ui.add_space(6.0);
}

fn candidate_row(ui: &mut egui::Ui, candidate: &mut Candidate, selectable: bool) {
    let p = palette(ui);
    let active = selectable && candidate.selected;
    egui::Frame::new()
        .fill(if active { p.panel_active } else { p.panel })
        .stroke(Stroke::new(
            1.0,
            if active {
                p.mint.gamma_multiply(0.45)
            } else {
                p.border
            },
        ))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(11, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if selectable {
                    ui.checkbox(
                        &mut candidate.selected,
                        RichText::new(&candidate.name)
                            .strong()
                            .size(12.5)
                            .color(p.paper),
                    );
                } else {
                    ui.label(
                        RichText::new(&candidate.name)
                            .strong()
                            .size(12.5)
                            .color(p.paper),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let source = if selectable {
                        if candidate.is_member() {
                            "LOCAL"
                        } else {
                            "NEARBY"
                        }
                    } else {
                        "PACKAGE"
                    };
                    ui.label(
                        RichText::new(source)
                            .monospace()
                            .size(8.5)
                            .color(if active { p.mint } else { p.faint }),
                    );
                    if let Some(coordinate) = &candidate.coordinate {
                        ui.label(
                            RichText::new(coordinate_text(coordinate))
                                .monospace()
                                .size(8.5)
                                .color(p.muted),
                        );
                    }
                });
            });
            ui.add(
                egui::Label::new(
                    RichText::new(&candidate.location)
                        .monospace()
                        .size(9.5)
                        .color(p.muted),
                )
                .truncate(),
            );
        });
    ui.add_space(6.0);
}

fn metric(ui: &mut egui::Ui, value: &str, label: &str) {
    let p = palette(ui);
    ui.vertical(|ui| {
        ui.label(RichText::new(value).strong().size(15.0).color(p.paper));
        ui.label(RichText::new(label).size(9.5).color(p.muted));
    });
}

fn step(ui: &mut egui::Ui, number: &str, label: &str, done: bool) {
    let p = palette(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
        ui.painter().circle_stroke(
            rect.center(),
            14.0,
            Stroke::new(1.0, if done { p.mint } else { p.faint }),
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            number,
            FontId::monospace(9.5),
            if done { p.mint } else { p.muted },
        );
        ui.label(
            RichText::new(label)
                .size(12.0)
                .color(if done { p.paper } else { p.muted }),
        );
    });
}

fn rail_line(ui: &mut egui::Ui, active: bool) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(30.0, 34.0), Sense::hover());
    ui.painter().line_segment(
        [
            egui::pos2(rect.center().x, rect.top()),
            egui::pos2(rect.center().x, rect.bottom()),
        ],
        Stroke::new(
            1.0,
            if active {
                p.mint.gamma_multiply(0.6)
            } else {
                p.faint
            },
        ),
    );
}

fn graph_mark(ui: &mut egui::Ui) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(205.0, 84.0), Sense::hover());
    let points = [
        egui::pos2(rect.left() + 12.0, rect.center().y - 23.0),
        egui::pos2(rect.left() + 12.0, rect.center().y + 22.0),
        egui::pos2(rect.left() + 74.0, rect.center().y),
        egui::pos2(rect.left() + 134.0, rect.center().y),
        egui::pos2(rect.right() - 12.0, rect.center().y - 19.0),
        egui::pos2(rect.right() - 12.0, rect.center().y + 19.0),
    ];
    let painter = ui.painter();
    for (a, b) in [(0, 2), (1, 2), (2, 3), (3, 4), (3, 5)] {
        painter.line_segment([points[a], points[b]], Stroke::new(1.0, p.faint));
    }
    for (index, point) in points.iter().enumerate() {
        painter.circle_filled(*point, if index == 3 { 6.0 } else { 4.0 }, p.mint);
        painter.circle_stroke(*point, 9.0, Stroke::new(1.0, p.mint.gamma_multiply(0.25)));
    }
}

fn federation_map(ui: &mut egui::Ui, selected: usize, graph_ready: bool, connected: bool) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 108.0), Sense::hover());
    let points = [
        egui::pos2(rect.left() + 18.0, rect.top() + 25.0),
        egui::pos2(rect.left() + 18.0, rect.center().y),
        egui::pos2(rect.left() + 18.0, rect.bottom() - 25.0),
        egui::pos2(rect.center().x - 20.0, rect.center().y),
        egui::pos2(rect.center().x + 50.0, rect.center().y),
        egui::pos2(rect.right() - 18.0, rect.center().y),
    ];
    let painter = ui.painter();
    let sources: &[usize] = match selected {
        1 => &[1],
        2 => &[0, 2],
        _ => &[0, 1, 2],
    };
    for &source in sources {
        painter.line_segment([points[source], points[3]], Stroke::new(1.0, p.faint));
    }
    painter.line_segment(
        [points[3], points[4]],
        Stroke::new(1.0, if graph_ready { p.mint } else { p.faint }),
    );
    painter.line_segment(
        [points[4], points[5]],
        Stroke::new(1.0, if connected { p.mint } else { p.faint }),
    );
    for &source in sources {
        painter.circle_filled(
            points[source],
            4.0,
            if selected > 0 { p.mint } else { p.faint },
        );
    }
    painter.circle_filled(points[3], 7.0, if selected > 0 { p.mint } else { p.faint });
    painter.circle_stroke(
        points[3],
        11.0,
        Stroke::new(1.0, p.mint.gamma_multiply(0.25)),
    );
    painter.circle_filled(points[4], 6.0, if graph_ready { p.mint } else { p.faint });
    painter.circle_filled(points[5], 6.0, if connected { p.mint } else { p.faint });
    painter.text(
        egui::pos2(points[3].x, rect.bottom() - 6.0),
        egui::Align2::CENTER_BOTTOM,
        format!("{selected} SOURCE{}", if selected == 1 { "" } else { "S" }),
        FontId::monospace(8.5),
        p.muted,
    );
}

fn primary_button(p: Palette, label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(p.bg))
        .fill(p.mint)
        .stroke(Stroke::NONE)
        .corner_radius(4)
        .min_size(Vec2::new(138.0, 40.0))
}

fn danger_button(p: Palette, label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(p.red))
        .fill(p.panel)
        .stroke(Stroke::new(1.0, p.red.gamma_multiply(0.7)))
        .corner_radius(4)
        .min_size(Vec2::new(110.0, 36.0))
}

fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let p = palette(ui);
    ui.add(secondary_button_widget(p, label))
}

fn secondary_button_widget(p: Palette, label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(p.paper))
        .fill(p.panel)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(4)
        .min_size(Vec2::new(96.0, 34.0))
}

fn quiet_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let p = palette(ui);
    ui.add(
        egui::Button::new(RichText::new(label).size(10.5).color(p.muted))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE),
    )
}

fn checked_dir(value: &str, label: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} cannot be empty."));
    }
    let path = PathBuf::from(value);
    if !path.is_dir() {
        return Err(format!("{label} is not a directory: {}", path.display()));
    }
    Ok(canonical(&path))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "workspace".into())
}

fn merge_members(members: &mut Vec<Member>, extra: Vec<Member>) {
    let mut paths: HashSet<PathBuf> = members
        .iter()
        .map(|member| canonical(&member.path))
        .collect();
    for member in extra {
        if paths.insert(canonical(&member.path)) {
            members.push(member);
        }
    }
}

fn repo_location(repo: &RepoMember) -> String {
    repo.path
        .as_deref()
        .or(repo.git.as_deref())
        .or(repo.subgraph.as_deref())
        .unwrap_or("declared repository")
        .to_string()
}

fn coordinate_text(coordinate: &Coordinate) -> String {
    let ecosystem = match coordinate.ecosystem {
        Ecosystem::Cargo => "cargo",
        Ecosystem::Npm => "npm",
        Ecosystem::Go => "go",
        Ecosystem::Python => "python",
        Ecosystem::Jvm => "jvm",
        Ecosystem::Gradle => "gradle",
        Ecosystem::DotNet => "dotnet",
        Ecosystem::Other => "package",
    };
    format!("{ecosystem} / {}", coordinate.name)
}

fn manifest_from_selection(
    name: &str,
    default_branch: &str,
    selected: Vec<&Candidate>,
) -> WorkspaceManifest {
    WorkspaceManifest {
        workspace: WorkspaceMeta {
            name: name.into(),
            default_branch: default_branch.into(),
            members: selected
                .iter()
                .filter_map(|candidate| match &candidate.source {
                    CandidateSource::Member(path) => Some(path.clone()),
                    CandidateSource::Repo(_) => None,
                })
                .collect(),
        },
        repos: selected
            .iter()
            .filter_map(|candidate| match &candidate.source {
                CandidateSource::Member(_) => None,
                CandidateSource::Repo(repo) => Some(repo.clone()),
            })
            .collect(),
    }
}

fn synaptic_binary() -> OsString {
    if let Some(path) = std::env::var_os("SYNAPTIC_BIN") {
        return path;
    }
    if let Ok(current) = std::env::current_exe() {
        let name = if cfg!(windows) {
            "synaptic.exe"
        } else {
            "synaptic"
        };
        let sibling = current.with_file_name(name);
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "synaptic".into()
}

fn synaptic_available() -> bool {
    let mut command = Command::new(synaptic_binary());
    command
        .arg("--version")
        .env("SYNAPTIC_UPDATE_CHECK", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command.status().is_ok_and(|status| status.success())
}

fn load_command_guide(path: &[String], root: &Path) -> Result<CommandGuide, String> {
    let binary = synaptic_binary();
    let mut command = Command::new(&binary);
    command.args(path).arg("--help");
    if root.is_dir() {
        command.current_dir(root);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|error| {
        format!(
            "Could not load guided controls from {}: {error}",
            binary.to_string_lossy()
        )
    })?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    if !output.status.success() {
        return Err(text.trim().to_string());
    }
    parse_command_guide(path, &text)
}

fn parse_command_guide(path: &[String], help: &str) -> Result<CommandGuide, String> {
    let lines: Vec<&str> = help.lines().collect();
    let usage = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix("Usage: "))
        .unwrap_or_default()
        .to_string();
    if usage.is_empty() {
        return Err("Synaptic returned help without a usage definition.".into());
    }
    let about = lines
        .iter()
        .map(|line| line.trim())
        .take_while(|line| !line.starts_with("Usage:"))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let subcommands = help_entries(&lines, "Commands")
        .into_iter()
        .filter(|(name, _)| name != "help")
        .map(|(name, summary)| GuideChoice { name, summary })
        .collect();
    let arguments = help_entries(&lines, "Arguments")
        .into_iter()
        .map(|(syntax, description)| positional_guide(&syntax, &description))
        .collect();
    let options = help_entries(&lines, "Options")
        .into_iter()
        .filter_map(|(syntax, description)| option_guide(&usage, &syntax, &description))
        .collect();
    Ok(CommandGuide {
        path: path.to_vec(),
        about,
        usage,
        subcommands,
        arguments,
        options,
    })
}

fn help_entries(lines: &[&str], heading: &str) -> Vec<(String, String)> {
    let marker = format!("{heading}:");
    let Some(start) = lines.iter().position(|line| line.trim() == marker) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in &lines[start + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            break;
        }
        let trimmed = line.trim_start();
        let is_entry = match heading {
            "Options" => trimmed.starts_with('-'),
            "Arguments" => trimmed.starts_with(['<', '[']),
            _ => line.len() - trimmed.len() <= 4,
        };
        if is_entry {
            entries.push(split_help_columns(trimmed));
        } else if let Some((_, description)) = entries.last_mut() {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str(trimmed);
        }
    }
    entries
}

fn split_help_columns(line: &str) -> (String, String) {
    let bytes = line.as_bytes();
    for index in 1..bytes.len().saturating_sub(1) {
        if bytes[index] == b' ' && bytes[index + 1] == b' ' {
            return (
                line[..index].trim().to_string(),
                line[index..].trim().to_string(),
            );
        }
    }
    (line.trim().to_string(), String::new())
}

fn positional_guide(syntax: &str, description: &str) -> GuideArgument {
    let required = syntax.starts_with('<');
    let multiple = syntax.contains("...");
    let name = syntax
        .trim_matches(['<', '>', '[', ']', '.'])
        .to_ascii_lowercase();
    let default = bracket_value(description, "[default: ");
    GuideArgument {
        label: friendly_label(&name),
        name,
        help: without_metadata(description),
        kind: GuideArgKind::Positional,
        required,
        multiple,
        value: default.clone().unwrap_or_default(),
        default,
        choices: syntax_choices(syntax, description),
        enabled: true,
    }
}

fn option_guide(usage: &str, syntax: &str, description: &str) -> Option<GuideArgument> {
    let name = syntax
        .split_whitespace()
        .find(|part| part.starts_with("--"))?
        .trim_end_matches(',')
        .to_string();
    if name == "--help" || name == "--version" {
        return None;
    }
    let kind = if syntax.contains('<') {
        GuideArgKind::Value
    } else {
        GuideArgKind::Flag
    };
    let required = usage
        .split_whitespace()
        .any(|part| part == name && !part.starts_with('['));
    let default = bracket_value(description, "[default: ");
    Some(GuideArgument {
        label: friendly_label(name.trim_start_matches('-')),
        name,
        help: without_metadata(description),
        kind,
        required,
        multiple: syntax.contains("..."),
        value: default.clone().unwrap_or_default(),
        default,
        choices: syntax_choices(syntax, description),
        enabled: required,
    })
}

fn bracket_value(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)? + prefix.len();
    let end = text[start..].find(']')? + start;
    Some(text[start..end].to_string())
}

fn without_metadata(text: &str) -> String {
    ["[default: ", "[possible values: "]
        .into_iter()
        .fold(text.to_string(), |mut value, prefix| {
            while let Some(start) = value.find(prefix) {
                let Some(end) = value[start..].find(']') else {
                    break;
                };
                value.replace_range(start..=start + end, "");
            }
            value.trim().to_string()
        })
}

fn syntax_choices(syntax: &str, description: &str) -> Vec<String> {
    let values = bracket_value(description, "[possible values: ").or_else(|| {
        let start = syntax.find('<')? + 1;
        let end = syntax[start..].find('>')? + start;
        syntax[start..end]
            .contains('|')
            .then(|| syntax[start..end].to_string())
    });
    values
        .map(|values| {
            values
                .split([',', '|'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn group_label(group: &str) -> &str {
    match group {
        "Audit" => "Check",
        "Integrate" => "Connect",
        "System" => "Maintain",
        _ => group,
    }
}

fn tool_label(name: &str) -> String {
    match name {
        "extract" => "Build graph",
        "update" => "Update graph",
        "watch" => "Watch repository",
        "workspace" => "Workspaces",
        "merge-graphs" => "Merge graphs",
        "migrate" => "Migrate graph storage",
        "cache" => "Extraction cache",
        "query" => "Search graph",
        "path" => "Find a path",
        "explain" => "Explain code",
        "affected" => "Find affected code",
        "references" => "Find references",
        "hazards" => "Find runtime risks",
        "search" => "Structural search",
        "export" => "Export graph",
        "prs" => "Review pull requests",
        "diff" => "Compare versions",
        "refactor" => "Plan a refactor",
        "predict" => "Predict impact",
        "contract" => "Change contracts",
        "speculate" => "Test a change",
        "eval" => "Evaluate predictions",
        "sql" => "Review SQL",
        "audit" => "Check project",
        "vuln" => "Vulnerabilities",
        "api" => "API dependencies",
        "ingest" => "Add external data",
        "serve" => "MCP server",
        "memory" => "Project memory",
        "global" => "Global graph",
        "install" => "Connect an assistant",
        "uninstall" => "Disconnect an assistant",
        "hook" => "Git hooks",
        "merge-driver" => "Merge driver",
        "skill" => "Assistant files",
        "self-update" => "Update Synaptic",
        _ => return friendly_label(name),
    }
    .into()
}

fn friendly_label(name: &str) -> String {
    let mut label = name.replace(['-', '_'], " ");
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    label
}

fn append_guide_value(
    args: &mut Vec<OsString>,
    argument: &GuideArgument,
    named: bool,
) -> Result<(), String> {
    match argument.kind {
        GuideArgKind::Flag => {
            if argument.enabled {
                args.push(argument.name.clone().into());
            }
        }
        GuideArgKind::Positional | GuideArgKind::Value => {
            let active = !named || argument.required || argument.enabled;
            if !active {
                return Ok(());
            }
            let value = argument.value.trim();
            if value.is_empty() {
                if argument.required || argument.enabled {
                    return Err(format!("Complete {} before running.", argument.label));
                }
                return Ok(());
            }
            if named {
                args.push(argument.name.clone().into());
            }
            if argument.multiple {
                args.extend(parse_command_line(value)?);
            } else {
                args.push(value.into());
            }
        }
    }
    Ok(())
}

fn build_command(mode: SetupMode) -> (&'static str, Vec<OsString>) {
    match mode {
        SetupMode::Single => (
            "Building repository graph",
            vec!["extract".into(), ".".into()],
        ),
        SetupMode::Federated => (
            "Building federated graph",
            vec!["workspace".into(), "build".into()],
        ),
    }
}

fn spawn_process(binary: OsString, root: PathBuf, args: Vec<OsString>) -> RunningTask {
    let (event_sender, events) = mpsc::sync_channel(128);
    let (controls, control_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut command = Command::new(&binary);
        command
            .args(&args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = event_sender.send(TaskEvent::Output(format!(
                    "Could not launch {}: {error}\n",
                    binary.to_string_lossy()
                )));
                let _ = event_sender.send(TaskEvent::Finished {
                    ok: false,
                    stopped: false,
                });
                return;
            }
        };
        let stdout = child
            .stdout
            .take()
            .map(|reader| stream_reader(reader, event_sender.clone()));
        let stderr = child
            .stderr
            .take()
            .map(|reader| stream_reader(reader, event_sender.clone()));
        let mut stdin = child.stdin.take();
        let mut stopped = false;
        let ok = loop {
            loop {
                match control_receiver.try_recv() {
                    Ok(TaskControl::Input(input)) => {
                        if let Some(writer) = &mut stdin
                            && writeln!(writer, "{input}")
                                .and_then(|_| writer.flush())
                                .is_err()
                        {
                            let _ = event_sender.send(TaskEvent::Output(
                                "Could not write to command input.\n".into(),
                            ));
                        }
                    }
                    Ok(TaskControl::Stop) => {
                        stopped = true;
                        stdin.take();
                        // ponytail: kill the direct CLI child; add process groups if nested tools prove orphan-prone.
                        let _ = child.kill();
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        stopped = true;
                        stdin.take();
                        let _ = child.kill();
                        break;
                    }
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) => std::thread::sleep(Duration::from_millis(30)),
                Err(error) => {
                    let _ = event_sender.send(TaskEvent::Output(format!(
                        "Could not read command status: {error}\n"
                    )));
                    let _ = child.kill();
                    break false;
                }
            }
        };
        if let Some(reader) = stdout {
            let _ = reader.join();
        }
        if let Some(reader) = stderr {
            let _ = reader.join();
        }
        let _ = event_sender.send(TaskEvent::Finished { ok, stopped });
    });
    RunningTask { events, controls }
}

fn stream_reader<R: Read + Send + 'static>(
    mut reader: R,
    sender: mpsc::SyncSender<TaskEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender
                        .send(TaskEvent::Output(
                            String::from_utf8_lossy(&buffer[..count]).into_owned(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(TaskEvent::Output(format!(
                        "Could not read command output: {error}\n"
                    )));
                    break;
                }
            }
        }
    })
}

fn parse_command_line(input: &str) -> Result<Vec<OsString>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match quote {
            Some(mark) if character == mark => quote = None,
            Some('\'') => current.push(character),
            Some('"') if character == '\\' => match chars.peek().copied() {
                Some('"' | '\\') => current.push(chars.next().unwrap_or_default()),
                _ => current.push(character),
            },
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current).into());
                    started = false;
                }
            }
            None if character == '\\' => match chars.peek().copied() {
                Some(next) if next.is_whitespace() || matches!(next, '\'' | '"' | '\\') => {
                    current.push(chars.next().unwrap_or_default());
                    started = true;
                }
                _ => {
                    current.push(character);
                    started = true;
                }
            },
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if let Some(mark) = quote {
        return Err(format!("Missing closing {mark} quote."));
    }
    if started {
        args.push(current.into());
    }
    Ok(args)
}

fn append_output(output: &mut String, text: &str) {
    const MAX_OUTPUT: usize = 2 * 1024 * 1024;
    output.push_str(text);
    if output.len() > MAX_OUTPUT {
        let mut cut = output.len() - MAX_OUTPUT + 32;
        while !output.is_char_boundary(cut) {
            cut += 1;
        }
        output.drain(..cut);
        output.insert_str(0, "[earlier output truncated]\n");
    }
}

fn command_text(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_desktop_binary(path: &Path) -> bool {
    path.file_stem().and_then(|stem| stem.to_str()) == Some("synaptic-ui")
}

fn required_path(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is not available"))
}

fn installed_app_path() -> Result<PathBuf, String> {
    #[cfg(windows)]
    return Ok(required_path("LOCALAPPDATA")?
        .join("Programs")
        .join("Synaptic")
        .join(target::binary_name("synaptic-ui")));

    #[cfg(target_os = "macos")]
    return Ok(required_path("HOME")?
        .join("Applications")
        .join("Synaptic.app")
        .join("Contents")
        .join("MacOS")
        .join("synaptic-ui"));

    #[cfg(all(unix, not(target_os = "macos")))]
    return Ok(required_path("HOME")?
        .join(".local")
        .join("lib")
        .join("synaptic")
        .join("synaptic-ui"));
}

fn launcher_path() -> Result<PathBuf, String> {
    #[cfg(windows)]
    return Ok(required_path("APPDATA")?
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Synaptic.lnk"));

    #[cfg(target_os = "macos")]
    return Ok(required_path("HOME")?
        .join("Applications")
        .join("Synaptic.app"));

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or(required_path("HOME")?.join(".local").join("share"));
        Ok(data.join("applications").join("synaptic.desktop"))
    }
}

fn application_menu_name() -> &'static str {
    #[cfg(windows)]
    return "Windows Start";
    #[cfg(target_os = "macos")]
    return "Applications";
    #[cfg(all(unix, not(target_os = "macos")))]
    return "your application menu";
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn registered_desktop_app() -> Option<PathBuf> {
    let installed = installed_app_path().ok()?;
    (installed.is_file() && launcher_path().ok()?.exists()).then_some(installed)
}

fn make_app_executable(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(_path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(_path, permissions).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn copy_release_binaries(current: &Path, installed: &Path) -> Result<(), String> {
    if !is_desktop_binary(current) {
        return Err(format!(
            "Refusing to install an unexpected executable: {}",
            current.display()
        ));
    }
    let source_dir = current
        .parent()
        .ok_or_else(|| "The app has no parent folder.".to_string())?;
    let destination = installed
        .parent()
        .ok_or_else(|| "The installation has no destination folder.".to_string())?;
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    if !same_path(current, installed) {
        std::fs::copy(current, installed).map_err(|error| error.to_string())?;
    }
    make_app_executable(installed)?;

    for name in ["synaptic", "syn"] {
        let source = source_dir.join(target::binary_name(name));
        if source.is_file() {
            let target = destination.join(target::binary_name(name));
            if !same_path(&source, &target) {
                std::fs::copy(&source, &target).map_err(|error| error.to_string())?;
            }
            make_app_executable(&target)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn register_launcher(installed: &Path, launcher: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Some(parent) = launcher.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$shell = New-Object -ComObject WScript.Shell; $shortcut = $shell.CreateShortcut($env:SYNAPTIC_SHORTCUT); $shortcut.TargetPath = $env:SYNAPTIC_UI; $shortcut.WorkingDirectory = $env:USERPROFILE; $shortcut.Description = 'Synaptic'; $shortcut.Save()",
        ])
        .env("SYNAPTIC_SHORTCUT", launcher)
        .env("SYNAPTIC_UI", installed)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "Windows could not create the Start menu shortcut.".into())
}

#[cfg(target_os = "macos")]
fn register_launcher(_installed: &Path, launcher: &Path) -> Result<(), String> {
    let plist = launcher.join("Contents").join("Info.plist");
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        plist,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleDisplayName</key><string>Synaptic</string><key>CFBundleExecutable</key><string>synaptic-ui</string><key>CFBundleIdentifier</key><string>com.synapticgraph.desktop</string><key>CFBundleName</key><string>Synaptic</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>{}</string></dict></plist>\n",
            env!("CARGO_PKG_VERSION")
        ),
    )
    .map_err(|error| error.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn register_launcher(installed: &Path, launcher: &Path) -> Result<(), String> {
    if let Some(parent) = launcher.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let executable = installed
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    std::fs::write(
        launcher,
        format!(
            "[Desktop Entry]\nType=Application\nName=Synaptic\nComment=Understand and maintain your codebase\nExec=\"{executable}\"\nIcon=applications-development\nTerminal=false\nCategories=Development;\nStartupNotify=true\n"
        ),
    )
    .map_err(|error| error.to_string())
}

fn register_desktop_app_at(
    current: &Path,
    installed: &Path,
    launcher: &Path,
) -> Result<PathBuf, String> {
    copy_release_binaries(current, installed)?;
    register_launcher(installed, launcher)?;
    Ok(installed.to_path_buf())
}

fn register_desktop_app() -> Result<PathBuf, String> {
    register_desktop_app_at(
        &std::env::current_exe().map_err(|error| error.to_string())?,
        &installed_app_path()?,
        &launcher_path()?,
    )
}

fn uninstall_desktop_app() -> Result<(), String> {
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    if !is_desktop_binary(&current) {
        return Err(format!(
            "Refusing to remove an unexpected executable: {}",
            current.display()
        ));
    }

    if let Some(installed) = registered_desktop_app() {
        let launcher = launcher_path()?;
        #[cfg(target_os = "macos")]
        let install_root = launcher.clone();
        #[cfg(not(target_os = "macos"))]
        let install_root = installed
            .parent()
            .ok_or_else(|| "The installation has no parent folder.".to_string())?
            .to_path_buf();

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            if same_path(&current, &installed) {
                return Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Remove-Item -LiteralPath $env:SYNAPTIC_SHORTCUT -Force -ErrorAction SilentlyContinue; for ($i = 0; $i -lt 40; $i++) { Start-Sleep -Milliseconds 250; Remove-Item -LiteralPath $env:SYNAPTIC_UI_DIR -Recurse -Force -ErrorAction SilentlyContinue; if (-not (Test-Path -LiteralPath $env:SYNAPTIC_UI_DIR)) { break } }",
                    ])
                    .env("SYNAPTIC_SHORTCUT", launcher)
                    .env("SYNAPTIC_UI_DIR", install_root)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .map(|_| ())
                    .map_err(|error| error.to_string());
            }
        }

        if launcher != install_root && launcher.is_file() {
            std::fs::remove_file(&launcher).map_err(|error| error.to_string())?;
        }
        return std::fs::remove_dir_all(&install_root).map_err(|error| error.to_string());
    }

    #[cfg(unix)]
    {
        std::fs::remove_file(&current).map_err(|error| error.to_string())
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "for ($i = 0; $i -lt 40; $i++) { Start-Sleep -Milliseconds 250; Remove-Item -LiteralPath $env:SYNAPTIC_UI_REMOVE -Force -ErrorAction SilentlyContinue; if (-not (Test-Path -LiteralPath $env:SYNAPTIC_UI_REMOVE)) { break } }",
            ])
            .env("SYNAPTIC_UI_REMOVE", &current)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Synaptic")
            .with_icon(egui::IconData::default())
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([820.0, 620.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "synaptic-ui",
        options,
        Box::new(|cc| Ok(Box::new(SynapticUi::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_uses_saved_choice_then_system_default() {
        assert_eq!(ThemeMode::from_storage("light"), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::from_storage("dark"), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::from_storage("unknown"), None);
        assert_eq!(
            ThemeMode::from_system(Some(egui::Theme::Light)),
            ThemeMode::Light
        );
        assert_eq!(ThemeMode::from_system(None), ThemeMode::Dark);
    }

    #[test]
    fn desktop_install_copies_the_bundle_and_creates_a_launcher() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "synaptic-ui-install-test-{}-{unique}",
            std::process::id()
        ));
        let source = base.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let current = source.join(target::binary_name("synaptic-ui"));
        std::fs::write(&current, b"desktop").unwrap();
        for name in ["synaptic", "syn"] {
            std::fs::write(source.join(target::binary_name(name)), b"cli").unwrap();
        }

        #[cfg(windows)]
        let (installed, launcher) = (
            base.join("install").join("synaptic-ui.exe"),
            base.join("Synaptic.lnk"),
        );
        #[cfg(target_os = "macos")]
        let (installed, launcher) = (
            base.join("Applications")
                .join("Synaptic.app")
                .join("Contents")
                .join("MacOS")
                .join("synaptic-ui"),
            base.join("Applications").join("Synaptic.app"),
        );
        #[cfg(all(unix, not(target_os = "macos")))]
        let (installed, launcher) = (
            base.join("install").join("synaptic-ui"),
            base.join("synaptic.desktop"),
        );

        register_desktop_app_at(&current, &installed, &launcher).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"desktop");
        assert_eq!(
            std::fs::read(installed.with_file_name(target::binary_name("synaptic"))).unwrap(),
            b"cli"
        );
        assert!(launcher.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn selection_becomes_the_standard_workspace_manifest() {
        let local = Candidate {
            name: "api".into(),
            location: "crates/api".into(),
            coordinate: None,
            selected: true,
            source: CandidateSource::Member("crates/api".into()),
        };
        let remote = Candidate {
            name: "web".into(),
            location: "../web".into(),
            coordinate: None,
            selected: true,
            source: CandidateSource::Repo(RepoMember {
                name: "web".into(),
                tag: None,
                coordinate: None,
                git: None,
                rev: None,
                subgraph: None,
                path: Some("../web".into()),
            }),
        };
        let manifest = manifest_from_selection("product", "main", vec![&local, &remote]);
        assert_eq!(manifest.workspace.members, ["crates/api"]);
        assert_eq!(manifest.repos[0].path.as_deref(), Some("../web"));
        assert!(candidate_visible(&local, CandidateFilter::Workspace, "API"));
        assert!(!candidate_visible(
            &remote,
            CandidateFilter::Selected,
            "missing"
        ));
    }

    #[test]
    fn setup_modes_use_their_native_build_commands() {
        assert_eq!(
            command_text(&build_command(SetupMode::Single).1),
            "extract ."
        );
        assert_eq!(
            command_text(&build_command(SetupMode::Federated).1),
            "workspace build"
        );
        assert_ne!(DARK.bg, LIGHT.bg);
        assert_ne!(DARK.mint, LIGHT.mint);
        assert!(is_desktop_binary(Path::new(&target::binary_name(
            "synaptic-ui"
        ))));
        assert!(!is_desktop_binary(Path::new(&target::binary_name(
            "synaptic"
        ))));
    }

    #[test]
    fn tool_catalog_filters_by_group_and_capability() {
        let query = CLI_TOOLS.iter().find(|tool| tool.name == "query").unwrap();
        let vuln = CLI_TOOLS.iter().find(|tool| tool.name == "vuln").unwrap();
        assert!(tool_visible(query, "All", "search graph"));
        assert!(tool_visible(query, "Explore", ""));
        assert!(!tool_visible(query, "Build", "query"));
        assert!(tool_visible(vuln, "All", "vulnerabilities"));
        assert_eq!(group_label("Audit"), "Check");
    }

    #[test]
    fn command_output_is_always_reserved_inside_the_editor() {
        for height in [360.0, 420.0, 800.0, 1200.0] {
            let (controls, output) = command_pane_heights(height);
            assert!(controls + output + 10.0 <= height);
            assert!((150.0..=260.0).contains(&output));
        }
    }

    #[test]
    fn guided_builder_parses_subcommands_options_and_required_values() {
        let vuln = parse_command_guide(
            &["vuln".into()],
            "Audit dependencies\n\nUsage: synaptic vuln <COMMAND>\n\nCommands:\n  scan  Scan the lockfile\n  help  Print help\n\nOptions:\n  -h, --help  Print help\n",
        )
        .unwrap();
        assert_eq!(vuln.subcommands[0].name, "scan");
        assert_eq!(vuln.subcommands.len(), 1);

        let mut accept = parse_command_guide(
            &["vuln".into(), "accept".into()],
            "Accept a finding\n\nUsage: synaptic vuln accept [OPTIONS] --reason <REASON> <FINDING>\n\nArguments:\n  <FINDING>  Finding id\n\nOptions:\n      --root <ROOT>      Repository root [default: .]\n      --reason <REASON>  Why the risk is acceptable\n      --json             Emit JSON\n  -h, --help             Print help\n",
        )
        .unwrap();
        accept.arguments[0].value = "VULN-42".into();
        let reason = accept
            .options
            .iter_mut()
            .find(|option| option.name == "--reason")
            .unwrap();
        assert!(reason.required);
        reason.value = "Temporary exception".into();
        let args: Vec<String> = accept
            .command_args()
            .unwrap()
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "vuln",
                "accept",
                "VULN-42",
                "--reason",
                "Temporary exception"
            ]
        );
    }

    #[test]
    #[ignore = "requires SYNAPTIC_BIN pointing at a built engine"]
    fn live_engine_guides_cover_every_nested_command() {
        let root = std::env::current_dir().unwrap();
        let mut pending: Vec<Vec<String>> = CLI_TOOLS
            .iter()
            .map(|tool| vec![tool.name.to_string()])
            .collect();
        let mut visited = 0;
        while let Some(path) = pending.pop() {
            let guide = load_command_guide(&path, &root)
                .unwrap_or_else(|error| panic!("{}: {error}", path.join(" ")));
            visited += 1;
            pending.extend(guide.subcommands.iter().map(|choice| {
                let mut child = path.clone();
                child.push(choice.name.clone());
                child
            }));
        }
        assert!(visited > CLI_TOOLS.len(), "no nested commands discovered");
    }

    #[test]
    fn command_arguments_are_cross_platform_and_never_shell_parsed() {
        let args = parse_command_line(
            r#"query "auth flow" --graph "C:\Work Graph\graph.json" ; echo $HOME"#,
        )
        .expect("valid command");
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "query",
                "auth flow",
                "--graph",
                r"C:\Work Graph\graph.json",
                ";",
                "echo",
                "$HOME"
            ]
        );
        assert!(parse_command_line("query 'missing").is_err());
    }

    #[test]
    fn command_catalog_matches_every_cli_variant() {
        let cli = include_str!("../../synaptic/src/cli.rs");
        let body = cli
            .split_once("pub(crate) enum Cmd {")
            .expect("Cmd enum")
            .1
            .split_once("pub(crate) enum ContractAction")
            .expect("next CLI enum")
            .0;
        let mut variants: Vec<String> = body
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("    ")?;
                if line.starts_with(' ') || !line.starts_with(char::is_uppercase) {
                    return None;
                }
                let name: String = line
                    .chars()
                    .take_while(|character| character.is_ascii_alphanumeric())
                    .collect();
                line[name.len()..]
                    .trim_start()
                    .starts_with('{')
                    .then(|| pascal_to_kebab(&name))
            })
            .collect();
        let mut catalog: Vec<String> = CLI_TOOLS
            .iter()
            .filter(|tool| tool.name != "help")
            .map(|tool| tool.name.to_string())
            .collect();
        variants.sort();
        catalog.sort();
        assert_eq!(catalog, variants);
    }

    #[test]
    fn command_runner_streams_stdin_and_stops_long_jobs() {
        let executable = std::env::current_exe().expect("test executable");
        let root = std::env::current_dir().expect("working directory");
        let echo = spawn_process(
            executable.clone().into_os_string(),
            root.clone(),
            vec![
                "--ignored".into(),
                "--exact".into(),
                "tests::runner_echo_child".into(),
                "--nocapture".into(),
            ],
        );
        echo.controls
            .send(TaskControl::Input("round trip".into()))
            .expect("send stdin");
        let (output, ok, stopped) = finish_task(&echo, Duration::from_secs(5));
        assert!(ok && !stopped, "{output}");
        assert!(output.contains("echo:round trip"), "{output}");

        let waiting = spawn_process(
            executable.into_os_string(),
            root,
            vec![
                "--ignored".into(),
                "--exact".into(),
                "tests::runner_wait_child".into(),
                "--nocapture".into(),
            ],
        );
        std::thread::sleep(Duration::from_millis(100));
        waiting
            .controls
            .send(TaskControl::Stop)
            .expect("stop child");
        let (_, _, stopped) = finish_task(&waiting, Duration::from_secs(5));
        assert!(stopped);
    }

    #[test]
    #[ignore]
    fn runner_echo_child() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).expect("read stdin");
        println!("echo:{}", line.trim_end());
    }

    #[test]
    #[ignore]
    fn runner_wait_child() {
        std::thread::sleep(Duration::from_secs(30));
    }

    fn finish_task(task: &RunningTask, timeout: Duration) -> (String, bool, bool) {
        let deadline = std::time::Instant::now() + timeout;
        let mut output = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "command runner timed out: {output}");
            match task.events.recv_timeout(remaining) {
                Ok(TaskEvent::Output(text)) => output.push_str(&text),
                Ok(TaskEvent::Finished { ok, stopped }) => return (output, ok, stopped),
                Err(error) => panic!("command runner channel failed: {error}: {output}"),
            }
        }
    }

    fn pascal_to_kebab(name: &str) -> String {
        let mut kebab = String::new();
        for (index, character) in name.chars().enumerate() {
            if index > 0 && character.is_ascii_uppercase() {
                kebab.push('-');
            }
            kebab.push(character.to_ascii_lowercase());
        }
        kebab
    }
}
