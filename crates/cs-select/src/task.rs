//! Stage 1 — task parsing (ALGORITHM.md §4): deterministic tokenization, no
//! NLP dependencies.
//!
//! Downstream consumes [`ParsedTask`]: weighted `terms`, `hints` (symbols,
//! paths, verbatim), and the `test_bias`/`config_bias` modifier flags.
//!
//! Normalization note: ALGORITHM §4 names NFKC. The dependency freeze
//! (ADR-011 manifest discipline; the cs-select manifest may not grow a
//! unicode table crate) excludes `unicode-normalization`, so normalization
//! here is full Unicode lowercasing via `str::to_lowercase` only. ASCII
//! identifiers — the corpus the Go pipeline indexes — are unaffected; the
//! NFKC-only compatibility cases (ligatures, CJK compatibility forms) are a
//! known, documented gap, and whitespace-like compatibility characters are
//! irrelevant because they are separators, not term characters.

use std::collections::{BTreeMap, BTreeSet};

use crate::tuning;

/// A distinct search term: the lowercased form everything downstream matches
/// with, plus the original-case spellings the task used (ALGORITHM §4 keeps
/// the original for exact-case matching — S1's case-sensitive variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTerm {
    /// Lowercased term (the matching key).
    pub lower: String,
    /// Original-case spellings seen in the task, sorted.
    pub originals: BTreeSet<String>,
}

/// Hint extraction output (ALGORITHM §4 step 4), each list deduplicated and
/// sorted. Whole hints are never stopword-filtered — they are exact-match
/// material. Qualified-name split parts are filtered (a stopword member like
/// the `item` in `Module::item` would match every such symbol in the repo),
/// while the member itself always becomes a term (see [`parse_task`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hints {
    /// Qualified-name hints (`pkg.Symbol`, `Module::item`) — the whole and
    /// the split parts, both ways per ALGORITHM §4.
    pub symbols: Vec<String>,
    /// Path hints: tokens containing `/` or a source-code extension.
    pub paths: Vec<String>,
    /// Backticked `` `verbatim` `` — exact identifier or path terms.
    pub verbatim_ident: Vec<String>,
    /// `"quoted strings"` — verbatim content search terms (error messages).
    pub verbatim_content: Vec<String>,
}

/// Modifier flags tagged from task words (ALGORITHM §4 step 5). The words
/// themselves are modifier hints, never terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskFlags {
    /// Task words like *test, tests, flaky* — raises test-affinity weights
    /// in stages 3–4.
    pub test_bias: bool,
    /// Task words like *config, deploy, migration* — raises config-file
    /// signal weights.
    pub config_bias: bool,
}

/// Stage-1 output: everything stages 2–5 consume about the task.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedTask {
    /// Distinct terms, ascending by [`ParsedTerm::lower`].
    pub terms: Vec<ParsedTerm>,
    /// Extracted hints.
    pub hints: Hints,
    /// Modifier flags.
    pub flags: TaskFlags,
}

