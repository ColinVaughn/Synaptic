use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use synaptic_api::PackageCoordinate;

use crate::advisory::Advisory;

/// Where a corpus came from and how complete it is.
///
/// Every report prints this. A scanner that does not say which corpus it read,
/// and how stale that corpus is, invites the reader to mistake "no findings"
/// for "no vulnerabilities".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescription {
    pub origin: String,
    pub advisory_count: usize,
    /// Most recent `modified` timestamp across the corpus, as an opaque string.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub newest_modified: Option<String>,
    /// Files that could not be parsed. Non-zero means coverage is incomplete
    /// and the operator needs to know.
    pub unreadable_documents: usize,
}

/// A supplier of advisories.
pub trait AdvisorySource {
    /// Every advisory that names this package as affected.
    fn advisories_for(&self, coordinate: &PackageCoordinate) -> Vec<&Advisory>;

    /// Provenance and completeness of the corpus.
    fn describe(&self) -> SourceDescription;
}

/// An advisory corpus read from a directory of OSV JSON documents.
///
/// Reads the filesystem only, never the network.
#[derive(Debug, Clone, Default)]
pub struct LocalDirSource {
    origin: String,
    advisories: Vec<Advisory>,
    index: BTreeMap<PackageCoordinate, Vec<usize>>,
    newest_modified: Option<String>,
    unreadable_documents: usize,
}

impl LocalDirSource {
    /// Load every `.json` document under `root`, recursively.
    ///
    /// A document that fails to parse is counted rather than propagated: one
    /// malformed file in a large corpus must not stop the rest of the corpus
    /// from being scanned. The count is reported so the gap stays visible.
    ///
    /// Reading and parsing dominate a scan -- a cargo corpus is 2,698 documents
    /// and npm's is 226,161 -- and each document is independent, so they are
    /// parsed in parallel. Paths are sorted first and results collected in that
    /// same order, so the corpus a scan sees never depends on which thread
    /// happened to finish first.
    pub fn load(root: &Path) -> Result<Self, SourceError> {
        if !root.exists() {
            return Err(SourceError::Missing(root.to_path_buf()));
        }
        let mut documents = Vec::new();
        collect_json_files(root, &mut documents)?;
        documents.sort();

        let parsed: Vec<Result<Advisory, ()>> = documents
            .par_iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).map_err(|_| ())?;
                Advisory::parse(&body).map_err(|_| ())
            })
            .collect();

        let mut advisories = Vec::with_capacity(parsed.len());
        let mut unreadable_documents = 0;
        for outcome in parsed {
            match outcome {
                Ok(advisory) => advisories.push(advisory),
                // A document that fails to parse is counted rather than
                // propagated: one malformed file in a large corpus must not
                // stop the rest from being scanned.
                Err(()) => unreadable_documents += 1,
            }
        }

        let mut source = Self::from_advisories(root.display().to_string(), advisories);
        source.unreadable_documents = unreadable_documents;
        Ok(source)
    }

    /// Record how many documents could not be read.
    ///
    /// A source built from advisories someone else fetched still has to report
    /// its gaps, or an incomplete result reads as a complete one.
    pub fn set_unreadable_documents(&mut self, count: usize) {
        self.unreadable_documents = count;
    }

    /// Build a source directly from parsed advisories, for callers that
    /// obtained them some other way.
    pub fn from_advisories(origin: impl Into<String>, advisories: Vec<Advisory>) -> Self {
        let mut index: BTreeMap<PackageCoordinate, Vec<usize>> = BTreeMap::new();
        let mut newest_modified: Option<String> = None;
        for (position, advisory) in advisories.iter().enumerate() {
            for affected in &advisory.affected {
                let slot = index.entry(affected.package.clone()).or_default();
                if !slot.contains(&position) {
                    slot.push(position);
                }
            }
            if let Some(modified) = &advisory.modified {
                if newest_modified
                    .as_ref()
                    .is_none_or(|current| modified > current)
                {
                    newest_modified = Some(modified.clone());
                }
            }
        }
        Self {
            origin: origin.into(),
            advisories,
            index,
            newest_modified,
            unreadable_documents: 0,
        }
    }
}

