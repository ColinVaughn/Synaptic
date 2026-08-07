//! CVSS v4.0 base scoring.
//!
//! This is a port of the scoring procedure published by FIRST in the CVSS v4.0
//! specification, together with the lookup data from the reference calculator
//! at <https://github.com/FIRSTdotorg/cvss-v4-calculator>:
//!
//! > Copyright (c) 2023 FIRST.ORG, Inc., Red Hat, and contributors
//! > SPDX-License-Identifier: BSD-2-Clause
//!
//! Only the base score (CVSS-B) is computed, matching how
//! [`crate::cvss_v3_base_score`] treats v3: threat and environmental metrics
//! present in a vector are ignored and take their specified defaults (`E:A`,
//! `CR:H`, `IR:H`, `AR:H`). A publisher who scored a lower exploit maturity
//! therefore sees the higher base number here, which is the standard reading of
//! a "base score" and errs toward attention rather than away from it.
//!
//! Severity distances are held in integer tenths rather than floats. The
//! reference implementation compares distances against zero to choose a maximal
//! vector, and doing that in binary floating point invites a wrong branch on an
//! exactly-representable-looking value like 0.1 + 0.2.

/// Score of every reachable macrovector, from the published lookup table.
///
/// Sorted by key so the lookup is a binary search. Generated from the reference
/// calculator's `cvss_lookup.js` rather than transcribed by hand: one wrong
/// digit in here is silent, affecting a single narrow class of vector.
static MACROVECTOR_SCORES: &[(&str, f64)] = &[
    ("000000", 10.0),
    ("000001", 9.9),
    ("000010", 9.8),
    ("000011", 9.5),
    ("000020", 9.5),
    ("000021", 9.2),
    ("000100", 10.0),
    ("000101", 9.6),
    ("000110", 9.3),
    ("000111", 8.7),
    ("000120", 9.1),
    ("000121", 8.1),
    ("000200", 9.3),
    ("000201", 9.0),
    ("000210", 8.9),
    ("000211", 8.0),
    ("000220", 8.1),
    ("000221", 6.8),
    ("001000", 9.8),
    ("001001", 9.5),
    ("001010", 9.5),
    ("001011", 9.2),
    ("001020", 9.0),
    ("001021", 8.4),
    ("001100", 9.3),
    ("001101", 9.2),
    ("001110", 8.9),
    ("001111", 8.1),
    ("001120", 8.1),
    ("001121", 6.5),
    ("001200", 8.8),
    ("001201", 8.0),
    ("001210", 7.8),
    ("001211", 7.0),
    ("001220", 6.9),
    ("001221", 4.8),
    ("002001", 9.2),
    ("002011", 8.2),
    ("002021", 7.2),
    ("002101", 7.9),
    ("002111", 6.9),
    ("002121", 5.0),
    ("002201", 6.9),
    ("002211", 5.5),
    ("002221", 2.7),
    ("010000", 9.9),
    ("010001", 9.7),
    ("010010", 9.5),
    ("010011", 9.2),
    ("010020", 9.2),
    ("010021", 8.5),
    ("010100", 9.5),
    ("010101", 9.1),
    ("010110", 9.0),
    ("010111", 8.3),
    ("010120", 8.4),
    ("010121", 7.1),
    ("010200", 9.2),
    ("010201", 8.1),
    ("010210", 8.2),
    ("010211", 7.1),
    ("010220", 7.2),
    ("010221", 5.3),
    ("011000", 9.5),
    ("011001", 9.3),
    ("011010", 9.2),
    ("011011", 8.5),
    ("011020", 8.5),
    ("011021", 7.3),
    ("011100", 9.2),
    ("011101", 8.2),
    ("011110", 8.0),
    ("011111", 7.2),
    ("011120", 7.0),
    ("011121", 5.9),
    ("011200", 8.4),
    ("011201", 7.0),
    ("011210", 7.1),
    ("011211", 5.2),
    ("011220", 5.0),
    ("011221", 3.0),
    ("012001", 8.6),
    ("012011", 7.5),
    ("012021", 5.2),
    ("012101", 7.1),
    ("012111", 5.2),
    ("012121", 2.9),
    ("012201", 6.3),
    ("012211", 2.9),
    ("012221", 1.7),
    ("100000", 9.8),
    ("100001", 9.5),
    ("100010", 9.4),
    ("100011", 8.7),
    ("100020", 9.1),
    ("100021", 8.1),
    ("100100", 9.4),
    ("100101", 8.9),
    ("100110", 8.6),
    ("100111", 7.4),
    ("100120", 7.7),
    ("100121", 6.4),
    ("100200", 8.7),
    ("100201", 7.5),
    ("100210", 7.4),
    ("100211", 6.3),
    ("100220", 6.3),
    ("100221", 4.9),
    ("101000", 9.4),
    ("101001", 8.9),
    ("101010", 8.8),
    ("101011", 7.7),
    ("101020", 7.6),
    ("101021", 6.7),
    ("101100", 8.6),
    ("101101", 7.6),
    ("101110", 7.4),
    ("101111", 5.8),
    ("101120", 5.9),
    ("101121", 5.0),
    ("101200", 7.2),
    ("101201", 5.7),
    ("101210", 5.7),
    ("101211", 5.2),
    ("101220", 5.2),
    ("101221", 2.5),
    ("102001", 8.3),
    ("102011", 7.0),
    ("102021", 5.4),
    ("102101", 6.5),
    ("102111", 5.8),
    ("102121", 2.6),
    ("102201", 5.3),
    ("102211", 2.1),
    ("102221", 1.3),
    ("110000", 9.5),
    ("110001", 9.0),
    ("110010", 8.8),
    ("110011", 7.6),
    ("110020", 7.6),
    ("110021", 7.0),
    ("110100", 9.0),
    ("110101", 7.7),
    ("110110", 7.5),
    ("110111", 6.2),
    ("110120", 6.1),
    ("110121", 5.3),
    ("110200", 7.7),
    ("110201", 6.6),
    ("110210", 6.8),
    ("110211", 5.9),
    ("110220", 5.2),
    ("110221", 3.0),
    ("111000", 8.9),
    ("111001", 7.8),
    ("111010", 7.6),
    ("111011", 6.7),
    ("111020", 6.2),
    ("111021", 5.8),
    ("111100", 7.4),
    ("111101", 5.9),
    ("111110", 5.7),
    ("111111", 5.7),
    ("111120", 4.7),
    ("111121", 2.3),
    ("111200", 6.1),
    ("111201", 5.2),
    ("111210", 5.7),
    ("111211", 2.9),
    ("111220", 2.4),
    ("111221", 1.6),
    ("112001", 7.1),
    ("112011", 5.9),
    ("112021", 3.0),
    ("112101", 5.8),
    ("112111", 2.6),
    ("112121", 1.5),
    ("112201", 2.3),
    ("112211", 1.3),
    ("112221", 0.6),
    ("200000", 9.3),
    ("200001", 8.7),
    ("200010", 8.6),
    ("200011", 7.2),
    ("200020", 7.5),
    ("200021", 5.8),
    ("200100", 8.6),
    ("200101", 7.4),
    ("200110", 7.4),
    ("200111", 6.1),
    ("200120", 5.6),
    ("200121", 3.4),
    ("200200", 7.0),
    ("200201", 5.4),
    ("200210", 5.2),
    ("200211", 4.0),
    ("200220", 4.0),
    ("200221", 2.2),
    ("201000", 8.5),
    ("201001", 7.5),
    ("201010", 7.4),
    ("201011", 5.5),
    ("201020", 6.2),
    ("201021", 5.1),
    ("201100", 7.2),
    ("201101", 5.7),
    ("201110", 5.5),
    ("201111", 4.1),
    ("201120", 4.6),
    ("201121", 1.9),
    ("201200", 5.3),
    ("201201", 3.6),
    ("201210", 3.4),
    ("201211", 1.9),
    ("201220", 1.9),
    ("201221", 0.8),
    ("202001", 6.4),
    ("202011", 5.1),
    ("202021", 2.0),
    ("202101", 4.7),
    ("202111", 2.1),
    ("202121", 1.1),
    ("202201", 2.4),
    ("202211", 0.9),
    ("202221", 0.4),
    ("210000", 8.8),
    ("210001", 7.5),
    ("210010", 7.3),
    ("210011", 5.3),
    ("210020", 6.0),
    ("210021", 5.0),
    ("210100", 7.3),
    ("210101", 5.5),
    ("210110", 5.9),
    ("210111", 4.0),
    ("210120", 4.1),
    ("210121", 2.0),
    ("210200", 5.4),
    ("210201", 4.3),
    ("210210", 4.5),
    ("210211", 2.2),
    ("210220", 2.0),
    ("210221", 1.1),
    ("211000", 7.5),
    ("211001", 5.5),
    ("211010", 5.8),
    ("211011", 4.5),
    ("211020", 4.0),
    ("211021", 2.1),
    ("211100", 6.1),
    ("211101", 5.1),
    ("211110", 4.8),
    ("211111", 1.8),
    ("211120", 2.0),
    ("211121", 0.9),
    ("211200", 4.6),
    ("211201", 1.8),
    ("211210", 1.7),
    ("211211", 0.7),
    ("211220", 0.8),
    ("211221", 0.2),
    ("212001", 5.3),
    ("212011", 2.4),
    ("212021", 1.4),
    ("212101", 2.4),
    ("212111", 1.2),
    ("212121", 0.5),
    ("212201", 1.0),
    ("212211", 0.3),
    ("212221", 0.1),
];