/// English function words plus the code-generic words ALGORITHM §4 names
/// (`file`, `files`, `code`, `repo`, `function`, `class`, `test`*, `bug`,
/// `fix`*, `add`, `change`, `make`, `update`…). `test`/`fix` families live
/// here as *modifier hints*, not terms; their inflections ride along.
/// `out`, `logged`, `users` are deliberately absent — the §11 worked trace
/// keeps them as terms. MUST stay sorted; `binary_search` correctness and a
/// unit test both depend on it.
const STOPWORDS: [&str; 184] = [
    "a",
    "about",
    "above",
    "add",
    "added",
    "adding",
    "adds",
    "after",
    "again",
    "all",
    "also",
    "always",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "auth",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "bug",
    "bugfix",
    "but",
    "by",
    "can",
    "change",
    "changes",
    "changing",
    "class",
    "classes",
    "cleanup",
    "code",
    "config",
    "could",
    "deploy",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "even",
    "few",
    "file",
    "files",
    "fix",
    "fixed",
    "fixes",
    "fixing",
    "flaky",
    "for",
    "from",
    "func",
    "function",
    "functions",
    "further",
    "get",
    "gets",
    "got",
    "had",
    "handler",
    "has",
    "have",
    "having",
    "help",
    "her",
    "here",
    "hers",
    "him",
    "his",
    "how",
    "if",
    "impl",
    "implement",
    "implements",
    "import",
    "in",
    "into",
    "is",
    "issue",
    "it",
    "item",
    "its",
    "itself",
    "just",
    "like",
    "make",
    "makes",
    "may",
    "me",
    "method",
    "methods",
    "might",
    "migration",
    "migrations",
    "more",
    "most",
    "much",
    "must",
    "my",
    "new",
    "no",
    "nor",
    "not",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "over",
    "own",
    "package",
    "please",
    "plus",
    "refactor",
    "refactoring",
    "repo",
    "repository",
    "same",
    "she",
    "should",
    "so",
    "some",
    "struct",
    "such",
    "test",
    "tests",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "there",
    "these",
    "they",
    "this",
    "those",
    "to",
    "too",
    "type",
    "types",
    "under",
    "until",
    "up",
    "upon",
    "us",
    "use",
    "used",
    "uses",
    "using",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "within",
    "without",
    "would",
    "you",
    "your",
    "yours",
];

/// Source extensions that make a token a path hint (ALGORITHM §4 step 4).
const SOURCE_EXTENSIONS: [&str; 8] = ["go", "ts", "tsx", "py", "js", "rs", "java", "cs"];

/// Parse free-form task text into terms, hints, and modifier flags.
///
/// Deterministic: identical input yields an identical [`ParsedTask`] on any
/// run — no randomness, no environment. Steps mirror ALGORITHM §4 in order;
/// two documented refinements are forced by the §11 worked trace: a term
/// whose first character is a digit is dropped (drops the trace's `60s`,
/// which is not "pure digits"), and a `_`/`-` token contributes its joined
/// form only when a part would otherwise be lost (`remember_me` keeps
/// `rememberme` for the too-short `me`; `auth-timeout` needs no
/// `authtimeout`).
#[must_use]
pub fn parse_task(task: &str) -> ParsedTask {
    let mut lowers: BTreeMap<String, ParsedTerm> = BTreeMap::new();
    let mut hints = Hints::default();
    let mut flags = TaskFlags::default();

    let (verbatim_ident, verbatim_content) = extract_verbatim_spans(task);
    hints.verbatim_ident = dedup_sorted(verbatim_ident);
    hints.verbatim_content = dedup_sorted(verbatim_content);

    for word in task.split_whitespace() {
        let cleaned = strip_line_suffix(trim_token_edges(word));
        if cleaned.is_empty() {
            continue;
        }

        // Modifier words are hints, never terms (ALGORITHM §4 step 3/5).
        let lower_word = cleaned.to_lowercase();
        let _ = tag_modifier(&lower_word, &mut flags);

        if cleaned.contains('/') || has_source_extension(cleaned) {
            hints.paths.push(cleaned.to_owned());
            // Path segments are words: `internal` and `session` are terms,
            // code-generic `auth`/`handler` fall out as stopwords.
            collect_terms(cleaned, &mut lowers, &mut flags, true);
        } else if let Some(parts) = qualified_parts(cleaned) {
            hints.symbols.push(cleaned.to_owned());
            for part in &parts {
                if !is_stopword(&part.to_lowercase()) {
                    hints.symbols.push(part.clone());
                }
            }
            // The member (last part) is always a term: the user typed this
            // exact qualified name, so it is signal by construction even
            // when the word itself is stopword-listed. Precision lives in
            // hints, recall in terms. Modifier words stay flags, never
            // terms — enforced inside `push_identifier_piece`, same rule
            // as plain tokens. Scopes (`Module`, `pkg`) contribute no
            // terms; the raw-token split below is skipped for qualified
            // tokens for exactly that reason.
            if let Some(member) = parts.last() {
                push_identifier_piece(member, &mut lowers, &mut flags);
            }
        } else {
            collect_terms(cleaned, &mut lowers, &mut flags, false);
        }
    }

    hints.symbols = dedup_sorted(hints.symbols);
    hints.paths = dedup_sorted(hints.paths);

    ParsedTask {
        terms: lowers.into_values().collect(),
        hints,
        flags,
    }
}

