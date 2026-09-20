//! The tuning table (ALGORITHM.md §13, ADR-023 D6): every constant the
//! selection algorithm consumes, in one module, each citing its source.
//!
//! Changing any value requires a benchmark run before/after (recall@8k,
//! precision, tokens; ALGORITHM.md §13) — never vibes. Constants are plain
//! `pub const`s on purpose: the workspace lint exception notes that the spec
//! specifies literal f64 scoring arithmetic and the table reads best as a
//! named list, not a struct of knobs.

/// Saturation κ of the seed and squash curves `x/(x+κ)` (ALGORITHM §5, §6).
pub const KAPPA: f64 = 2.0;

/// Seed floor: files below this seed are not frontier seeds; no seeds at all
/// means map mode (ALGORITHM §5, §6 `frontier₀`).
pub const SEED_FLOOR: f64 = 0.05;

// --- Stage 1 — task parsing (ALGORITHM §4) ---

/// Terms shorter than this are dropped (ALGORITHM §4 step 3).
pub const MIN_TERM_LEN: usize = 3;

/// Default `--tokens` budget (ALGORITHM §2 inputs).
pub const DEFAULT_BUDGET_TOKENS: u64 = 16_000;

// --- Stage 2 — seeding signal weights (ALGORITHM §5 table) ---

/// S1 exact symbol-name match weight.
pub const S1_EXACT_SYMBOL_WEIGHT: f64 = 3.0;
/// S2 basename stem match weight.
pub const S2_BASENAME_WEIGHT: f64 = 2.0;
/// S3 path-segment match weight.
pub const S3_PATH_SEGMENT_WEIGHT: f64 = 1.0;
/// S4 BM25 over symbol names weight.
pub const S4_BM25_WEIGHT: f64 = 1.0;
/// S5 identifier-term density in content weight.
pub const S5_CONTENT_DENSITY_WEIGHT: f64 = 0.5;
/// S6 doc/comment term match weight.
pub const S6_DOC_TERM_WEIGHT: f64 = 0.5;
/// S7 git recency weight — fixed at 0 until cs-git lands (step 12, ADR-023
/// D1), so `--no-git` and "no git" are the same code path from day one.
pub const S7_GIT_RECENCY_WEIGHT: f64 = 0.0;
/// S8 config-file affinity weight.
pub const S8_CONFIG_AFFINITY_WEIGHT: f64 = 1.0;
/// S9 forced includes (`--include`) weight — user intent trumps everything.
pub const S9_FORCED_INCLUDE_WEIGHT: f64 = 10.0;

// --- Stage 2 — per-signal values (ALGORITHM §5 table) ---

/// S1 case-sensitive match value.
pub const S1_CASE_SENSITIVE_VALUE: f64 = 1.0;
/// S1 case-insensitive match value.
pub const S1_CASE_INSENSITIVE_VALUE: f64 = 0.7;
/// S2 exact stem match value.
pub const S2_STEM_EXACT_VALUE: f64 = 1.0;
/// S2 stem-contains-term value.
pub const S2_STEM_CONTAINS_VALUE: f64 = 0.6;
/// S3 value per matching path segment.
pub const S3_PER_SEGMENT_VALUE: f64 = 0.5;
/// S3 cap on the summed segment values.
pub const S3_SEGMENT_CAP: f64 = 1.0;
/// S4 normalization divisor: bm25 magnitude `b` becomes `b/(b+3)`
/// (ALGORITHM §5; the sign quirk is pinned in `crate::seed`'s tests, ADR-023
/// D3).
pub const S4_BM25_NORM_DIVISOR: f64 = 3.0;
/// S5 density divisor: `min(1, hits/8)`.
pub const S5_DENSITY_DIVISOR: f64 = 8.0;
/// S6 density divisor: `min(1, hits/4)`.
pub const S6_DENSITY_DIVISOR: f64 = 4.0;
/// S9 forced-include value.
pub const S9_FORCED_INCLUDE_VALUE: f64 = 1.0;