impl AdvisorySource for LocalDirSource {
    fn advisories_for(&self, coordinate: &PackageCoordinate) -> Vec<&Advisory> {
        self.index
            .get(coordinate)
            .map(|positions| {
                positions
                    .iter()
                    .filter_map(|position| self.advisories.get(*position))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn describe(&self) -> SourceDescription {
        SourceDescription {
            origin: self.origin.clone(),
            advisory_count: self.advisories.len(),
            newest_modified: self.newest_modified.clone(),
            unreadable_documents: self.unreadable_documents,
        }
    }
}

/// Several per-ecosystem corpora answering as one.
///
/// A polyglot repository needs advisories for each ecosystem it locks, and
/// those ship as separate exports. Queries fan out to every member; a
/// coordinate only ever matches the corpus that carries its ecosystem.
#[derive(Debug, Default)]
pub struct CompositeSource {
    members: Vec<LocalDirSource>,
}

impl CompositeSource {
    pub fn new(members: Vec<LocalDirSource>) -> Self {
        Self { members }
    }

    pub fn push(&mut self, source: LocalDirSource) {
        self.members.push(source);
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

impl AdvisorySource for CompositeSource {
    fn advisories_for(&self, coordinate: &PackageCoordinate) -> Vec<&Advisory> {
        self.members
            .iter()
            .flat_map(|member| member.advisories_for(coordinate))
            .collect()
    }

    fn describe(&self) -> SourceDescription {
        // One combined description, so a report still states exactly what was
        // searched and how complete it was.
        let mut origin = self
            .members
            .iter()
            .map(|member| member.describe().origin)
            .collect::<Vec<_>>();
        origin.sort();
        SourceDescription {
            origin: if origin.is_empty() {
                "(no corpus)".into()
            } else {
                origin.join(", ")
            },
            advisory_count: self
                .members
                .iter()
                .map(|member| member.describe().advisory_count)
                .sum(),
            newest_modified: self
                .members
                .iter()
                .filter_map(|member| member.describe().newest_modified)
                .max(),
            unreadable_documents: self
                .members
                .iter()
                .map(|member| member.describe().unreadable_documents)
                .sum(),
        }
    }
}

fn collect_json_files(directory: &Path, out: &mut Vec<PathBuf>) -> Result<(), SourceError> {
    let entries = std::fs::read_dir(directory).map_err(|source| SourceError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SourceError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("cannot read advisory directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("advisory directory {0} does not exist")]
    Missing(PathBuf),
    #[error("cannot reach {url}: {message}")]
    Transport { url: String, message: String },
    #[error("OSV publishes no advisories for the {0} ecosystem")]
    UnsupportedEcosystem(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_api::Ecosystem;

    fn write(root: &Path, relative: &str, body: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn advisory_json(id: &str, package: &str, modified: &str) -> String {
        format!(
            r#"{{
                "id": "{id}",
                "modified": "{modified}",
                "affected": [
                    {{ "package": {{ "ecosystem": "crates.io", "name": "{package}" }} }}
                ]
            }}"#
        )
    }

    fn cargo(name: &str) -> PackageCoordinate {
        PackageCoordinate::new(Ecosystem::Cargo, name)
    }

    #[test]
    fn loads_advisories_and_indexes_them_by_affected_package() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.json",
            &advisory_json("OSV-A", "alpha", "2026-01-01T00:00:00Z"),
        );
        write(
            dir.path(),
            "b.json",
            &advisory_json("OSV-B", "beta", "2026-02-01T00:00:00Z"),
        );

        let source = LocalDirSource::load(dir.path()).unwrap();

        let hits = source.advisories_for(&cargo("alpha"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "OSV-A");
    }

    #[test]
    fn recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "nested/deeper/c.json",
            &advisory_json("OSV-C", "gamma", "2026-03-01T00:00:00Z"),
        );

        let source = LocalDirSource::load(dir.path()).unwrap();

        assert_eq!(source.advisories_for(&cargo("gamma")).len(), 1);
    }

    #[test]
    fn ignores_files_that_are_not_json() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "not an advisory");
        write(
            dir.path(),
            "a.json",
            &advisory_json("OSV-A", "alpha", "2026-01-01T00:00:00Z"),
        );

        let source = LocalDirSource::load(dir.path()).unwrap();

        assert_eq!(source.describe().advisory_count, 1);
        assert_eq!(source.describe().unreadable_documents, 0);
    }

    #[test]
    fn an_unparseable_document_is_counted_rather_than_aborting_the_load() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "broken.json", "{ this is not json");
        write(
            dir.path(),
            "good.json",
            &advisory_json("OSV-A", "alpha", "2026-01-01T00:00:00Z"),
        );

        let source = LocalDirSource::load(dir.path()).unwrap();
        let description = source.describe();

        assert_eq!(description.advisory_count, 1);
        assert_eq!(
            description.unreadable_documents, 1,
            "incomplete coverage must be visible, not silent"
        );
    }

