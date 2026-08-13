//! Deterministic, bounded detection of repository build and test commands.
//!
//! The legacy root-marker detector remains for API compatibility. New `speculate`
//! and API repair verification use the recursive command plan so every independent
//! project in a polyglot repository is either checked or represented by an explicit
//! gap.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const COMMAND_PLAN_VERSION: u32 = 1;
const MAX_SCAN_DEPTH: usize = 16;
const MAX_DIRECTORIES: usize = 50_000;
const MAX_PROJECTS: usize = 512;
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const NOISE_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".render-cache",
    ".vscode",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "synaptic-out",
    "target",
    "vendor",
];

/// The commands detected for a project. `test` may contain a `{files}`
/// placeholder that the runner expands to the at-risk test files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCommands {
    /// The ecosystem the markers identified (e.g. "rust"), for reporting.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub language: Option<String>,
    /// A command that runs the tests (possibly file-scoped via `{files}`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub test: Option<String>,
    /// A command that builds / type-checks the project.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub check: Option<String>,
}

/// One independently verifiable build-system root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedProject {
    pub ecosystem: String,
    /// Repository-relative working directory. An empty path means repository root.
    pub root: PathBuf,
    pub manifests: Vec<PathBuf>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    /// A native workspace/solution build at this root owns matching child projects.
    #[serde(default)]
    pub covers_descendants: bool,
}

impl DetectedProject {
    /// Whether this project can own at least one changed repository-relative path.
    pub fn is_relevant_to(&self, changed_files: &[String]) -> bool {
        if changed_files.is_empty() {
            return true;
        }
        changed_files.iter().map(Path::new).any(|file| {
            if !self.root.as_os_str().is_empty() {
                return file.starts_with(&self.root);
            }
            self.manifests.iter().any(|manifest| manifest == file)
                || source_ecosystem(
                    file.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default(),
                )
                .is_some_and(|source| project_supports_source(&self.ecosystem, source))
        })
    }
}

/// A project surface for which a safe, deterministic command could not be inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDetectionGap {
    pub ecosystem: String,
    pub root: PathBuf,
    pub capability: MissingCapability,
    pub reason: String,
}

impl CommandDetectionGap {
    /// Whether this unresolved capability can affect the changed paths.
    pub fn is_relevant_to(&self, changed_files: &[String]) -> bool {
        if changed_files.is_empty() || self.ecosystem == "repository" {
            return true;
        }
        changed_files.iter().map(Path::new).any(|file| {
            if !self.root.as_os_str().is_empty() {
                return file.starts_with(&self.root);
            }
            source_ecosystem(
                file.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
            )
            .is_some_and(|source| project_supports_source(&self.ecosystem, source))
                || manifest_matches_ecosystem(file, &self.ecosystem)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingCapability {
    Check,
    Test,
    CheckAndTest,
}

impl MissingCapability {
    pub fn resolved_by(self, explicit_check: bool, explicit_test: bool) -> bool {
        match self {
            Self::Check => explicit_check,
            Self::Test => explicit_test,
            Self::CheckAndTest => explicit_check && explicit_test,
        }
    }
}

/// Complete recursive build/test plan for a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCommandPlan {
    pub version: u32,
    pub projects: Vec<DetectedProject>,
    pub gaps: Vec<CommandDetectionGap>,
    pub directories_scanned: usize,
    pub truncated: bool,
}

#[derive(Debug, Default)]
struct DirectorySnapshot {
    root: PathBuf,
    files: BTreeSet<String>,
    directories: BTreeSet<String>,
}

fn has(files: &[String], name: &str) -> bool {
    files.iter().any(|f| f == name)
}

/// Detect test/check commands from the marker file names present at the repo
/// root. The first ecosystem (in a fixed priority order, for determinism) whose
/// marker is present wins; ties never depend on input order.
pub fn detect_commands(root_files: &[String]) -> DetectedCommands {
    // Rust.
    if has(root_files, "Cargo.toml") {
        return DetectedCommands {
            language: Some("rust".into()),
            test: Some("cargo test".into()),
            check: Some("cargo build".into()),
        };
    }
    // Go.
    if has(root_files, "go.mod") {
        return DetectedCommands {
            language: Some("go".into()),
            test: Some("go test ./...".into()),
            check: Some("go build ./...".into()),
        };
    }
    // Python: pytest over the at-risk files; no separate build step.
    if has(root_files, "pyproject.toml")
        || has(root_files, "setup.py")
        || has(root_files, "pytest.ini")
        || has(root_files, "tox.ini")
    {
        return DetectedCommands {
            language: Some("python".into()),
            test: Some("pytest {files}".into()),
            check: None,
        };
    }
    // Node / TypeScript. A tsconfig adds a type-check step.
    if has(root_files, "package.json") {
        let check = has(root_files, "tsconfig.json").then(|| "npx tsc --noEmit".to_string());
        return DetectedCommands {
            language: Some("node".into()),
            test: Some("npm test".into()),
            check,
        };
    }
    DetectedCommands::default()
}

/// Recursively detect all supported project/build systems without entering
/// dependency, generated-output, VCS, or symlinked directories.
pub fn detect_command_plan(root: &Path) -> io::Result<DetectedCommandPlan> {
    let (snapshots, directories_scanned, truncated_scan) = scan_directories(root)?;
    let mut projects = Vec::new();
    let mut gaps = Vec::new();
    let mut truncated_projects = false;

    for snapshot in &snapshots {
        detect_directory(root, snapshot, &mut projects, &mut gaps)?;
        if projects.len() >= MAX_PROJECTS {
            projects.truncate(MAX_PROJECTS);
            truncated_projects = true;
            gaps.push(CommandDetectionGap {
                ecosystem: "repository".into(),
                root: PathBuf::new(),
                capability: MissingCapability::CheckAndTest,
                reason: format!(
                    "project cap of {MAX_PROJECTS} reached; configure commands explicitly"
                ),
            });
            break;
        }
    }

    collapse_owned_descendants(&mut projects);
    add_missing_compilation_gaps(&snapshots, &projects, &mut gaps);
    add_uncovered_language_gaps(&snapshots, &projects, &mut gaps);
    collapse_redundant_gaps(&mut gaps);
    if truncated_scan {
        gaps.push(CommandDetectionGap {
            ecosystem: "repository".into(),
            root: PathBuf::new(),
            capability: MissingCapability::CheckAndTest,
            reason: format!(
                "directory scan exceeded depth {MAX_SCAN_DEPTH} or directory cap {MAX_DIRECTORIES}"
            ),
        });
    }
    projects.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.ecosystem.cmp(&right.ecosystem))
    });
    gaps.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.ecosystem.cmp(&right.ecosystem))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    gaps.dedup();

    Ok(DetectedCommandPlan {
        version: COMMAND_PLAN_VERSION,
        projects,
        gaps,
        directories_scanned,
        truncated: truncated_scan || truncated_projects,
    })
}

