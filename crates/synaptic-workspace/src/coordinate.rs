//! Published **package coordinate** of a member — the key cross-repo imports are
//! matched against. Parsed from the member's package manifest:
//! Rust `Cargo.toml` → crate name, JS `package.json` → package name, Go `go.mod`
//! → module path, Python `pyproject.toml` → distribution name, Maven `pom.xml` →
//! `groupId:artifactId`, Gradle → module name, .NET `.csproj`/`.fsproj` → project
//! name. (JVM/.NET imports spell a package namespace, not this build coordinate —
//! see the synthesized namespace in `export_surface`.)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Value as Toml;

/// Which package ecosystem a coordinate belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    Cargo,
    Npm,
    Go,
    Python,
    /// JVM ecosystem — Maven (`groupId:artifactId`) or Gradle.
    Jvm,
    /// Gradle module (settings `rootProject.name` or dir name).
    Gradle,
    /// .NET project (`AssemblyName`/`RootNamespace` or project-file stem).
    DotNet,
    /// A coordinate synthesized from a declared repo/artifact name (no recognized
    /// package manifest to read an ecosystem from).
    Other,
}

/// A member's published coordinate: the name another member would `import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coordinate {
    pub ecosystem: Ecosystem,
    pub name: String,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Crate name from `Cargo.toml` `[package].name`.
fn cargo_name(root: &Path) -> Option<String> {
    let data: Toml = toml::from_str(&read(&root.join("Cargo.toml"))?).ok()?;
    data.get("package")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// Package name from `package.json` `name`.
fn npm_name(root: &Path) -> Option<String> {
    let data: serde_json::Value = serde_json::from_str(&read(&root.join("package.json"))?).ok()?;
    data.get("name")?.as_str().map(str::to_string)
}

/// Module path from `go.mod` — the first `module <path>` directive. go.mod is a
/// line-oriented format, not TOML, so it is parsed by hand.
fn go_module(root: &Path) -> Option<String> {
    for line in read(&root.join("go.mod"))?.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            // Strip an inline `// comment` and surrounding whitespace/quotes.
            let m = rest
                .split("//")
                .next()
                .unwrap_or(rest)
                .trim()
                .trim_matches('"');
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
    }
    None
}