/// Maximal vectors per EQ level, from the reference calculator's
/// `max_composed.js`, as level indices.
///
/// Each EQ contributes a disjoint set of metrics, so these are held structured
/// rather than as vector strings the scorer would have to re-parse.
///
/// EQ5 is omitted deliberately. It contributes only `E`, which none of the
/// severity distances read, so including it would multiply the search without
/// ever changing which combination is selected first.
mod maxes {
    /// Level indices for `VC`, `VI`, `VA`, `CR`, `IR`, `AR`.
    pub type ImpactMax = (u8, u8, u8, u8, u8, u8);

    /// (AV, PR, UI)
    pub static EQ1: [&[(u8, u8, u8)]; 3] = [
        &[(0, 0, 0)],
        &[(1, 0, 0), (0, 1, 0), (0, 0, 1)],
        &[(3, 0, 0), (1, 1, 1)],
    ];

    /// (AC, AT)
    pub static EQ2: [&[(u8, u8)]; 2] = [&[(0, 0)], &[(1, 0), (0, 1)]];

    /// (VC, VI, VA, CR, IR, AR), indexed by EQ3 then EQ6. The EQ3=2/EQ6=0 cell
    /// is empty because that combination cannot occur: if no impact is High,
    /// no requirement can pair with a High impact.
    pub static EQ3_EQ6: [[&[ImpactMax]; 2]; 3] = [
        [
            &[(0, 0, 0, 0, 0, 0)],
            &[(0, 0, 1, 1, 1, 0), (0, 0, 0, 1, 1, 1)],
        ],
        [
            &[(1, 0, 0, 0, 0, 0), (0, 1, 0, 0, 0, 0)],
            &[
                (1, 0, 1, 0, 1, 0),
                (1, 0, 0, 0, 1, 1),
                (0, 1, 0, 1, 0, 1),
                (0, 1, 1, 1, 0, 0),
                (1, 1, 0, 0, 0, 1),
            ],
        ],
        [&[], &[(1, 1, 1, 0, 0, 0)]],
    ];