fn scan_directories(root: &Path) -> io::Result<(Vec<DirectorySnapshot>, usize, bool)> {
    let mut queue = VecDeque::from([(PathBuf::new(), 0_usize)]);
    let mut snapshots = Vec::new();
    let mut scanned = 0_usize;
    let mut truncated = false;

    while let Some((relative, depth)) = queue.pop_front() {
        if scanned >= MAX_DIRECTORIES {
            truncated = true;
            break;
        }
        scanned += 1;
        let directory = root.join(&relative);
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        let mut snapshot = DirectorySnapshot {
            root: relative.clone(),
            ..DirectorySnapshot::default()
        };
        for entry in entries {
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                snapshot.directories.insert(name.clone());
                if !is_noise_directory(&name) {
                    if depth < MAX_SCAN_DEPTH {
                        queue.push_back((relative.join(name), depth + 1));
                    } else {
                        truncated = true;
                    }
                }
            } else if kind.is_file() {
                snapshot.files.insert(name);
            }
        }
        snapshots.push(snapshot);
    }
    snapshots.sort_by(|left, right| {
        left.root
            .components()
            .count()
            .cmp(&right.root.components().count())
            .then_with(|| left.root.cmp(&right.root))
    });
    Ok((snapshots, scanned, truncated))
}

fn is_noise_directory(name: &str) -> bool {
    NOISE_DIRECTORIES
        .iter()
        .any(|noise| name.eq_ignore_ascii_case(noise))
}

fn detect_directory(
    repository: &Path,
    snapshot: &DirectorySnapshot,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) -> io::Result<()> {
    let files = &snapshot.files;
    let relative = &snapshot.root;
    let absolute = repository.join(relative);

    if files.contains("Cargo.toml") {
        let manifest = read_bounded(&absolute.join("Cargo.toml"))?;
        projects.push(project(
            "rust",
            relative,
            ["Cargo.toml"],
            ["cargo build --workspace --all-targets"],
            ["cargo test --workspace"],
            manifest.contains("[workspace]"),
        ));
    }
    if files.contains("go.mod") {
        projects.push(project(
            "go",
            relative,
            ["go.mod"],
            ["go build ./..."],
            ["go test ./..."],
            false,
        ));
    }
    if files.contains("pyproject.toml")
        || files.contains("setup.py")
        || files.contains("setup.cfg")
        || files.contains("pytest.ini")
        || files.contains("tox.ini")
    {
        let manifests = [
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "pytest.ini",
            "tox.ini",
        ]
        .into_iter()
        .filter(|name| files.contains(*name));
        projects.push(project(
            "python",
            relative,
            manifests,
            ["python -m compileall -q ."],
            ["python -m pytest"],
            false,
        ));
    }
    if files.contains("package.json") {
        detect_node(&absolute, relative, files, projects, gaps)?;
    }
    if files.contains("deno.json") || files.contains("deno.jsonc") {
        projects.push(project(
            "deno",
            relative,
            files
                .iter()
                .filter(|name| matches!(name.as_str(), "deno.json" | "deno.jsonc"))
                .map(String::as_str),
            ["deno check ."],
            ["deno test"],
            false,
        ));
    }

    detect_jvm(&absolute, relative, files, projects, gaps)?;
    detect_dotnet(relative, files, projects, gaps);

    if files.contains("Package.swift") {
        projects.push(project(
            "swift",
            relative,
            ["Package.swift"],
            ["swift build"],
            ["swift test"],
            false,
        ));
    }
    if files.contains("composer.json") {
        detect_composer(&absolute, relative, projects, gaps)?;
    }
    if files.contains("Gemfile") || files.iter().any(|name| extension(name) == "gemspec") {
        let mut tests = Vec::new();
        if files.contains("Rakefile") {
            tests.push("bundle exec rake test".to_string());
        } else if files.contains(".rspec") || snapshot.directories.contains("spec") {
            tests.push("bundle exec rspec".to_string());
        }
        projects.push(project(
            "ruby",
            relative,
            files
                .iter()
                .filter(|name| *name == "Gemfile" || extension(name) == "gemspec")
                .map(String::as_str),
            ["bundle check"],
            tests.iter().map(String::as_str),
            false,
        ));
        if tests.is_empty() {
            gap_for(
                gaps,
                "ruby",
                relative,
                MissingCapability::Test,
                "no Rake test task or RSpec marker was detected",
            );
        }
    }
    if files.contains("pubspec.yaml") {
        let contents = read_bounded(&absolute.join("pubspec.yaml"))?;
        let (ecosystem, tool) = if contents.contains("sdk: flutter") {
            ("flutter", "flutter")
        } else {
            ("dart", "dart")
        };
        projects.push(project(
            ecosystem,
            relative,
            ["pubspec.yaml"],
            [format!("{tool} analyze")],
            [format!("{tool} test")],
            false,
        ));
    }
    if files.contains("mix.exs") {
        projects.push(project(
            "elixir",
            relative,
            ["mix.exs"],
            ["mix compile --warnings-as-errors"],
            ["mix test"],
            false,
        ));
    }
    if files.contains("Project.toml") && files.contains("Manifest.toml") {
        projects.push(project(
            "julia",
            relative,
            ["Project.toml", "Manifest.toml"],
            ["julia --project=. -e \"using Pkg; Pkg.precompile()\""],
            ["julia --project=. -e \"using Pkg; Pkg.test()\""],
            false,
        ));
    }
    if files.contains("build.zig") {
        projects.push(project(
            "zig",
            relative,
            ["build.zig"],
            ["zig build"],
            ["zig build test"],
            false,
        ));
    }
    if files.contains("fpm.toml") {
        projects.push(project(
            "fortran-fpm",
            relative,
            ["fpm.toml"],
            ["fpm build"],
            ["fpm test"],
            false,
        ));
    }
    detect_native(relative, files, projects, gaps, &absolute)?;
    detect_powershell(relative, files, &snapshot.directories, projects, gaps);
    detect_lua(relative, files, &snapshot.directories, projects, gaps);
    detect_shell(relative, files, projects, gaps);
    detect_pascal(relative, files, projects, gaps);

    if files.contains("qlpack.yml") || files.contains("qlpack.yaml") {
        projects.push(project(
            "codeql",
            relative,
            files
                .iter()
                .filter(|name| matches!(name.as_str(), "qlpack.yml" | "qlpack.yaml"))
                .map(String::as_str),
            ["codeql pack create --no-publish ."],
            ["codeql test run ."],
            false,
        ));
    }
    if files.contains("sfdx-project.json") {
        gap(
            gaps,
            "salesforce-apex",
            relative,
            "Apex compilation/tests require an authenticated org; configure an authenticated validation command explicitly",
        );
    }
    if files.contains(".terraform.lock.hcl") || files.iter().any(|name| extension(name) == "tf") {
        let has_tests = files
            .iter()
            .any(|name| name.to_ascii_lowercase().ends_with(".tftest.hcl"));
        projects.push(project(
            "terraform",
            relative,
            files
                .iter()
                .filter(|name| *name == ".terraform.lock.hcl" || extension(name) == "tf")
                .map(String::as_str),
            ["terraform fmt -check", "terraform validate"],
            has_tests.then_some("terraform test"),
            false,
        ));
        if !has_tests {
            gap_for(
                gaps,
                "terraform",
                relative,
                MissingCapability::Test,
                "no .tftest.hcl suite was detected; configure an integration or policy test command",
            );
        }
    }

    Ok(())
}