/// Tag modifier words as flags (ALGORITHM §4 step 5). Returns `true` when
/// the word was a modifier — callers skip term collection for those.
#[must_use]
fn tag_modifier(lower_word: &str, flags: &mut TaskFlags) -> bool {
    match lower_word {
        "test" | "tests" | "flaky" => {
            flags.test_bias = true;
            true
        }
        "config" | "deploy" | "migration" | "migrations" => {
            flags.config_bias = true;
            true
        }
        _ => false,
    }
}
/// `true` when the word is on the stopword list.
#[must_use]
pub fn is_stopword(lower: &str) -> bool {
    STOPWORDS.binary_search(&lower).is_ok()
}

/// Split an alphanumeric run at camelCase boundaries (`authTimeout` →
/// `auth`, `timeout`; `HTTPServer` → `HTTP`, `server` piece set). Original
/// case is preserved; callers lowercase for matching.
#[must_use]
pub fn split_camel(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    for i in 1..chars.len() {
        let c = chars[i];
        let prev = chars[i - 1];
        let boundary = (prev.is_lowercase() || prev.is_numeric()) && c.is_uppercase()
            || prev.is_uppercase()
                && c.is_uppercase()
                && chars.get(i + 1).is_some_and(|c| c.is_lowercase());
        if boundary {
            parts.push(chars[start..i].iter().collect());
            start = i;
        }
    }
    if start < chars.len() {
        parts.push(chars[start..].iter().collect());
    }
    parts
}

/// Gather terms from one cleaned token: split on non-alphanumerics, camelCase
/// boundaries, and — when a part would otherwise be lost — the joined form
/// (`remember_me` → `rememberme`, ALGORITHM §11's worked trace).
///
/// Two filters keep the term set tight. Whole-word tokens pass the stopword
/// list, but identifier fragments (camelCase pieces, qualified members,
/// `_`/`-` runs inside a compound token) skip it: a fragment like `auth` in
/// `auth-timeout` is S1/S4 signal, and filtering English function words out
/// of identifiers would blind exact matching. Path-hint tokens go the other
/// way — their segments are words (`internal`, `session` are terms;
/// code-generic `auth`, `handler` are stopwords) — so callers pass
/// `filter_words` for those. The joined form is contributed only when some
/// run was dropped by the shape rules below: `remember-me` keeps
/// `rememberme` because `me` is too short, while `auth-timeout` needs no
/// `authtimeout` since both parts survive. Stopword-dropped parts never
/// trigger the join — resurrecting filtered words would defeat filtering.
fn collect_terms(
    token: &str,
    lowers: &mut BTreeMap<String, ParsedTerm>,
    flags: &mut TaskFlags,
    filter_words: bool,
) {
    let runs: Vec<&str> = token.split(|c: char| !c.is_alphanumeric()).collect();
    for run in &runs {
        let pieces = split_camel(run);
        let whole_word = filter_words || (runs.len() == 1 && pieces.len() == 1);
        for piece in &pieces {
            if whole_word {
                push_term(piece, piece, lowers);
            } else {
                push_identifier_piece(piece, lowers, flags);
            }
        }
    }
    let joinable = runs.iter().all(|r| !r.is_empty())
        && (token.contains('_') || token.contains('-'))
        && runs.len() > 1;
    if joinable && runs.iter().any(|r| shape_dropped(r)) {
        let joined: String = runs.concat();
        // The join can spell a modifier word from innocent parts
        // (`tes-t` → `test`); flags win over terms here as everywhere.
        if !tag_modifier(&joined.to_lowercase(), flags) {
            push_term(&joined, token, lowers);
        }
    }
}

