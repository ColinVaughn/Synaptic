//! Differential test of this crate's CVSS v4.0 scorer against the reference
//! implementation published by FIRST.
//!
//! The fixture was produced by driving FIRST's own calculator over a stride
//! through the base-metric lattice, plus the corners that pin both ends of the
//! scale. It exists because the v4 macrovector table is 270 hand-transcribed
//! numbers, and a single wrong digit there is silent: it produces a plausible
//! score for one narrow class of vector and nothing else ever notices.

use synaptic_vuln::cvss_v4_base_score;

const REFERENCE: &str = include_str!("fixtures/cvss4_reference_scores.txt");

#[test]
fn every_reference_vector_scores_exactly_as_first_calculates_it() {
    let mut checked = 0;
    let mut mismatches = Vec::new();

    for line in REFERENCE.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (vector, expected) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("malformed fixture line: {line}"));
        let expected: f64 = expected.parse().expect("fixture score is a number");

        match cvss_v4_base_score(vector) {
            Some(actual) if (actual - expected).abs() < 1e-9 => checked += 1,
            Some(actual) => mismatches.push(format!("{vector}: expected {expected}, got {actual}")),
            None => mismatches.push(format!("{vector}: expected {expected}, got None")),
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} vectors disagree with the reference implementation:\n{}",
        mismatches.len(),
        checked + mismatches.len(),
        mismatches.join("\n")
    );
    assert!(checked > 500, "fixture shrank unexpectedly: {checked}");
}

/// The same differential over the entire base-metric lattice.
///
/// The committed fixture is a stride through that lattice, which is enough to
/// catch a wrong table cell in CI without carrying a 6 MB file in the
/// repository. This test checks all 104,976 of them against a file produced by
/// the reference calculator, and exists so that anyone who edits the
/// macrovector table can prove they did not break a vector the stride skips:
///
/// ```text
/// SYNAPTIC_CVSS4_FULL_FIXTURE=/path/to/cvss4_all.txt \
///   cargo test -p synaptic-vuln --test cvss4_reference -- --ignored
/// ```
#[test]
#[ignore = "needs a full reference fixture; set SYNAPTIC_CVSS4_FULL_FIXTURE"]
fn every_vector_in_the_whole_lattice_matches_the_reference() {
    let Ok(path) = std::env::var("SYNAPTIC_CVSS4_FULL_FIXTURE") else {
        panic!("set SYNAPTIC_CVSS4_FULL_FIXTURE to a reference-generated fixture");
    };
    let body = std::fs::read_to_string(&path).expect("fixture is readable");

    let mut checked = 0;
    let mut mismatches = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (vector, expected) = line.split_once(' ').expect("well-formed fixture line");
        let expected: f64 = expected.parse().expect("fixture score is a number");
        match cvss_v4_base_score(vector) {
            Some(actual) if (actual - expected).abs() < 1e-9 => checked += 1,
            Some(actual) => mismatches.push(format!("{vector}: expected {expected}, got {actual}")),
            None => mismatches.push(format!("{vector}: expected {expected}, got None")),
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} vectors disagree:\n{}",
        mismatches.len(),
        checked + mismatches.len(),
        mismatches
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(checked, 104_976, "fixture does not cover the whole lattice");
}

/// Worsening any single metric must never lower the score.
///
/// This is the property that catches a mistyped digit in the macrovector table,
/// which spot checks cannot: a wrong cell shows up as a dip in a lattice that
/// is otherwise monotone.
#[test]
fn worsening_any_single_metric_never_lowers_the_score() {
    // Each metric's values, ordered from least to most severe.
    const ORDERED: &[(&str, &[&str])] = &[
        ("AV", &["P", "L", "A", "N"]),
        ("AC", &["H", "L"]),
        ("AT", &["P", "N"]),
        ("PR", &["H", "L", "N"]),
        ("UI", &["A", "P", "N"]),
        ("VC", &["N", "L", "H"]),
        ("VI", &["N", "L", "H"]),
        ("VA", &["N", "L", "H"]),
        ("SC", &["N", "L", "H"]),
        ("SI", &["N", "L", "H"]),
        ("SA", &["N", "L", "H"]),
    ];

    // A deterministic stride over the lattice keeps this a CI-time test while
    // still visiting every metric value many times over.
    let mut violations = Vec::new();
    for seed in (0..104_976_u32).step_by(37) {
        let base = vector_at(seed);
        let base_score = cvss_v4_base_score(&render(&base)).expect("base vector scores");

        for (index, (metric, values)) in ORDERED.iter().enumerate() {
            let current = base[index];
            let position = values.iter().position(|value| *value == current).unwrap();
            if position + 1 >= values.len() {
                continue;
            }
            let mut worse = base.clone();
            worse[index] = values[position + 1];
            let worse_score = cvss_v4_base_score(&render(&worse)).expect("worsened vector scores");
            if worse_score < base_score - 1e-9 {
                violations.push(format!(
                    "{} worsening {metric} {current}->{} dropped {base_score} to {worse_score}",
                    render(&base),
                    values[position + 1]
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} monotonicity violations:\n{}",
        violations.len(),
        violations
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Decode an index into the base-metric lattice, in the same order as `ORDERED`.
fn vector_at(mut seed: u32) -> Vec<&'static str> {
    const SPACE: &[&[&str]] = &[
        &["N", "A", "L", "P"],
        &["L", "H"],
        &["N", "P"],
        &["N", "L", "H"],
        &["N", "P", "A"],
        &["H", "L", "N"],
        &["H", "L", "N"],
        &["H", "L", "N"],
        &["H", "L", "N"],
        &["H", "L", "N"],
        &["H", "L", "N"],
    ];
    let mut out = Vec::with_capacity(SPACE.len());
    for values in SPACE.iter().rev() {
        out.push(values[(seed as usize) % values.len()]);
        seed /= values.len() as u32;
    }
    out.reverse();
    out
}

fn render(values: &[&str]) -> String {
    const NAMES: &[&str] = &[
        "AV", "AC", "AT", "PR", "UI", "VC", "VI", "VA", "SC", "SI", "SA",
    ];
    let body = NAMES
        .iter()
        .zip(values)
        .map(|(name, value)| format!("{name}:{value}"))
        .collect::<Vec<_>>()
        .join("/");
    format!("CVSS:4.0/{body}")
}