    #[test]
    fn describes_the_corpus_origin_and_newest_advisory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.json",
            &advisory_json("OSV-A", "alpha", "2026-01-01T00:00:00Z"),
        );
        write(
            dir.path(),
            "b.json",
            &advisory_json("OSV-B", "beta", "2026-07-30T00:00:00Z"),
        );

        let description = LocalDirSource::load(dir.path()).unwrap().describe();

        assert_eq!(description.advisory_count, 2);
        assert_eq!(
            description.newest_modified.as_deref(),
            Some("2026-07-30T00:00:00Z")
        );
        assert!(description.origin.contains(dir.path().to_str().unwrap()));
    }

    #[test]
    fn an_unknown_package_has_no_advisories() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.json",
            &advisory_json("OSV-A", "alpha", "2026-01-01T00:00:00Z"),
        );

        let source = LocalDirSource::load(dir.path()).unwrap();

        assert!(source.advisories_for(&cargo("nothing-here")).is_empty());
    }

    #[test]
    fn one_advisory_affecting_several_packages_is_indexed_under_each() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "multi.json",
            r#"{
                "id": "OSV-MULTI",
                "affected": [
                    { "package": { "ecosystem": "crates.io", "name": "alpha" } },
                    { "package": { "ecosystem": "crates.io", "name": "beta" } }
                ]
            }"#,
        );

        let source = LocalDirSource::load(dir.path()).unwrap();

        assert_eq!(source.advisories_for(&cargo("alpha"))[0].id, "OSV-MULTI");
        assert_eq!(source.advisories_for(&cargo("beta"))[0].id, "OSV-MULTI");
    }

    #[test]
    fn withdrawn_advisories_are_returned_so_the_gate_can_record_them() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "w.json",
            r#"{
                "id": "OSV-W",
                "withdrawn": "2026-05-01T00:00:00Z",
                "affected": [
                    { "package": { "ecosystem": "crates.io", "name": "alpha" } }
                ]
            }"#,
        );

        let source = LocalDirSource::load(dir.path()).unwrap();
        let hits = source.advisories_for(&cargo("alpha"));

        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_withdrawn());
    }

    #[test]
    fn a_composite_answers_from_whichever_ecosystem_corpus_holds_the_package() {
        let cargo_corpus = LocalDirSource::from_advisories(
            "cargo-corpus",
            vec![
                Advisory::parse(&advisory_json("RUSTSEC-1", "alpha", "2026-01-01T00:00:00Z"))
                    .unwrap(),
            ],
        );
        let npm = LocalDirSource::from_advisories(
            "npm-corpus",
            vec![Advisory::parse(
                r#"{
                    "id": "GHSA-1",
                    "modified": "2026-06-01T00:00:00Z",
                    "affected": [
                        { "package": { "ecosystem": "npm", "name": "left-pad" } }
                    ]
                }"#,
            )
            .unwrap()],
        );
        let composite = CompositeSource::new(vec![cargo_corpus, npm]);

        assert_eq!(composite.advisories_for(&cargo("alpha"))[0].id, "RUSTSEC-1");
        assert_eq!(
            composite.advisories_for(&PackageCoordinate::new(Ecosystem::Npm, "left-pad"))[0].id,
            "GHSA-1"
        );
        assert!(composite
            .advisories_for(&PackageCoordinate::new(Ecosystem::Npm, "alpha"))
            .is_empty());
    }

    #[test]
    fn a_composite_reports_every_corpus_it_searched() {
        let composite = CompositeSource::new(vec![
            LocalDirSource::from_advisories(
                "cargo-corpus",
                vec![Advisory::parse(&advisory_json("A", "a", "2026-01-01T00:00:00Z")).unwrap()],
            ),
            LocalDirSource::from_advisories(
                "npm-corpus",
                vec![Advisory::parse(&advisory_json("B", "b", "2026-07-01T00:00:00Z")).unwrap()],
            ),
        ]);

        let described = composite.describe();

        assert_eq!(described.advisory_count, 2);
        assert!(described.origin.contains("cargo-corpus"));
        assert!(described.origin.contains("npm-corpus"));
        assert_eq!(
            described.newest_modified.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "the newest advisory across all corpora"
        );
    }

    #[test]
    fn an_empty_composite_names_itself_rather_than_looking_like_a_real_corpus() {
        let described = CompositeSource::default().describe();

        assert_eq!(described.origin, "(no corpus)");
        assert_eq!(described.advisory_count, 0);
    }

    #[test]
    fn a_missing_directory_is_an_error_not_an_empty_corpus() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("does-not-exist");

        let error = LocalDirSource::load(&absent).unwrap_err();

        assert!(matches!(error, SourceError::Missing(_)));
    }

    #[test]
    fn an_empty_directory_loads_as_an_empty_corpus() {
        let dir = tempfile::tempdir().unwrap();

        let description = LocalDirSource::load(dir.path()).unwrap().describe();

        assert_eq!(description.advisory_count, 0);
        assert_eq!(description.newest_modified, None);
    }

    #[test]
    fn a_parallel_load_produces_the_same_corpus_every_time() {
        // Documents are parsed across threads, so nothing about the result may
        // depend on which thread finished first. Malformed documents are
        // interleaved so the unreadable count has to come out stable too.
        let dir = tempfile::tempdir().unwrap();
        for index in 0..200 {
            let body = if index % 17 == 0 {
                "{ not json".to_string()
            } else {
                format!(
                    r#"{{"id":"OSV-{index:04}","modified":"2026-01-{:02}T00:00:00Z","affected":[{{"package":{{"ecosystem":"crates.io","name":"pkg{index}"}}}}]}}"#,
                    (index % 28) + 1
                )
            };
            std::fs::write(dir.path().join(format!("{index:04}.json")), body).unwrap();
        }

        let first = LocalDirSource::load(dir.path()).unwrap();
        let baseline: Vec<String> = first
            .advisories
            .iter()
            .map(|advisory| advisory.id.clone())
            .collect();
        let described = AdvisorySource::describe(&first);

        for _ in 0..8 {
            let again = LocalDirSource::load(dir.path()).unwrap();
            let ids: Vec<String> = again
                .advisories
                .iter()
                .map(|advisory| advisory.id.clone())
                .collect();
            assert_eq!(ids, baseline, "advisory order must be stable across loads");
            assert_eq!(AdvisorySource::describe(&again), described);
        }

        // Sorted by file name, which is the order the paths were walked in.
        assert!(baseline.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(described.advisory_count, 188);
        assert_eq!(described.unreadable_documents, 12);
        assert_eq!(
            described.newest_modified.as_deref(),
            Some("2026-01-28T00:00:00Z")
        );
    }
}