    /// (SC, SI, SA)
    pub static EQ4: [&[(u8, u8, u8)]; 3] = [&[(1, 0, 0)], &[(1, 1, 1)], &[(2, 2, 2)]];
}

/// Maximal severity distance within each EQ level, in tenths, from the
/// reference calculator's `max_severity.js`.
mod max_severity {
    pub static EQ1: [u32; 3] = [1, 4, 5];
    pub static EQ2: [u32; 2] = [1, 2];
    pub static EQ3_EQ6: [[u32; 2]; 3] = [[7, 6], [8, 8], [0, 10]];
    pub static EQ4: [u32; 3] = [6, 5, 4];
}

/// The severity weight of a metric level.
///
/// Every `*_levels` table in the reference implementation is some prefix or
/// suffix of this one, so a single table indexed by level serves all of them.
///
/// These are read as literals rather than computed as `index as f64 * 0.1`,
/// which is not the same number: `3.0 * 0.1` is 0.30000000000000004 while the
/// literal `0.3` is 0.29999999999999999. The scoring procedure sums these
/// weights and then rounds to one decimal place, so a difference in the last
/// bits decides scores that land on a `.x5` boundary.
fn weight(level: u8) -> f64 {
    const LEVELS: [f64; 4] = [0.0, 0.1, 0.2, 0.3];
    LEVELS[level as usize]
}

/// One parsed base vector, as level indices.
///
/// Every metric is stored as its position in the published severity ordering,
/// least severe first. That is exactly the ordering of the `*_levels` tables of
/// the reference implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BaseVector {
    av: u8,
    ac: u8,
    at: u8,
    pr: u8,
    ui: u8,
    vc: u8,
    vi: u8,
    va: u8,
    sc: u8,
    si: u8,
    sa: u8,
}

fn level(value: &str, table: &[(&str, u8)]) -> Option<u8> {
    table
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, level)| *level)
}