// --- Stage 2 — S5/S8 candidate and I/O caps ---

/// S5 content density is computed only for the top candidates by the S1–S4
/// union (ALGORITHM §5: "grep-prefiltered on candidate files only (top 2,000
/// by S1–S4 union), not the whole repo").
pub const S5_CANDIDATE_CAP: usize = 2_000;
/// v1 defensive cap (not in ALGORITHM.md): content read for S5/S8 density
/// stops after this many bytes per file. Eight hits saturate S5, so relevance
/// is decided long before the cap; the bound keeps an adversarially huge
/// candidate file from dominating selection time (ARCHITECTURE §4.6
/// "adversarial huge frontiers (caps)").
pub const CONTENT_SCAN_CAP_BYTES: u64 = 1024 * 1024;

// --- Stage 3 — graph propagation (ALGORITHM §6; ADR-023 D5) ---

/// Frontier cap per hop, files beyond it dropped by (score desc, path asc).
pub const FRONTIER_CAP: usize = 64;
/// Number of bounded walk hops.
pub const WALK_HOPS: u8 = 2;
/// Per-hop decay applied to propagated contributions.
pub const HOP_DECAY: f64 = 0.5;
/// Factor scaling the propagated score before the squash.
pub const PROP_FACTOR: f64 = 0.8;

/// `import_out` traversal weight (A imports B; reading B explains A).
pub const IMPORT_OUT_WEIGHT: f64 = 0.6;
/// `import_in` traversal weight (B imports A; blast radius), traversed
/// against the stored edge direction.
pub const IMPORT_IN_WEIGHT: f64 = 0.45;
/// `ref_def` base weight before the per-pair √-damping.
pub const REF_DEF_BASE_WEIGHT: f64 = 0.5;
/// `ref_def` damping constant: weight is `0.5·√(count/(count+8))`.
pub const REF_DEF_DAMPENER: f64 = 8.0;
/// `test_affinity` weight (bidirectional).
pub const TEST_AFFINITY_WEIGHT: f64 = 0.7;
/// `test_affinity` multiplier while the task carries `test_bias`.
pub const TEST_BIAS_MULTIPLIER: f64 = 1.25;

// --- Stage 4 — level assignment (ALGORITHM §7) ---

/// `total ≥ 0.75` assigns L5.
pub const LEVEL_L5_MIN: f64 = 0.75;
/// `total ≥ 0.50` assigns L3 (below the L5 band).
pub const LEVEL_L3_MIN: f64 = 0.50;
/// `total ≥ 0.30` assigns L2 (below the L3 band).
pub const LEVEL_L2_MIN: f64 = 0.30;
/// `total ≥ 0.15` assigns L1 (below the L2 band); under it, L0.
pub const LEVEL_L1_MIN: f64 = 0.15;
/// Likely-edit boost: files seeded via S1/S2 get `+0.1` total.
pub const LIKELY_EDIT_BOOST: f64 = 0.1;

// --- Stage 5 — budget fitting (ALGORITHM §8; ADR-023 D4) ---

/// Fixed overhead reserve numerator: 8% of budget, the top of ALGORITHM's
/// 2–8% band, frozen until measurement says otherwise (ADR-023 D4).
pub const OVERHEAD_RESERVE_NUM: u64 = 8;
/// Fixed overhead reserve denominator (percent scale).
pub const OVERHEAD_RESERVE_DEN: u64 = 100;
/// Promotion trigger numerator: promote while `Σcost ≤ budget − 5%`.
pub const PROMOTION_TRIGGER_NUM: u64 = 5;
/// Promotion trigger denominator (percent scale).
pub const PROMOTION_TRIGGER_DEN: u64 = 100;
/// Per-file render slack numerator: +15% over the exact token count,
/// covering render furniture (line numbers, anchors, elision markers) —
/// ADR-023 D4.
pub const RENDER_SLACK_NUM: u64 = 115;
/// Per-file render slack denominator.
pub const RENDER_SLACK_DEN: u64 = 100;