fn detect_node(
    absolute: &Path,
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) -> io::Result<()> {
    let source = read_bounded(&absolute.join("package.json"))?;
    let value: serde_json::Value = match serde_json::from_str(&source) {
        Ok(value) => value,
        Err(error) => {
            gap(
                gaps,
                "node",
                relative,
                format!("package.json could not be parsed: {error}"),
            );
            return Ok(());
        }
    };
    let (ecosystem, runner, lockfile) = if files.contains("pnpm-lock.yaml") {
        ("node-pnpm", "pnpm run", Some("pnpm-lock.yaml"))
    } else if files.contains("yarn.lock") {
        ("node-yarn", "yarn run", Some("yarn.lock"))
    } else if files.contains("bun.lock") || files.contains("bun.lockb") {
        (
            "node-bun",
            "bun run",
            files
                .contains("bun.lock")
                .then_some("bun.lock")
                .or(Some("bun.lockb")),
        )
    } else {
        (
            "node-npm",
            "npm run",
            files
                .contains("package-lock.json")
                .then_some("package-lock.json"),
        )
    };
    let scripts = value.get("scripts").and_then(serde_json::Value::as_object);
    let script = |name: &str| {
        scripts
            .and_then(|scripts| scripts.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
    };
    let has_script = |name: &str| script(name).is_some();
    let mut checks = Vec::new();
    for name in ["typecheck", "check", "build"] {
        if has_script(name) {
            checks.push(format!("{runner} {name}"));
        }
    }
    let test = ["test:ci", "test", "ci:test"].into_iter().find(|name| {
        script(name).is_some_and(|command| {
            let command = command.to_ascii_lowercase();
            !command.contains("no test specified") && command != "exit 1"
        })
    });
    let mut manifests = vec!["package.json"];
    if let Some(lockfile) = lockfile {
        manifests.push(lockfile);
    }
    projects.push(project(
        ecosystem,
        relative,
        manifests,
        checks.iter().map(String::as_str),
        test.map(|name| format!("{runner} {name}")),
        false,
    ));
    if test.is_none() {
        gap_for(
            gaps,
            ecosystem,
            relative,
            MissingCapability::Test,
            "no declared test:ci, test, or ci:test script was detected",
        );
    }
    Ok(())
}

fn detect_jvm(
    absolute: &Path,
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) -> io::Result<()> {
    let gradle = files.contains("build.gradle") || files.contains("build.gradle.kts");
    let maven = files.contains("pom.xml");
    let sbt = files.contains("build.sbt");
    let mill = files.contains("build.sc") || files.contains("mill-build");
    let kinds = [gradle, maven, sbt, mill]
        .into_iter()
        .filter(|present| *present)
        .count();
    if kinds > 1 {
        gap(
            gaps,
            "jvm",
            relative,
            "multiple JVM build systems are present at one root; configure authoritative commands",
        );
        return Ok(());
    }
    if gradle {
        let wrapper = if cfg!(windows) && files.contains("gradlew.bat") {
            "gradlew.bat"
        } else if files.contains("gradlew") {
            "./gradlew"
        } else {
            "gradle"
        };
        projects.push(project(
            "jvm-gradle",
            relative,
            files
                .iter()
                .filter(|name| {
                    matches!(
                        name.as_str(),
                        "build.gradle"
                            | "build.gradle.kts"
                            | "settings.gradle"
                            | "settings.gradle.kts"
                    )
                })
                .map(String::as_str),
            [format!("{wrapper} --no-daemon assemble")],
            [format!("{wrapper} --no-daemon test")],
            files.contains("settings.gradle") || files.contains("settings.gradle.kts"),
        ));
    } else if maven {
        let wrapper = if cfg!(windows) && files.contains("mvnw.cmd") {
            "mvnw.cmd"
        } else if files.contains("mvnw") {
            "./mvnw"
        } else {
            "mvn"
        };
        let contents = read_bounded(&absolute.join("pom.xml"))?;
        projects.push(project(
            "jvm-maven",
            relative,
            ["pom.xml"],
            [format!("{wrapper} -B -ntp -DskipTests package")],
            [format!("{wrapper} -B -ntp test")],
            contents.contains("<modules>") || contents.contains("<modules "),
        ));
    } else if sbt {
        projects.push(project(
            "jvm-sbt",
            relative,
            ["build.sbt"],
            ["sbt Test/compile"],
            ["sbt test"],
            true,
        ));
    } else if mill {
        let wrapper = if cfg!(windows) && files.contains("mill.bat") {
            "mill.bat"
        } else if files.contains("mill") {
            "./mill"
        } else {
            "mill"
        };
        projects.push(project(
            "jvm-mill",
            relative,
            files
                .iter()
                .filter(|name| matches!(name.as_str(), "build.sc" | "mill-build"))
                .map(String::as_str),
            [format!("{wrapper} --no-server __.compile")],
            [format!("{wrapper} --no-server __.test")],
            true,
        ));
    }
    Ok(())
}

fn detect_dotnet(
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let solutions = files
        .iter()
        .filter(|name| matches!(extension(name), "sln" | "slnx"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let project_files = files
        .iter()
        .filter(|name| matches!(extension(name), "csproj" | "fsproj" | "vbproj"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if solutions.is_empty() && project_files.is_empty() {
        return;
    }
    let (manifests, target, covers_descendants) = if solutions.len() == 1 {
        (solutions.clone(), command_file_arg(solutions[0]), true)
    } else if solutions.len() > 1 {
        gap(
            gaps,
            "dotnet",
            relative,
            "multiple solution files are present; configure the authoritative solution command",
        );
        return;
    } else if project_files.len() == 1 {
        (
            project_files.clone(),
            command_file_arg(project_files[0]),
            false,
        )
    } else {
        gap(
            gaps,
            "dotnet",
            relative,
            "multiple project files have no solution; configure authoritative build/test commands",
        );
        return;
    };
    let Some(target) = target else {
        gap(
            gaps,
            "dotnet",
            relative,
            ".NET project/solution filename is unsafe for automatic shell execution",
        );
        return;
    };
    projects.push(project(
        "dotnet",
        relative,
        manifests,
        [format!("dotnet build {target} --nologo")],
        [format!("dotnet test {target} --nologo --no-build")],
        covers_descendants,
    ));
}

fn detect_composer(
    absolute: &Path,
    relative: &Path,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) -> io::Result<()> {
    let source = read_bounded(&absolute.join("composer.json"))?;
    let value: serde_json::Value = match serde_json::from_str(&source) {
        Ok(value) => value,
        Err(error) => {
            gap(
                gaps,
                "php-composer",
                relative,
                format!("composer.json could not be parsed: {error}"),
            );
            return Ok(());
        }
    };
    let scripts = value.get("scripts").and_then(serde_json::Value::as_object);
    let has_script = |name: &str| scripts.is_some_and(|scripts| scripts.contains_key(name));
    let mut checks = vec!["composer validate --strict --no-check-publish".to_string()];
    for name in ["analyse", "analyze", "static-analysis", "check"] {
        if has_script(name) {
            checks.push(format!("composer run {name}"));
        }
    }
    let test = ["test:ci", "test", "ci:test"]
        .into_iter()
        .find(|name| has_script(name));
    projects.push(project(
        "php-composer",
        relative,
        ["composer.json"],
        checks.iter().map(String::as_str),
        test.map(|name| format!("composer run {name}")),
        false,
    ));
    if test.is_none() {
        gap_for(
            gaps,
            "php-composer",
            relative,
            MissingCapability::Test,
            "no declared test:ci, test, or ci:test Composer script was detected",
        );
    }
    Ok(())
}

fn detect_native(
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
    absolute: &Path,
) -> io::Result<()> {
    if files.contains("CMakeLists.txt") {
        let build_dir = "synaptic-out/verification/cmake";
        projects.push(project(
            "native-cmake",
            relative,
            ["CMakeLists.txt"],
            [format!(
                "cmake -S . -B {build_dir} -DBUILD_TESTING=ON && cmake --build {build_dir}"
            )],
            [format!(
                "ctest --test-dir {build_dir} --output-on-failure --no-tests=error"
            )],
            true,
        ));
    } else if files.contains("meson.build") {
        let build_dir = "synaptic-out/verification/meson";
        projects.push(project(
            "native-meson",
            relative,
            ["meson.build"],
            [format!(
                "meson setup {build_dir} --buildtype=debug && meson compile -C {build_dir}"
            )],
            [format!("meson test -C {build_dir} --print-errorlogs")],
            true,
        ));
    } else if (files.contains("Makefile") || files.contains("makefile"))
        && !has_primary_build_manifest(files)
    {
        let manifest = if files.contains("Makefile") {
            "Makefile"
        } else {
            "makefile"
        };
        let contents = read_bounded(&absolute.join(manifest))?;
        let has_test = contents
            .lines()
            .any(|line| line.trim_start().starts_with("test:"));
        projects.push(project(
            "native-make",
            relative,
            [manifest],
            ["make"],
            has_test.then_some("make test"),
            false,
        ));
        if !has_test {
            gap_for(
                gaps,
                "native-make",
                relative,
                MissingCapability::Test,
                "Makefile has no explicit test target",
            );
        }
    }
    Ok(())
}

fn detect_lua(
    relative: &Path,
    files: &BTreeSet<String>,
    directories: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let rockspecs = files
        .iter()
        .filter(|name| extension(name) == "rockspec")
        .collect::<Vec<_>>();
    if rockspecs.is_empty() {
        return;
    }
    if rockspecs.len() > 1 {
        gap(
            gaps,
            "lua",
            relative,
            "multiple rockspecs are present; configure the authoritative package command",
        );
        return;
    }
    let Some(rockspec) = command_file_arg(rockspecs[0]) else {
        gap(
            gaps,
            "lua",
            relative,
            "rockspec filename is unsafe for automatic shell execution",
        );
        return;
    };
    let has_tests = directories.contains("spec") || directories.contains("tests");
    projects.push(project(
        "lua",
        relative,
        rockspecs.iter().map(|name| name.as_str()),
        [format!("luarocks make --local {rockspec}")],
        has_tests.then_some("busted"),
        false,
    ));
    if !has_tests {
        gap_for(
            gaps,
            "lua",
            relative,
            MissingCapability::Test,
            "no Busted spec/tests directory was detected",
        );
    }
}

fn detect_shell(
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let scripts = files
        .iter()
        .filter(|name| matches!(extension(name), "sh" | "bash" | "bats"))
        .collect::<Vec<_>>();
    if scripts.is_empty() {
        return;
    }
    let Some(arguments) = scripts
        .iter()
        .map(|name| command_file_arg(name))
        .collect::<Option<Vec<_>>>()
    else {
        gap(
            gaps,
            "shell",
            relative,
            "shell filename is unsafe for automatic command execution",
        );
        return;
    };
    let bats = scripts
        .iter()
        .zip(&arguments)
        .filter(|(name, _)| extension(name) == "bats")
        .map(|(_, argument)| argument.clone())
        .collect::<Vec<_>>();
    let syntax_inputs = scripts
        .iter()
        .zip(&arguments)
        .filter(|(name, _)| matches!(extension(name), "sh" | "bash"))
        .map(|(_, argument)| argument.clone())
        .collect::<Vec<_>>();
    let checks =
        (!syntax_inputs.is_empty()).then(|| format!("bash -n {}", syntax_inputs.join(" ")));
    let tests = (!bats.is_empty()).then(|| format!("bats {}", bats.join(" ")));
    projects.push(project(
        "shell",
        relative,
        scripts.iter().map(|name| name.as_str()),
        checks,
        tests,
        false,
    ));
    if bats.is_empty() {
        gap_for(
            gaps,
            "shell",
            relative,
            MissingCapability::Test,
            "no Bats test file was detected",
        );
    }
}

fn detect_pascal(
    relative: &Path,
    files: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let entry = files
        .iter()
        .find(|name| matches!(extension(name), "lpi" | "lpr" | "dpr" | "dproj"));
    let Some(entry) = entry else { return };
    let Some(argument) = command_file_arg(entry) else {
        gap(
            gaps,
            "pascal",
            relative,
            "Pascal project filename is unsafe for automatic command execution",
        );
        return;
    };
    let check = match extension(entry) {
        "lpi" => format!("lazbuild --build-all {argument}"),
        "dproj" => format!("msbuild {argument} /t:Build"),
        _ => format!("fpc {argument}"),
    };
    projects.push(project(
        "pascal",
        relative,
        [entry.as_str()],
        [check],
        None::<String>,
        false,
    ));
    gap_for(
        gaps,
        "pascal",
        relative,
        MissingCapability::Test,
        "no standard Pascal test manifest exists; configure the project test runner",
    );
}

fn add_uncovered_language_gaps(
    snapshots: &[DirectorySnapshot],
    projects: &[DetectedProject],
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let mut seen = BTreeSet::new();
    for snapshot in snapshots {
        for file in &snapshot.files {
            let Some(language) = source_ecosystem(file) else {
                continue;
            };
            let source = snapshot.root.join(file);
            let covered = projects.iter().any(|project| {
                source.starts_with(&project.root)
                    && project_supports_source(&project.ecosystem, language)
            });
            if covered || !seen.insert((snapshot.root.clone(), language.to_string())) {
                continue;
            }
            let reason = match language {
                "sql" => {
                    "SQL validation is database-specific; configure schema, migration, and integration commands"
                }
                "salesforce-apex" => {
                    "Apex compilation/tests require an authenticated org; configure validation commands"
                }
                "verilog" => {
                    "no HDL build/test manifest was detected; configure simulator, lint, and test commands"
                }
                "classic-asp" => {
                    "Classic ASP has no portable local compiler/test convention; configure host-specific commands"
                }
                _ => {
                    "source is outside a recognized build/test project; configure authoritative commands"
                }
            };
            gap(gaps, language, &snapshot.root, reason);
        }
    }
}

fn add_missing_compilation_gaps(
    snapshots: &[DirectorySnapshot],
    projects: &[DetectedProject],
    gaps: &mut Vec<CommandDetectionGap>,
) {
    for project in projects
        .iter()
        .filter(|project| project.ecosystem.starts_with("node-") && project.checks.is_empty())
    {
        let typed_source = snapshots.iter().any(|snapshot| {
            if !snapshot.root.starts_with(&project.root) {
                return false;
            }
            let owned_by_nested_project = projects.iter().any(|other| {
                other.ecosystem.starts_with("node-")
                    && other.root != project.root
                    && other.root.starts_with(&project.root)
                    && snapshot.root.starts_with(&other.root)
            });
            !owned_by_nested_project
                && snapshot.files.iter().any(|name| {
                    name.starts_with("tsconfig") && name.ends_with(".json")
                        || matches!(
                            extension(name),
                            "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" | "astro"
                        )
                })
        });
        if typed_source {
            gap_for(
                gaps,
                &project.ecosystem,
                &project.root,
                MissingCapability::Check,
                "typed frontend sources have no declared typecheck, check, or build script",
            );
        }
    }
}

fn collapse_redundant_gaps(gaps: &mut Vec<CommandDetectionGap>) {
    gaps.sort_by(|left, right| {
        left.root
            .components()
            .count()
            .cmp(&right.root.components().count())
            .then_with(|| left.root.cmp(&right.root))
            .then_with(|| left.ecosystem.cmp(&right.ecosystem))
    });
    let mut retained: Vec<CommandDetectionGap> = Vec::new();
    for candidate in gaps.drain(..) {
        let redundant = retained.iter().any(|parent| {
            parent.ecosystem == candidate.ecosystem
                && parent.capability == candidate.capability
                && parent.reason == candidate.reason
                && parent.root != candidate.root
                && candidate.root.starts_with(&parent.root)
        });
        if !redundant {
            retained.push(candidate);
        }
    }
    *gaps = retained;
}

fn source_ecosystem(name: &str) -> Option<&'static str> {
    match extension(name).to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "go" => Some("go"),
        "py" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte"
        | "astro" => Some("node"),
        "java" | "kt" | "kts" | "scala" | "sc" | "groovy" | "gradle" => Some("jvm"),
        "cs" | "fs" | "vb" | "razor" | "cshtml" => Some("dotnet"),
        "swift" => Some("swift"),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "m" | "mm" => Some("native"),
        "rb" => Some("ruby"),
        "php" => Some("php-composer"),
        "lua" => Some("lua"),
        "dart" => Some("dart"),
        "ex" | "exs" => Some("elixir"),
        "jl" => Some("julia"),
        "zig" => Some("zig"),
        "ps1" | "psm1" => Some("powershell"),
        "v" | "sv" | "svh" | "vh" => Some("verilog"),
        "f" | "f90" | "f95" | "f03" | "f08" | "for" => Some("fortran"),
        "sh" | "bash" | "bats" => Some("shell"),
        "ql" | "qll" => Some("codeql"),
        "cls" | "trigger" => Some("salesforce-apex"),
        "pas" | "pp" | "dpr" | "dpk" | "lpr" => Some("pascal"),
        "asp" | "asa" => Some("classic-asp"),
        "sql" => Some("sql"),
        "tf" | "tfvars" => Some("terraform"),
        _ => None,
    }
}

fn project_supports_source(project: &str, source: &str) -> bool {
    project == source
        || (project.starts_with("node-") && source == "node")
        || (project == "deno" && source == "node")
        || (project.starts_with("jvm-") && source == "jvm")
        || (project.starts_with("native-") && matches!(source, "native" | "fortran" | "verilog"))
        || (project == "fortran-fpm" && source == "fortran")
        || (project == "flutter" && source == "dart")
}

fn manifest_matches_ecosystem(path: &Path, ecosystem: &str) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match ecosystem {
        "rust" => name == "Cargo.toml",
        "go" => name == "go.mod",
        "python" => matches!(name, "pyproject.toml" | "setup.py" | "setup.cfg"),
        "node" | "node-npm" | "node-pnpm" | "node-yarn" | "node-bun" => name == "package.json",
        "jvm" | "jvm-gradle" | "jvm-maven" | "jvm-sbt" | "jvm-mill" => matches!(
            name,
            "build.gradle" | "build.gradle.kts" | "pom.xml" | "build.sbt" | "build.sc"
        ),
        "dotnet" => matches!(
            extension(name),
            "sln" | "slnx" | "csproj" | "fsproj" | "vbproj"
        ),
        "swift" => name == "Package.swift",
        "php-composer" => name == "composer.json",
        "ruby" => name == "Gemfile" || extension(name) == "gemspec",
        "dart" | "flutter" => name == "pubspec.yaml",
        "elixir" => name == "mix.exs",
        "julia" => name == "Project.toml",
        "zig" => name == "build.zig",
        "terraform" => extension(name) == "tf",
        "codeql" => matches!(name, "qlpack.yml" | "qlpack.yaml"),
        "salesforce-apex" => name == "sfdx-project.json",
        _ => false,
    }
}

fn has_primary_build_manifest(files: &BTreeSet<String>) -> bool {
    [
        "Cargo.toml",
        "Package.swift",
        "build.gradle",
        "build.gradle.kts",
        "build.sbt",
        "build.zig",
        "composer.json",
        "go.mod",
        "mix.exs",
        "package.json",
        "pom.xml",
        "pubspec.yaml",
        "pyproject.toml",
    ]
    .iter()
    .any(|name| files.contains(*name))
}

fn command_file_arg(name: &str) -> Option<String> {
    if name.chars().any(|character| character.is_control()) {
        return None;
    }
    if cfg!(windows) {
        if name
            .chars()
            .any(|character| matches!(character, '&' | '|' | '<' | '>' | '^' | '%' | '!' | '"'))
        {
            return None;
        }
        Some(format!("\"{name}\""))
    } else {
        Some(format!("'{}'", name.replace('\'', "'\\''")))
    }
}

fn detect_powershell(
    relative: &Path,
    files: &BTreeSet<String>,
    directories: &BTreeSet<String>,
    projects: &mut Vec<DetectedProject>,
    gaps: &mut Vec<CommandDetectionGap>,
) {
    let manifests = files
        .iter()
        .filter(|name| extension(name) == "psd1")
        .map(String::as_str)
        .collect::<Vec<_>>();
    if manifests.is_empty() {
        return;
    }
    let has_tests = files
        .iter()
        .any(|name| name.to_ascii_lowercase().ends_with(".tests.ps1"))
        || directories.contains("tests");
    projects.push(project(
        "powershell",
        relative,
        manifests,
        ["pwsh -NoProfile -Command \"Invoke-ScriptAnalyzer -Path . -Recurse -Severity Error\""],
        has_tests.then_some("pwsh -NoProfile -Command \"Invoke-Pester -Path . -CI\""),
        false,
    ));
    if !has_tests {
        gap_for(
            gaps,
            "powershell",
            relative,
            MissingCapability::Test,
            "no Pester test marker was detected",
        );
    }
}

fn project<M, C, T, MS, CS, TS>(
    ecosystem: &str,
    root: &Path,
    manifests: M,
    checks: C,
    tests: T,
    covers_descendants: bool,
) -> DetectedProject
where
    M: IntoIterator<Item = MS>,
    C: IntoIterator<Item = CS>,
    T: IntoIterator<Item = TS>,
    MS: AsRef<str>,
    CS: AsRef<str>,
    TS: AsRef<str>,
{
    DetectedProject {
        ecosystem: ecosystem.into(),
        root: root.to_path_buf(),
        manifests: manifests
            .into_iter()
            .map(|manifest| root.join(manifest.as_ref()))
            .collect(),
        checks: checks
            .into_iter()
            .map(|command| command.as_ref().to_string())
            .collect(),
        tests: tests
            .into_iter()
            .map(|command| command.as_ref().to_string())
            .collect(),
        covers_descendants,
    }
}

fn gap(
    gaps: &mut Vec<CommandDetectionGap>,
    ecosystem: &str,
    root: &Path,
    reason: impl Into<String>,
) {
    gap_for(
        gaps,
        ecosystem,
        root,
        MissingCapability::CheckAndTest,
        reason,
    );
}

fn gap_for(
    gaps: &mut Vec<CommandDetectionGap>,
    ecosystem: &str,
    root: &Path,
    capability: MissingCapability,
    reason: impl Into<String>,
) {
    gaps.push(CommandDetectionGap {
        ecosystem: ecosystem.into(),
        root: root.to_path_buf(),
        capability,
        reason: reason.into(),
    });
}

fn collapse_owned_descendants(projects: &mut Vec<DetectedProject>) {
    projects.sort_by(|left, right| {
        left.root
            .components()
            .count()
            .cmp(&right.root.components().count())
            .then_with(|| left.root.cmp(&right.root))
            .then_with(|| left.ecosystem.cmp(&right.ecosystem))
    });
    let mut retained: Vec<DetectedProject> = Vec::new();
    for candidate in projects.drain(..) {
        let owned = retained.iter().any(|parent| {
            parent.covers_descendants
                && same_ecosystem_family(&parent.ecosystem, &candidate.ecosystem)
                && parent.root != candidate.root
                && candidate.root.starts_with(&parent.root)
        });
        if !owned {
            retained.push(candidate);
        }
    }
    *projects = retained;
}

fn same_ecosystem_family(left: &str, right: &str) -> bool {
    left == right
        || (left.starts_with("jvm-") && right.starts_with("jvm-"))
        || (left.starts_with("node-") && right.starts_with("node-"))
        || (left.starts_with("native-") && right.starts_with("native-"))
}

fn read_bounded(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manifest {} exceeds {MAX_MANIFEST_BYTES} bytes",
                path.display()
            ),
        ));
    }
    fs::read_to_string(path)
}