/// Distribution name from `pyproject.toml`: PEP 621 `[project].name`, else
/// `[tool.poetry].name`.
fn python_name(root: &Path) -> Option<String> {
    let data: Toml = toml::from_str(&read(&root.join("pyproject.toml"))?).ok()?;
    if let Some(n) = data
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(Toml::as_str)
    {
        return Some(n.to_string());
    }
    data.get("tool")?
        .get("poetry")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// `groupId:artifactId` from a Maven `pom.xml` (artifactId alone if no groupId).
fn maven_name(root: &Path) -> Option<String> {
    let text = read(&root.join("pom.xml"))?;
    let doc = roxmltree::Document::parse(&text).ok()?;
    let root_el = doc.root_element();
    // Only the project's own coordinate: direct children, not <parent>/<dependency>.
    let child_text = |tag: &str| {
        root_el
            .children()
            .find(|c| c.is_element() && c.has_tag_name(tag))
            .and_then(|c| c.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let artifact = child_text("artifactId")?;
    Some(match child_text("groupId") {
        Some(g) => format!("{g}:{artifact}"),
        None => artifact,
    })
}

/// Gradle module name: `rootProject.name = '…'` in settings, else the dir name.
/// Triggered by the presence of a `build.gradle`/`build.gradle.kts`.
fn gradle_name(root: &Path) -> Option<String> {
    if !root.join("build.gradle").exists() && !root.join("build.gradle.kts").exists() {
        return None;
    }
    for f in ["settings.gradle", "settings.gradle.kts"] {
        if let Some(text) = read(&root.join(f)) {
            for line in text.lines() {
                let line = line.split("//").next().unwrap_or(line).trim();
                if let Some(rest) = line.strip_prefix("rootProject.name") {
                    let v = rest
                        .trim()
                        .trim_start_matches('=')
                        .trim()
                        .trim_matches(['"', '\'']);
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    root.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// The first project file (`*.csproj`/`*.fsproj`/`*.vbproj`) directly in `dir`,
/// lexically smallest for determinism.
fn first_dotnet_proj(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("csproj") | Some("fsproj") | Some("vbproj")
            )
        })
        .min()
}

/// The first Visual Studio solution (`*.sln`) directly in `dir`, lexically
/// smallest for determinism.
pub(crate) fn first_sln(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sln"))
        .min()
}

/// Project files (`*.csproj`/`*.fsproj`/`*.vbproj`) referenced by a `.sln`. `dir`
/// is the directory the solution sits in; returned paths are `dir`-joined with
/// backslashes normalized. Parses `Project("{type}") = "Name", "path", "{GUID}"`
/// rows (the quoted path is field index 5 when splitting the line on `"`).
pub(crate) fn sln_project_files(dir: &Path, sln: &Path) -> Vec<PathBuf> {
    let Some(text) = read(sln) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("Project(") {
            continue;
        }
        let fields: Vec<&str> = line.split('"').collect();
        if let Some(path) = fields.get(5) {
            let rel = path.replace('\\', "/");
            if rel.ends_with(".csproj") || rel.ends_with(".fsproj") || rel.ends_with(".vbproj") {
                out.push(dir.join(rel));
            }
        }
    }
    out
}

/// `<AssemblyName>`/`<RootNamespace>` of a .NET project file, else its file stem.
fn dotnet_proj_name(proj: &Path) -> Option<String> {
    let text = std::fs::read_to_string(proj).ok()?;
    if let Ok(doc) = roxmltree::Document::parse(&text) {
        for tag in ["AssemblyName", "RootNamespace"] {
            if let Some(v) = doc
                .descendants()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(v.to_string());
            }
        }
    }
    proj.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// .NET project name for a member root. Tries, in order: a project file directly
/// in `root`; the first project a root `.sln` references (the standard layout —
/// solution at the repo root, projects in subdirectories); the `.sln` file stem.
/// Each project name is its `<AssemblyName>`/`<RootNamespace>` or file stem.
fn dotnet_name(root: &Path) -> Option<String> {
    if let Some(proj) = first_dotnet_proj(root) {
        return dotnet_proj_name(&proj);
    }
    let sln = first_sln(root)?;
    if let Some(proj) = sln_project_files(root, &sln).into_iter().min() {
        if let Some(name) = dotnet_proj_name(&proj) {
            return Some(name);
        }
    }
    sln.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// The package coordinate of the member rooted at `member_root`, or `None` if no
/// recognized manifest is present. Precedence: Cargo → npm → Go → Python → Maven
/// → Gradle → .NET (a member normally has exactly one).
pub fn package_coordinate(member_root: &Path) -> Option<Coordinate> {
    if let Some(name) = cargo_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Cargo,
            name,
        });
    }
    if let Some(name) = npm_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Npm,
            name,
        });
    }
    if let Some(name) = go_module(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Go,
            name,
        });
    }
    if let Some(name) = python_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Python,
            name,
        });
    }
    if let Some(name) = maven_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Jvm,
            name,
        });
    }
    if let Some(name) = gradle_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::Gradle,
            name,
        });
    }
    if let Some(name) = dotnet_name(member_root) {
        return Some(Coordinate {
            ecosystem: Ecosystem::DotNet,
            name,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn reads_cargo_crate_name() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "Cargo.toml",
            "[package]\nname = \"billing\"\nversion=\"0.1.0\"\n",
        );
        assert_eq!(
            package_coordinate(d.path()),
            Some(Coordinate {
                ecosystem: Ecosystem::Cargo,
                name: "billing".into()
            })
        );
    }

    #[test]
    fn reads_npm_package_name() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "package.json",
            "{\"name\": \"@acme/billing\", \"version\": \"1.0.0\"}",
        );
        assert_eq!(
            package_coordinate(d.path()),
            Some(Coordinate {
                ecosystem: Ecosystem::Npm,
                name: "@acme/billing".into()
            })
        );
    }

    #[test]
    fn reads_go_module_path_ignoring_comment() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "go.mod",
            "module github.com/acme/billing // the billing service\n\ngo 1.22\n",
        );
        assert_eq!(
            package_coordinate(d.path()),
            Some(Coordinate {
                ecosystem: Ecosystem::Go,
                name: "github.com/acme/billing".into()
            })
        );
    }

    #[test]
    fn reads_pep621_then_poetry() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "pyproject.toml",
            "[project]\nname = \"billing\"\n",
        );
        assert_eq!(
            package_coordinate(d.path()),
            Some(Coordinate {
                ecosystem: Ecosystem::Python,
                name: "billing".into()
            })
        );
        write(
            d.path(),
            "pyproject.toml",
            "[tool.poetry]\nname = \"billing-svc\"\n",
        );
        assert_eq!(package_coordinate(d.path()).unwrap().name, "billing-svc");
    }

    #[test]
    fn reads_maven_group_and_artifact() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "pom.xml",
            r#"<project><groupId>com.acme</groupId><artifactId>billing</artifactId></project>"#,
        );
        let c = package_coordinate(d.path()).unwrap();
        assert_eq!(c.ecosystem, Ecosystem::Jvm);
        assert_eq!(c.name, "com.acme:billing");
    }

    #[test]
    fn reads_gradle_root_project_name() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "build.gradle", "plugins { id 'java' }\n");
        write(
            d.path(),
            "settings.gradle",
            "rootProject.name = 'identity'\n",
        );
        let c = package_coordinate(d.path()).unwrap();
        assert_eq!(c.ecosystem, Ecosystem::Gradle);
        assert_eq!(c.name, "identity");
    }

    #[test]
    fn reads_dotnet_assembly_name() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "Svc.csproj",
            r#"<Project><PropertyGroup><AssemblyName>Acme.Svc</AssemblyName></PropertyGroup></Project>"#,
        );
        let c = package_coordinate(d.path()).unwrap();
        assert_eq!(c.ecosystem, Ecosystem::DotNet);
        assert_eq!(c.name, "Acme.Svc");
    }

    #[test]
    fn dotnet_coordinate_from_solution_with_subdir_projects() {
        // The standard .NET layout: a `.sln` at the repo root, projects in
        // subdirectories (no `.csproj` directly at the root). The coordinate is
        // taken from the first solution project's AssemblyName/RootNamespace.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("App")).unwrap();
        write(
            r,
            "Acme.sln",
            "Microsoft Visual Studio Solution File\n\
             Project(\"{GUID}\") = \"App\", \"App\\App.csproj\", \"{X}\"\nEndProject\n",
        );
        write(
            &r.join("App"),
            "App.csproj",
            r#"<Project><PropertyGroup><AssemblyName>Acme.App</AssemblyName></PropertyGroup></Project>"#,
        );
        let c = package_coordinate(r).unwrap();
        assert_eq!(c.ecosystem, Ecosystem::DotNet);
        assert_eq!(c.name, "Acme.App");
    }

    #[test]
    fn dotnet_coordinate_falls_back_to_solution_stem() {
        // A `.sln` whose referenced project is missing/unreadable still yields a
        // coordinate from the solution file stem, so a surface is built.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(
            r,
            "Gadget.sln",
            "Project(\"{GUID}\") = \"Gone\", \"Gone\\Gone.csproj\", \"{X}\"\n",
        );
        let c = package_coordinate(r).unwrap();
        assert_eq!(c.ecosystem, Ecosystem::DotNet);
        assert_eq!(c.name, "Gadget");
    }

    #[test]
    fn cargo_wins_over_others_and_none_when_absent() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(package_coordinate(d.path()), None);
        write(d.path(), "Cargo.toml", "[package]\nname = \"a\"\n");
        write(d.path(), "package.json", "{\"name\": \"b\"}");
        assert_eq!(
            package_coordinate(d.path()).unwrap().ecosystem,
            Ecosystem::Cargo
        );
    }
}