/// Demotion loss `level_import` values for the greedy ladder (ALGORITHM §8):
/// L5→L3 = 1.0, L3→L2 = 0.5, L2→L1 = 0.25, L1→L0 = 0.1. Indexed by the
/// level being demoted *from* (L4 is only reached by test promotion, whose
/// floor is L2 — it never demotes first).
pub const DEMOTION_LOSS: [f64; 6] = [0.0, 0.1, 0.25, 0.5, 0.5, 1.0];

/// The saturating sum `x/(x+κ)` (ALGORITHM §5, reused as the stage-3 squash).
///
/// One strong signal stays sufficient (x=2 ⇒ 0.5, x=6 ⇒ 0.75, x=18 ⇒ 0.9)
/// while a pile of weak ones cannot explode a file's seed.
#[must_use]
pub fn saturate(x: f64) -> f64 {
    x / (x + KAPPA)
}

#[cfg(test)]
// Exact float equality is the point here: these tests pin spec constants
// bit-for-bit, so any drift — however small — must fail loudly.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn saturation_matches_the_specified_curve() {
        let (kappa, floor) = (KAPPA, SEED_FLOOR);
        assert_eq!(kappa, 2.0, "ALGORITHM §5 κ = 2.0");
        assert_eq!(floor, 0.05, "ALGORITHM §5/§6 seed floor");
        assert_eq!(saturate(0.0), 0.0);
        assert_eq!(saturate(2.0), 0.5, "x=2 ⇒ seed 0.5 (ALGORITHM §5)");
        assert_eq!(saturate(6.0), 0.75, "x=6 ⇒ seed 0.75 (ALGORITHM §5)");
        assert_eq!(saturate(18.0), 0.9, "x=18 ⇒ seed 0.9 (ALGORITHM §5)");
        assert!(saturate(1_000.0) < 1.0, "saturating, never reaching 1.0");
    }

    #[test]
    fn level_bands_descend_in_spec_order() {
        assert_eq!(
            (LEVEL_L5_MIN, LEVEL_L3_MIN, LEVEL_L2_MIN, LEVEL_L1_MIN),
            (0.75, 0.5, 0.3, 0.15),
            "ALGORITHM §7 threshold bands, descending"
        );
    }

    #[test]
    fn walk_and_edge_weights_match_the_spec() {
        assert_eq!(FRONTIER_CAP, 64);
        assert_eq!(WALK_HOPS, 2);
        assert_eq!(HOP_DECAY, 0.5);
        assert_eq!(PROP_FACTOR, 0.8);
        assert_eq!(
            (IMPORT_OUT_WEIGHT, IMPORT_IN_WEIGHT),
            (0.6, 0.45),
            "asymmetric import weights (ALGORITHM §6)"
        );
        assert_eq!(REF_DEF_BASE_WEIGHT, 0.5);
        assert_eq!(REF_DEF_DAMPENER, 8.0);
        assert_eq!(TEST_AFFINITY_WEIGHT, 0.7);
        assert_eq!(TEST_BIAS_MULTIPLIER, 1.25);
    }

    #[test]
    fn budget_fitting_constants_match_the_spec() {
        assert_eq!(OVERHEAD_RESERVE_NUM, 8, "8% = top of the 2–8% band");
        assert_eq!(PROMOTION_TRIGGER_NUM, 5, "5% promotion trigger");
        assert_eq!(RENDER_SLACK_NUM, 115, "+15% per-file slack (ADR-023 D4)");
        assert_eq!(RENDER_SLACK_DEN, 100);
        assert_eq!(
            DEMOTION_LOSS,
            [0.0, 0.1, 0.25, 0.5, 0.5, 1.0],
            "level_import ladder (ALGORITHM §8)"
        );
    }

    #[test]
    fn git_recency_is_pinned_off_until_step_12() {
        assert_eq!(
            S7_GIT_RECENCY_WEIGHT, 0.0,
            "ADR-023 D1: S7 stays 0 until cs-git lands"
        );
    }
}