fn extension(name: &str) -> &str {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_rust() {
        let d = detect_commands(&files(&["Cargo.toml", "src"]));
        assert_eq!(d.language.as_deref(), Some("rust"));
        assert_eq!(d.test.as_deref(), Some("cargo test"));
        assert_eq!(d.check.as_deref(), Some("cargo build"));
    }

    #[test]
    fn detects_python_pytest_with_file_placeholder() {
        let d = detect_commands(&files(&["pyproject.toml"]));
        assert_eq!(d.language.as_deref(), Some("python"));
        assert_eq!(d.test.as_deref(), Some("pytest {files}"));
        assert!(d.check.is_none());
    }

    #[test]
    fn detects_go() {
        let d = detect_commands(&files(&["go.mod", "main.go"]));
        assert_eq!(d.language.as_deref(), Some("go"));
        assert_eq!(d.test.as_deref(), Some("go test ./..."));
    }

    #[test]
    fn node_check_only_with_tsconfig() {
        let plain = detect_commands(&files(&["package.json"]));
        assert_eq!(plain.test.as_deref(), Some("npm test"));
        assert!(plain.check.is_none(), "no tsconfig -> no type-check");
        let ts = detect_commands(&files(&["package.json", "tsconfig.json"]));
        assert_eq!(ts.check.as_deref(), Some("npx tsc --noEmit"));
    }

    #[test]
    fn rust_wins_over_node_for_determinism() {
        // A polyglot repo root: the fixed priority order picks rust regardless of
        // the order the file names arrive in.
        let d = detect_commands(&files(&["package.json", "Cargo.toml"]));
        assert_eq!(d.language.as_deref(), Some("rust"));
    }

    #[test]
    fn nothing_detected_is_empty() {
        let d = detect_commands(&files(&["README.md"]));
        assert_eq!(d, DetectedCommands::default());
    }

    #[test]
    fn command_plan_recursively_covers_a_polyglot_repository() {
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "rust/Cargo.toml",
            "[package]\nname='sample'\nversion='0.1.0'\n",
        );
        write(repo.path(), "go/go.mod", "module example.test/sample\n");
        write(
            repo.path(),
            "jvm/pom.xml",
            "<project><modelVersion>4.0.0</modelVersion></project>",
        );
        write(repo.path(), "jvm/mvnw.cmd", "");
        write(
            repo.path(),
            "swift/Package.swift",
            "// swift-tools-version: 6.0\n",
        );
        write(
            repo.path(),
            "dotnet/App.csproj",
            "<Project Sdk='Microsoft.NET.Sdk'/>",
        );
        write(
            repo.path(),
            "php/composer.json",
            r#"{"scripts":{"analyse":"phpstan analyse","test":"phpunit"}}"#,
        );
        write(
            repo.path(),
            "elixir/mix.exs",
            "defmodule Sample.MixProject do end\n",
        );
        write(
            repo.path(),
            "zig/build.zig",
            "pub fn build(b: *std.Build) void {}\n",
        );

        let plan = detect_command_plan(repo.path()).unwrap();
        let ecosystems = plan
            .projects
            .iter()
            .map(|project| project.ecosystem.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            ecosystems,
            BTreeSet::from([
                "dotnet",
                "elixir",
                "go",
                "jvm-maven",
                "php-composer",
                "rust",
                "swift",
                "zig"
            ])
        );
        assert!(
            plan.projects
                .iter()
                .all(|project| { !project.checks.is_empty() && !project.tests.is_empty() })
        );
        assert!(plan.gaps.is_empty(), "unexpected gaps: {:?}", plan.gaps);
    }

    #[test]
    fn command_plan_uses_declared_node_scripts_and_lockfile_manager() {
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "package.json",
            r#"{"scripts":{"typecheck":"tsc --noEmit","build":"vite build","test:ci":"vitest run"}}"#,
        );
        write(repo.path(), "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");

        let plan = detect_command_plan(repo.path()).unwrap();
        let project = &plan.projects[0];

        assert_eq!(project.ecosystem, "node-pnpm");
        assert_eq!(project.checks, vec!["pnpm run typecheck", "pnpm run build"]);
        assert_eq!(project.tests, vec!["pnpm run test:ci"]);
        assert_eq!(
            project.manifests,
            vec![
                PathBuf::from("package.json"),
                PathBuf::from("pnpm-lock.yaml")
            ]
        );
    }

    #[test]
    fn npm_init_placeholder_is_not_treated_as_a_real_test_suite() {
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        write(repo.path(), "app.js", "module.exports = 1;\n");

        let plan = detect_command_plan(repo.path()).unwrap();

        assert!(plan.projects[0].tests.is_empty());
        assert!(plan.gaps.iter().any(|gap| {
            gap.ecosystem == "node-npm" && gap.capability == MissingCapability::Test
        }));
    }

    #[test]
    fn workspace_roots_collapse_redundant_native_children() {
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "Cargo.toml",
            "[workspace]\nmembers=['crates/child']\n",
        );
        write(
            repo.path(),
            "crates/child/Cargo.toml",
            "[package]\nname='child'\nversion='0.1.0'\n",
        );
        write(repo.path(), "settings.gradle.kts", "include(\":app\")\n");
        write(repo.path(), "build.gradle.kts", "plugins { java }\n");
        write(repo.path(), "app/build.gradle.kts", "plugins { java }\n");
        write(repo.path(), "node_modules/ignored/package.json", "{}\n");
        write(repo.path(), ".render-cache/ignored/package.json", "{}\n");
        write(repo.path(), "vendor/ignored/composer.json", "{}\n");

        let plan = detect_command_plan(repo.path()).unwrap();

        assert_eq!(
            plan.projects
                .iter()
                .filter(|project| project.ecosystem == "rust")
                .count(),
            1
        );
        assert_eq!(
            plan.projects
                .iter()
                .filter(|project| project.ecosystem == "jvm-gradle")
                .count(),
            1
        );
        assert!(
            !plan
                .projects
                .iter()
                .any(|project| project.root.starts_with("node_modules")
                    || project.root.starts_with(".render-cache")
                    || project.root.starts_with("vendor"))
        );
    }

    #[test]
    fn unsafe_or_unportable_projects_are_explicit_gaps() {
        let repo = tempfile::tempdir().unwrap();
        write(repo.path(), "salesforce/sfdx-project.json", "{}\n");
        write(repo.path(), "sql/schema.sql", "select 1;\n");

        let plan = detect_command_plan(repo.path()).unwrap();

        assert!(
            plan.gaps
                .iter()
                .any(|gap| gap.ecosystem == "salesforce-apex"
                    && gap.reason.contains("authenticated"))
        );
        assert!(
            plan.gaps
                .iter()
                .any(|gap| gap.ecosystem == "sql" && gap.reason.contains("database-specific"))
        );
    }

    #[test]
    fn root_polyglot_plans_select_only_the_changed_ecosystem() {
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "Cargo.toml",
            "[package]\nname='sample'\nversion='0.1.0'\n",
        );
        write(
            repo.path(),
            "package.json",
            r#"{"scripts":{"build":"vite build"}}"#,
        );
        write(repo.path(), "src/lib.rs", "pub fn value() -> u8 { 1 }\n");
        write(repo.path(), "web/app.ts", "export const value = 1;\n");

        let plan = detect_command_plan(repo.path()).unwrap();
        let rust_change = vec!["src/lib.rs".to_string()];
        let node_change = vec!["web/app.ts".to_string()];
        let rust = plan
            .projects
            .iter()
            .find(|project| project.ecosystem == "rust")
            .unwrap();
        let node = plan
            .projects
            .iter()
            .find(|project| project.ecosystem == "node-npm")
            .unwrap();
        let node_test_gap = plan
            .gaps
            .iter()
            .find(|gap| gap.ecosystem == "node-npm")
            .unwrap();

        assert!(rust.is_relevant_to(&rust_change));
        assert!(!node.is_relevant_to(&rust_change));
        assert!(!node_test_gap.is_relevant_to(&rust_change));
        assert!(!rust.is_relevant_to(&node_change));
        assert!(node.is_relevant_to(&node_change));
        assert!(node_test_gap.is_relevant_to(&node_change));
    }

    #[test]
    fn detects_build_systems_for_every_supported_language_family() {
        let repo = tempfile::tempdir().unwrap();
        write(repo.path(), "gradle/build.gradle.kts", "plugins { java }\n");
        write(
            repo.path(),
            "gradle/settings.gradle.kts",
            "rootProject.name='sample'\n",
        );
        write(repo.path(), "gradle/gradlew", "");
        write(repo.path(), "gradle/gradlew.bat", "");
        write(
            repo.path(),
            "scala/build.sbt",
            "scalaVersion := \"3.5.0\"\n",
        );
        write(repo.path(), "dotnet/App.sln", "\n");
        write(
            repo.path(),
            "ruby/Gemfile",
            "source 'https://rubygems.org'\n",
        );
        write(repo.path(), "ruby/Rakefile", "task :test\n");
        write(
            repo.path(),
            "flutter/pubspec.yaml",
            "environment:\n  sdk: flutter\n",
        );
        write(repo.path(), "dart/pubspec.yaml", "name: sample\n");
        write(repo.path(), "julia/Project.toml", "[deps]\n");
        write(
            repo.path(),
            "julia/Manifest.toml",
            "manifest_format = \"2.0\"\n",
        );
        write(repo.path(), "native/CMakeLists.txt", "enable_testing()\n");
        write(repo.path(), "meson/meson.build", "project('sample', 'c')\n");
        write(repo.path(), "make/Makefile", "all:\n\ntest:\n");
        write(repo.path(), "fortran/fpm.toml", "name = 'sample'\n");
        write(repo.path(), "lua/sample.rockspec", "package = 'sample'\n");
        write(
            repo.path(),
            "lua/spec/sample_spec.lua",
            "describe('sample', function() end)\n",
        );
        write(repo.path(), "powershell/Module.psd1", "@{}\n");
        write(
            repo.path(),
            "powershell/tests/Module.Tests.ps1",
            "Describe 'Module' {}\n",
        );
        write(repo.path(), "terraform/main.tf", "terraform {}\n");
        write(
            repo.path(),
            "terraform/main.tftest.hcl",
            "run \"test\" {}\n",
        );
        write(repo.path(), "codeql/qlpack.yml", "name: sample/tests\n");
        write(
            repo.path(),
            "shell/check.bats",
            "@test 'sample' { true; }\n",
        );
        write(repo.path(), "pascal/sample.lpi", "<CONFIG/>\n");
        write(repo.path(), "deno/deno.json", "{}\n");

        let plan = detect_command_plan(repo.path()).unwrap();
        let ecosystems = plan
            .projects
            .iter()
            .map(|project| project.ecosystem.as_str())
            .collect::<BTreeSet<_>>();

        for expected in [
            "codeql",
            "dart",
            "deno",
            "dotnet",
            "flutter",
            "fortran-fpm",
            "julia",
            "jvm-gradle",
            "jvm-sbt",
            "lua",
            "native-cmake",
            "native-make",
            "native-meson",
            "pascal",
            "powershell",
            "ruby",
            "shell",
            "terraform",
        ] {
            assert!(
                ecosystems.contains(expected),
                "missing {expected}: {plan:?}"
            );
        }
    }

    #[test]
    fn project_cap_is_reported_as_truncated_and_never_exceeded() {
        let repo = tempfile::tempdir().unwrap();
        for index in 0..=MAX_PROJECTS {
            write(
                repo.path(),
                &format!("project-{index:04}/go.mod"),
                &format!("module example.test/project-{index}\n"),
            );
        }

        let plan = detect_command_plan(repo.path()).unwrap();

        assert_eq!(plan.projects.len(), MAX_PROJECTS);
        assert!(plan.truncated);
        assert!(
            plan.gaps
                .iter()
                .any(|gap| { gap.ecosystem == "repository" && gap.reason.contains("project cap") })
        );
    }
}
