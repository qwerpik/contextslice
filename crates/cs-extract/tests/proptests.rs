//! Property tests for Go extraction robustness (MASTER_PLAN §8.1).
//!
//! Two input strategies, one shared invariant set:
//!
//! 1. Arbitrary bytes (`0..4096` `u8`s), lossily decoded to UTF-8 — the
//!    "anything the filesystem can hold" class.
//! 2. Go-shaped mixtures: concatenations of pool pieces (keywords,
//!    operators, punctuation, emoji, NUL, backslash, arbitrary unicode
//!    characters) that land *near* real Go but never exactly on it.
//!
//! Invariants for every generated input:
//!
//! - **Total and infallible.** `extract(source, Language::Go)` never
//!   panics and never returns `Err`: for an adapter language, malformed
//!   source is *labeled* ([`cs_extract::ParseStatus::Partial`]) rather
//!   than failed; only impossible-to-parse conditions error (no adapter,
//!   no tree), and neither can occur for Go here (crate-level docs).
//! - **In-bounds spans.** Every def, ref and import span satisfies
//!   `start_byte <= end_byte <= source.len()`.
//! - **Documented ordering.** Defs are nondecreasing by
//!   `(start_byte, end_byte, name)`, refs by `(start_byte, end_byte,
//!   name)`, imports by `(start_byte, raw)` — in particular `start_byte`
//!   never decreases within any of the three vectors (the sort keys the
//!   Go adapter pins).
//!
//! Inputs are capped far below 64 KiB: `extract` itself imposes no size
//! cap (the scanner caps inputs upstream and cs-extract trusts callers),
//! but oversized random inputs would only spend wall-clock, not sharpen
//! the invariants.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use cs_extract::extract;
use cs_scanner::Language;

/// One span must sit inside the source: `start <= end <= source_len`.
fn check_span_in_bounds(
    what: &str,
    span: cs_extract::Span,
    source_len: usize,
) -> Result<(), TestCaseError> {
    if span.start_byte > span.end_byte {
        return Err(TestCaseError::fail(format!(
            "{what} span: start_byte {} exceeds end_byte {}",
            span.start_byte, span.end_byte
        )));
    }
    if usize::try_from(span.end_byte).is_ok_and(|end| end > source_len) {
        return Err(TestCaseError::fail(format!(
            "{what} span: end_byte {} exceeds {}-byte source",
            span.end_byte, source_len
        )));
    }
    Ok(())
}

/// The full invariant set for one generated source.
fn check_extraction(source: &str) -> Result<(), TestCaseError> {
    let file = match extract(source, Language::Go) {
        Ok(file) => file,
        Err(err) => {
            return Err(TestCaseError::fail(format!(
                "extract of {}-byte Go source errored (must never happen for an adapter \
                 language): {err}",
                source.len()
            )));
        }
    };

    for def in &file.defs {
        check_span_in_bounds("def", def.span, source.len())?;
    }
    for r in &file.refs {
        check_span_in_bounds("ref", r.span, source.len())?;
    }
    for imp in &file.imports {
        check_span_in_bounds("import", imp.span, source.len())?;
    }

    // Documented sort keys, nondecreasing over each vector.
    for pair in file.defs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let a_key = (a.span.start_byte, a.span.end_byte, &a.name);
        let b_key = (b.span.start_byte, b.span.end_byte, &b.name);
        if a_key > b_key {
            return Err(TestCaseError::fail(format!(
                "defs out of documented order: {a:?} appears before {b:?}"
            )));
        }
    }
    for pair in file.refs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let a_key = (a.span.start_byte, a.span.end_byte, &a.name);
        let b_key = (b.span.start_byte, b.span.end_byte, &b.name);
        if a_key > b_key {
            return Err(TestCaseError::fail(format!(
                "refs out of documented order: {a:?} appears before {b:?}"
            )));
        }
    }
    for pair in file.imports.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let a_key = (a.span.start_byte, &a.raw);
        let b_key = (b.span.start_byte, &b.raw);
        if a_key > b_key {
            return Err(TestCaseError::fail(format!(
                "imports out of documented order: {a:?} appears before {b:?}"
            )));
        }
    }
    Ok(())
}

/// A string assembled from Go-shaped pieces plus arbitrary unicode: near
/// real source, never exactly on it.
fn go_shaped_source() -> impl Strategy<Value = String> {
    /// Pieces a near-Go source is built from: keywords, operators,
    /// punctuation, emoji, NUL, backticks, backslashes.
    const POOL: &[&str] = &[
        "func ",
        "package p\n",
        "{}",
        ":=",
        "\"",
        "\n",
        "//",
        "f(",
        "x",
        "/*",
        "*/",
        "return",
        "struct {",
        "interface {}",
        "map[string]int",
        "\u{1F600}",
        "\u{0}",
        "`",
        "\\",
    ];
    let piece = prop_oneof![
        14 => proptest::sample::select(POOL).prop_map(str::to_owned).boxed(),
        6 => any::<char>().prop_map(|c| c.to_string()).boxed(),
    ];
    proptest::collection::vec(piece, 0..256).prop_map(|pieces| pieces.concat())
}

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 256,
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn arbitrary_bytes_never_error_and_uphold_span_and_order_invariants(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096usize),
    ) {
        let source = String::from_utf8_lossy(&bytes).into_owned();
        check_extraction(&source)?;
    }

    #[test]
    fn go_shaped_mixtures_never_error_and_uphold_span_and_order_invariants(
        source in go_shaped_source(),
    ) {
        check_extraction(&source)?;
    }
}