/// `true` when a piece fails the shape rules: empty, digit-led, all digits,
/// or shorter than [`tuning::MIN_TERM_LEN`]. Stopwords are a separate,
/// later filter.
fn shape_dropped(piece: &str) -> bool {
    let mut chars = piece.chars();
    match chars.next() {
        None => true,
        Some(first) => {
            first.is_numeric()
                || piece.chars().all(char::is_numeric)
                || piece.chars().count() < tuning::MIN_TERM_LEN
        }
    }
}

/// Record one term under its lowercase key, keeping the original spelling.
fn insert_term(lower: String, original: &str, lowers: &mut BTreeMap<String, ParsedTerm>) {
    lowers
        .entry(lower.clone())
        .or_insert_with(|| ParsedTerm {
            lower,
            originals: BTreeSet::new(),
        })
        .originals
        .insert(original.to_owned());
}

/// Apply the term filters (ALGORITHM §4 step 3) and record the term in both
/// lower and original-case form. `original` is usually the piece itself;
/// the joined form passes the raw token so `remember_me` remembers its
/// `snake_case` spelling for exact-case matching.
fn push_term(piece: &str, original: &str, lowers: &mut BTreeMap<String, ParsedTerm>) {
    if shape_dropped(piece) {
        return;
    }
    let lower = piece.to_lowercase();
    if is_stopword(&lower) {
        return;
    }
    insert_term(lower, original, lowers);
}

/// Record an identifier fragment as a term without the stopword rule.
/// Qualified members and camelCase pieces are signal by construction (see
/// [`collect_terms`]); shape rules still apply, and modifier words stay
/// flags — a `FlakyTest` sets `test_bias` instead of leaking `flaky`.
fn push_identifier_piece(
    piece: &str,
    lowers: &mut BTreeMap<String, ParsedTerm>,
    flags: &mut TaskFlags,
) {
    if tag_modifier(&piece.to_lowercase(), flags) {
        return;
    }
    if shape_dropped(piece) {
        return;
    }
    insert_term(piece.to_lowercase(), piece, lowers);
}

/// Trim punctuation/quote characters from both ends of a whitespace token,
/// keeping interior characters (and `/`) intact.
fn trim_token_edges(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/')
}

/// Strip a trailing `:42` line suffix — a stack-trace frame is a path plus a
/// line (ALGORITHM §10 last row; ADR-023 D1).
fn strip_line_suffix(token: &str) -> &str {
    match token.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => path,
        _ => token,
    }
}