/// Parse a `CVSS:4.0/...` vector string into its base metrics.
///
/// Returns `None` for anything that is not a well-formed v4 vector carrying
/// every base metric. A caller that gets `None` must report an unknown severity
/// rather than substituting a number.
fn parse(vector: &str) -> Option<BaseVector> {
    const AV: &[(&str, u8)] = &[("N", 0), ("A", 1), ("L", 2), ("P", 3)];
    const AC: &[(&str, u8)] = &[("L", 0), ("H", 1)];
    const AT: &[(&str, u8)] = &[("N", 0), ("P", 1)];
    const PR: &[(&str, u8)] = &[("N", 0), ("L", 1), ("H", 2)];
    const UI: &[(&str, u8)] = &[("N", 0), ("P", 1), ("A", 2)];
    const IMPACT: &[(&str, u8)] = &[("H", 0), ("L", 1), ("N", 2)];
    const SC: &[(&str, u8)] = &[("H", 1), ("L", 2), ("N", 3)];
    const SI_SA: &[(&str, u8)] = &[("S", 0), ("H", 1), ("L", 2), ("N", 3)];

    let mut parts = vector.trim().split('/');
    if parts.next()? != "CVSS:4.0" {
        return None;
    }

    let mut found: Vec<(&str, &str)> = Vec::new();
    for part in parts {
        // A separator with nothing after it, which a trailing `/` produces.
        // Tolerated rather than fatal, matching how the v3 reader treats the
        // same vectors: every base metric is still required below, so this
        // loosens the shape of the string and not what has to be in it.
        if part.trim().is_empty() {
            continue;
        }
        let (key, value) = part.split_once(':')?;
        found.push((key.trim(), value.trim()));
    }
    let get = |key: &str| -> Option<&str> {
        found
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    };

    Some(BaseVector {
        av: level(get("AV")?, AV)?,
        ac: level(get("AC")?, AC)?,
        at: level(get("AT")?, AT)?,
        pr: level(get("PR")?, PR)?,
        ui: level(get("UI")?, UI)?,
        vc: level(get("VC")?, IMPACT)?,
        vi: level(get("VI")?, IMPACT)?,
        va: level(get("VA")?, IMPACT)?,
        sc: level(get("SC")?, SC)?,
        si: level(get("SI")?, SI_SA)?,
        sa: level(get("SA")?, SI_SA)?,
    })
}

impl BaseVector {
    /// The six equivalence-class levels that identify this vector's macrovector.
    ///
    /// EQ5 is always 0 and EQ6 always assumes High requirements, because a base
    /// score takes the specified defaults for the threat and environmental
    /// metrics that would otherwise move them.
    fn macrovector(&self) -> [u8; 6] {
        let eq1 = if self.av == 0 && self.pr == 0 && self.ui == 0 {
            0
        } else if (self.av == 0 || self.pr == 0 || self.ui == 0) && self.av != 3 {
            1
        } else {
            2
        };

        let eq2 = u8::from(!(self.ac == 0 && self.at == 0));

        let eq3 = if self.vc == 0 && self.vi == 0 {
            0
        } else if self.vc == 0 || self.vi == 0 || self.va == 0 {
            1
        } else {
            2
        };

        // EQ4 level 0 needs MSI:S or MSA:S, which a base vector never carries.
        let eq4 = if self.sc == 1 || self.si == 1 || self.sa == 1 {
            1
        } else {
            2
        };

        // CR, IR and AR all default to High, so EQ6 turns on whether any impact
        // is High.
        let eq6 = u8::from(!(self.vc == 0 || self.vi == 0 || self.va == 0));

        [eq1, eq2, eq3, eq4, 0, eq6]
    }
}

fn lookup(macrovector: [u8; 6]) -> Option<f64> {
    let key: String = macrovector
        .iter()
        .map(|level| (b'0' + level) as char)
        .collect();
    MACROVECTOR_SCORES
        .binary_search_by(|(candidate, _)| (*candidate).cmp(key.as_str()))
        .ok()
        .map(|index| MACROVECTOR_SCORES[index].1)
}

