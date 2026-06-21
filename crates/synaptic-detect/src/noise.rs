use std::path::Path;

// Own-output + cache/build/dependency directories.
const SKIP_DIRS: &[&str] = &[
    "venv",
    ".venv",
    "env",
    ".env",
    "node_modules",
    "__pycache__",
    ".git",
    "dist",
    "build",
    "target",
    "out",
    "site-packages",
    "lib64",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".eggs",
    "synaptic-out",
    "coverage",
    "lcov-report",
    "visual-tests",
    "visual-test",
    "__snapshots__",
    "snapshots",
    "storybook-static",
    "dist-protected",
    ".next",
    ".nuxt",
    ".turbo",
    ".angular",
    ".idea",
    ".cache",
    ".parcel-cache",
    ".svelte-kit",
    ".terraform",
    ".serverless",
    ".synaptic",
    ".worktrees",
];

// Lockfiles we never index.
const SKIP_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "go.work.sum",
];

/// True if a directory name is generated/dependency noise.
pub fn is_noise_dir(name: &str, parent: &Path) -> bool {
    if SKIP_DIRS.contains(&name) {
        return true;
    }
    if name.ends_with("_venv") || name.ends_with("_env") || name.ends_with(".egg-info") {
        return true;
    }
    // worktrees/ nested inside a dotted dir (e.g. .git/worktrees/).
    if name == "worktrees"
        && parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
    {
        return true;
    }
    false
}

/// True if a filename is a lockfile we never index.
pub(crate) fn is_skip_file(name: &str) -> bool {
    SKIP_FILES.contains(&name)
}