/// `true` when the token ends in a known source extension
/// (`\.(go|ts|tsx|py|js|rs|java|cs)$`).
fn has_source_extension(token: &str) -> bool {
    token
        .rsplit_once('.')
        .is_some_and(|(_, ext)| SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// Qualified-name shape check (`pkg.Symbol`, `Module::item`): at least two
/// `.`/`:`-separated parts, each starting with a letter and at least two
/// characters. Returns the split parts.
fn qualified_parts(token: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = token.split(['.', ':']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let well_formed = parts
        .iter()
        .all(|p| p.chars().count() >= 2 && p.chars().next().is_some_and(char::is_alphabetic));
    well_formed.then(|| parts.iter().map(|p| (*p).to_owned()).collect())
}

/// Extract `` `backticked` `` and `"quoted"` spans verbatim (unterminated
/// spans run to the end of the task; contents are trimmed).
fn extract_verbatim_spans(task: &str) -> (Vec<String>, Vec<String>) {
    let mut ident = Vec::new();
    let mut content = Vec::new();
    let mut rest = task;
    while let Some(pos) = rest.find(['`', '"']) {
        let open = rest[pos..].chars().next().unwrap_or('`');
        let body = &rest[pos + open.len_utf8()..];
        let Some(close) = body.find(open) else {
            let span = body.trim();
            if !span.is_empty() {
                if open == '`' {
                    ident.push(span.to_owned());
                } else {
                    content.push(span.to_owned());
                }
            }
            break;
        };
        let span = body[..close].trim();
        if !span.is_empty() {
            if open == '`' {
                ident.push(span.to_owned());
            } else {
                content.push(span.to_owned());
            }
        }
        rest = &body[close + open.len_utf8()..];
    }
    (ident, content)
}

fn dedup_sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items.dedup();
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lowers(parsed: &ParsedTask) -> Vec<&str> {
        parsed.terms.iter().map(|t| t.lower.as_str()).collect()
    }

    #[test]
    fn stopword_table_is_sorted_for_binary_search() {
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(STOPWORDS.to_vec(), sorted, "STOPWORDS must stay sorted");
        assert!(
            (100..=200).contains(&STOPWORDS.len()),
            "~120 words per ALGORITHM §4 plus deliberate inflections, got {}",
            STOPWORDS.len()
        );
        for kept in [
            "out",
            "logged",
            "users",
            "remember",
            "authentication",
            "timeout",
        ] {
            assert!(
                !is_stopword(kept),
                "{kept} is a term in the §11 worked trace"
            );
        }
    }

    #[test]
    fn worked_trace_task_yields_the_specified_terms() {
        // ALGORITHM §11 stage 1: "Fix the authentication timeout — users get
        // logged out after 60s even with remember-me" ⇒ terms = {authentication,
        // timeout, users, logged, out, remember, rememberme}; fix/get/even/
        // with dropped as stopwords, 60s by the first-char-digit rule.
        let parsed = parse_task(
            "Fix the authentication timeout — users get logged out after 60s even with remember-me",
        );
        assert_eq!(
            lowers(&parsed),
            vec![
                "authentication",
                "logged",
                "out",
                "remember",
                "rememberme",
                "timeout",
                "users"
            ]
        );
        assert!(!parsed.flags.test_bias);
        assert!(!parsed.flags.config_bias);
        assert!(parsed.hints.paths.is_empty());
        let remember = parsed
            .terms
            .iter()
            .find(|t| t.lower == "rememberme")
            .expect("joined form is a term");
        assert_eq!(
            remember.originals.iter().next().map(String::as_str),
            Some("remember-me"),
            "original spelling is preserved for exact-case matching"
        );
    }

    #[test]
    fn camel_case_splits_on_identifier_boundaries() {
        let parsed = parse_task("parse the authTimeout and HTTPServer plus parseHTMLString");
        assert_eq!(
            lowers(&parsed),
            vec!["auth", "html", "http", "parse", "server", "string", "timeout"]
        );
        // camelCase does not produce joined forms — only `_`/`-` do.
        assert!(!lowers(&parsed).contains(&"authtimeout"));
        let auth = parsed.terms.iter().find(|t| t.lower == "auth").unwrap();
        assert_eq!(
            auth.originals
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["auth"],
            "the split piece's original spelling is kept"
        );
    }

    #[test]
    fn snake_and_kebab_tokens_yield_parts_and_joined_form() {
        let parsed = parse_task("remember_me and auth-timeout");
        assert_eq!(
            lowers(&parsed),
            vec!["auth", "remember", "rememberme", "timeout"]
        );
        assert!(!lowers(&parsed).contains(&"and"), "\"and\" is a stopword");
        let remember = parsed
            .terms
            .iter()
            .find(|t| t.lower == "rememberme")
            .expect("joined form is a term");
        assert_eq!(
            remember.originals.iter().next().map(String::as_str),
            Some("remember_me"),
            "the joined form remembers the snake_case spelling"
        );
    }

    #[test]
    fn short_pure_digit_and_leading_digit_terms_are_dropped() {
        let parsed = parse_task("ab 123 60s x1 valid");
        assert_eq!(lowers(&parsed), vec!["valid"]);
    }

    #[test]
    fn backticks_and_quotes_become_verbatim_hints() {
        let parsed = parse_task("fix `Login` handling of \"connection refused\" now");
        assert_eq!(parsed.hints.verbatim_ident, vec!["Login"]);
        assert_eq!(parsed.hints.verbatim_content, vec!["connection refused"]);
        assert_eq!(
            lowers(&parsed),
            vec!["connection", "handling", "login", "now", "refused"]
        );
    }

    #[test]
    fn unterminated_verbatim_span_runs_to_end_of_task() {
        let parsed = parse_task("see `Session");
        assert_eq!(parsed.hints.verbatim_ident, vec!["Session"]);
    }

    #[test]
    fn path_hints_cover_slashes_and_source_extensions() {
        let parsed = parse_task("look at internal/auth/session.go then main.go and handler.ts");
        assert_eq!(
            parsed.hints.paths,
            vec!["handler.ts", "internal/auth/session.go", "main.go"]
        );
        // The segments are still ordinary terms; "go"/"ts" are too short.
        assert_eq!(
            lowers(&parsed),
            vec!["internal", "look", "main", "session", "then"]
        );
    }

    #[test]
    fn stack_trace_frame_is_a_path_hint_with_the_line_stripped() {
        let parsed = parse_task("panic in auth/session.go:42");
        assert_eq!(parsed.hints.paths, vec!["auth/session.go"]);
    }

    #[test]
    fn qualified_names_split_both_ways() {
        let parsed = parse_task("wire Module::item through pkg.Symbol");
        assert_eq!(
            parsed.hints.symbols,
            vec!["Module", "Module::item", "Symbol", "pkg", "pkg.Symbol"]
        );
        assert_eq!(lowers(&parsed), vec!["item", "symbol", "through", "wire"]);
    }

    #[test]
    fn numbers_and_abbreviations_are_not_qualified_hints() {
        let parsed = parse_task("value 3.14 vs e.g the rest");
        assert!(
            parsed.hints.symbols.is_empty(),
            "got {:?}",
            parsed.hints.symbols
        );
        assert!(parsed.hints.paths.is_empty());
    }

    #[test]
    fn modifier_words_set_flags_and_never_become_terms() {
        let parsed = parse_task("tests are flaky, check the deploy config and the migration");
        assert!(parsed.flags.test_bias);
        assert!(parsed.flags.config_bias);
        for banned in ["test", "tests", "flaky", "deploy", "config", "migration"] {
            assert!(
                !lowers(&parsed).contains(&banned),
                "{banned} leaked as a term"
            );
        }
    }

    #[test]
    fn identifier_fragments_keep_modifier_discipline() {
        // `FlakyTest`/`config_value` mention modifiers inside identifiers:
        // the flags fire, but the words never become terms — the same rule
        // as plain tokens, or `test_bias` would be set twice over.
        let parsed = parse_task("FlakyTest timeout in config_value");
        assert!(parsed.flags.test_bias);
        assert!(parsed.flags.config_bias);
        for banned in ["test", "tests", "flaky", "config"] {
            assert!(
                !lowers(&parsed).contains(&banned),
                "{banned} leaked as a term"
            );
        }
        assert_eq!(lowers(&parsed), vec!["timeout", "value"]);
    }

    #[test]
    fn joined_form_never_spells_a_modifier_word() {
        // `tes-t` joins to `test`: flags win over terms here as everywhere.
        let parsed = parse_task("tes-t timeout");
        assert!(!lowers(&parsed).contains(&"test"));
        assert_eq!(lowers(&parsed), vec!["tes", "timeout"]);
    }

    #[test]
    fn empty_and_punctuation_only_tasks_parse_to_something_valid() {
        assert_eq!(parse_task(""), ParsedTask::default());
        assert_eq!(parse_task("... !!! —"), ParsedTask::default());
    }

    #[test]
    fn parsing_is_deterministic_across_runs() {
        let task = "Fix the `Session.Validate` timeout in internal/auth/session.go";
        assert_eq!(parse_task(task), parse_task(task));
    }
}