/// Compute the CVSS v4.0 base score for a vector string.
///
/// Returns `None` when the vector is not a parseable v4 vector carrying every
/// base metric, so that an unreadable vector reports an unknown severity rather
/// than a fabricated number.
pub fn cvss_v4_base_score(vector: &str) -> Option<f64> {
    let base = parse(vector)?;

    // A vulnerability with no impact on anything scores zero without consulting
    // the table, per the specification.
    if base.vc == 2 && base.vi == 2 && base.va == 2 && base.sc == 3 && base.si == 3 && base.sa == 3
    {
        return Some(0.0);
    }

    let macrovector = base.macrovector();
    let [eq1, eq2, eq3, eq4, eq5, eq6] = macrovector;
    let value = lookup(macrovector)?;

    // The next lower macrovector in each equivalence class, which may not exist.
    // EQ3 and EQ6 move together: from (0,0) either coordinate may step, and the
    // better-scoring of the two is used.
    let lower_eq1 = lookup([eq1 + 1, eq2, eq3, eq4, eq5, eq6]);
    let lower_eq2 = lookup([eq1, eq2 + 1, eq3, eq4, eq5, eq6]);
    let lower_eq3_eq6 = match (eq3, eq6) {
        (0, 0) => {
            let left = lookup([eq1, eq2, eq3, eq4, eq5, eq6 + 1]);
            let right = lookup([eq1, eq2, eq3 + 1, eq4, eq5, eq6]);
            match (left, right) {
                (Some(left_score), Some(right_score)) if left_score > right_score => left,
                _ => right,
            }
        }
        (_, 1) => lookup([eq1, eq2, eq3 + 1, eq4, eq5, eq6]),
        _ => lookup([eq1, eq2, eq3, eq4, eq5, eq6 + 1]),
    };
    let lower_eq4 = lookup([eq1, eq2, eq3, eq4 + 1, eq5, eq6]);
    let lower_eq5 = lookup([eq1, eq2, eq3, eq4, eq5 + 1, eq6]);

    // Find the first maximal vector this one is no more severe than in every
    // metric.
    let mut distances = None;
    'search: for &(max_av, max_pr, max_ui) in maxes::EQ1[eq1 as usize] {
        for &(max_ac, max_at) in maxes::EQ2[eq2 as usize] {
            for &(max_vc, max_vi, max_va, max_cr, max_ir, max_ar) in
                maxes::EQ3_EQ6[eq3 as usize][eq6 as usize]
            {
                for &(max_sc, max_si, max_sa) in maxes::EQ4[eq4 as usize] {
                    let candidate = [
                        weight(base.av) - weight(max_av),
                        weight(base.pr) - weight(max_pr),
                        weight(base.ui) - weight(max_ui),
                        weight(base.ac) - weight(max_ac),
                        weight(base.at) - weight(max_at),
                        weight(base.vc) - weight(max_vc),
                        weight(base.vi) - weight(max_vi),
                        weight(base.va) - weight(max_va),
                        weight(base.sc) - weight(max_sc),
                        weight(base.si) - weight(max_si),
                        weight(base.sa) - weight(max_sa),
                        // Requirements default to High, which is level 0.
                        -weight(max_cr),
                        -weight(max_ir),
                        -weight(max_ar),
                    ];
                    if candidate.iter().all(|distance| *distance >= 0.0) {
                        distances = Some(candidate);
                        break 'search;
                    }
                }
            }
        }
    }
    // The published maxima cover every reachable macrovector, so this is
    // unreachable for a well-formed vector; reporting nothing beats reporting a
    // number derived from a vector that was never selected.
    let distances = distances?;

    // Summed in the order the reference sums them, because floating-point
    // addition is not associative and the result is rounded to one decimal.
    let severity_eq1 = distances[0] + distances[1] + distances[2];
    let severity_eq2 = distances[3] + distances[4];
    let severity_eq3_eq6 =
        distances[5] + distances[6] + distances[7] + distances[11] + distances[12] + distances[13];
    let severity_eq4 = distances[8] + distances[9] + distances[10];

    // Each existing lower macrovector contributes the share of its scoring gap
    // that this vector has already travelled. EQ5 contributes a zero share but
    // still counts toward the mean, exactly as the reference does.
    const STEP: f64 = 0.1;
    let mut normalized = 0.0;
    let mut existing_lower = 0.0;
    let mut accumulate = |lower: Option<f64>, severity: f64, max: u32| {
        if let Some(lower) = lower {
            existing_lower += 1.0;
            if max > 0 {
                normalized += (value - lower) * (severity / (max as f64 * STEP));
            }
        }
    };
    accumulate(lower_eq1, severity_eq1, max_severity::EQ1[eq1 as usize]);
    accumulate(lower_eq2, severity_eq2, max_severity::EQ2[eq2 as usize]);
    accumulate(
        lower_eq3_eq6,
        severity_eq3_eq6,
        max_severity::EQ3_EQ6[eq3 as usize][eq6 as usize],
    );
    accumulate(lower_eq4, severity_eq4, max_severity::EQ4[eq4 as usize]);
    accumulate(lower_eq5, 0.0, 1);

    let mean = if existing_lower == 0.0 {
        0.0
    } else {
        normalized / existing_lower
    };

    Some(((value - mean).clamp(0.0, 10.0) * 10.0).round() / 10.0)
}
