//! Language detection: extension map and shebang patterns.
//!
//! Tier assignment follows LANGUAGES.md §1. Tier 1 (Go, TypeScript/JavaScript,
//! Python) have extraction adapters; everything else is detected so the pipeline
//! can label it honestly, but runs in heuristic mode (LANGUAGES.md §7).

use serde::{Deserialize, Serialize};

/// The language of a scanned file, as far as the scanner can tell.
///
/// This is a *label*, not a claim of parseability. `Language::Unknown` covers
/// both "no adapter and no recognized extension" and "recognized as a data
/// file"; downstream stages degrade rather than assume (ARCHITECTURE §11).
///
/// Serialized names are pinned explicitly instead of derived, because they are
/// persisted as `files.lang` values and appear in rendered headers — they are
/// part of the on-disk contract and must agree with [`Language::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    /// Go — Tier 1, the reference adapter (LANGUAGES.md §2.1).
    #[serde(rename = "go")]
    Go,
    /// TypeScript (`.ts`).
    #[serde(rename = "ts")]
    TypeScript,
    /// TSX (`.tsx`) — same grammar, JSX node types enabled.
    #[serde(rename = "tsx")]
    Tsx,
    /// JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`).
    #[serde(rename = "js")]
    JavaScript,
    /// Python — Tier 1 (LANGUAGES.md §2.3).
    #[serde(rename = "python")]
    Python,
    /// Recognized extension, no adapter yet (Tier 2 / Deferred).
    #[serde(rename = "unsupported")]
    Unsupported,
    /// No recognized extension or shebang.
    #[serde(rename = "unknown")]
    Unknown,
}

impl Language {
    /// The stable string used as the `files.lang` column value
    /// (ARCHITECTURE §5) and in rendered headers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::TypeScript => "ts",
            Self::Tsx => "tsx",
            Self::JavaScript => "js",
            Self::Python => "python",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// Whether an extraction adapter exists for this language (LANGUAGES.md §1).
    #[must_use]
    pub const fn has_adapter(self) -> bool {
        matches!(
            self,
            Self::Go | Self::TypeScript | Self::Tsx | Self::JavaScript | Self::Python
        )
    }
}

/// Map a repo-relative, `/`-separated path to a [`Language`].
///
/// Extension matching is case-insensitive on the final component only, so
/// `Makefile`-style names are not confused with `Foo.Go`. Extensionless names
/// with known conventions (e.g. `go.mod`) are handled explicitly.
#[must_use]
pub fn detect_language(path: &str) -> Language {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let lower_name = file_name.to_ascii_lowercase();

    // Exact-name conventions first: these files have no extension but are
    // unambiguous, and the resolver needs to find them (LANGUAGES.md §6).
    match lower_name.as_str() {
        "go.mod" | "go.sum" => return Language::Go,
        "pyproject.toml" | "setup.py" | "setup.cfg" | "pyrightconfig.json" => {
            return Language::Python;
        }
        "tsconfig.json" | "jsconfig.json" => return Language::TypeScript,
        _ => {}
    }

    // Extension match on the last dot only.
    let Some((_, ext)) = lower_name.rsplit_once('.') else {
        return Language::Unknown;
    };
    match ext {
        "go" => Language::Go,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "py" | "pyi" => Language::Python,
        // Recognized as source in Tier 2 / Deferred tiers (LANGUAGES.md §3–4).
        "rs" | "java" | "cs" | "c" | "h" | "cc" | "cpp" | "hpp" | "rb" | "php" | "swift" | "kt"
        | "scala" | "lua" | "sh" | "bash" | "zsh" => Language::Unsupported,
        // Data/config/markup are not languages (LANGUAGES.md §4); they stay
        // eligible for path and content signals only.
        _ => Language::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tier_one_extensions() {
        assert_eq!(detect_language("main.go"), Language::Go);
        assert_eq!(detect_language("src/app.ts"), Language::TypeScript);
        assert_eq!(detect_language("src/app.tsx"), Language::Tsx);
        assert_eq!(detect_language("src/app.js"), Language::JavaScript);
        assert_eq!(detect_language("src/app.mjs"), Language::JavaScript);
        assert_eq!(detect_language("pkg/mod.py"), Language::Python);
        assert_eq!(detect_language("pkg/mod.pyi"), Language::Python);
    }

    #[test]
    fn detection_uses_only_the_final_component() {
        // A directory named like an extension must not leak into the verdict.
        assert_eq!(detect_language("go/main.rs"), Language::Unsupported);
        assert_eq!(detect_language("a.b/c"), Language::Unknown);
    }

    #[test]
    fn recognizes_extensionless_project_files() {
        assert_eq!(detect_language("go.mod"), Language::Go);
        assert_eq!(detect_language("tsconfig.json"), Language::TypeScript);
        assert_eq!(detect_language("pyproject.toml"), Language::Python);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(detect_language("Main.GO"), Language::Go);
        assert_eq!(detect_language("App.TSX"), Language::Tsx);
    }

    #[test]
    fn classifies_other_languages_as_unsupported() {
        assert_eq!(detect_language("lib.rs"), Language::Unsupported);
        assert_eq!(detect_language("Main.java"), Language::Unsupported);
    }

    #[test]
    fn data_and_text_files_are_unknown() {
        assert_eq!(detect_language("README.md"), Language::Unknown);
        assert_eq!(detect_language("data.json"), Language::Unknown);
        assert_eq!(detect_language("Makefile"), Language::Unknown);
        assert_eq!(detect_language("no_extension"), Language::Unknown);
    }

    #[test]
    fn adapter_availability_matches_languages_md() {
        assert!(Language::Go.has_adapter());
        assert!(Language::Python.has_adapter());
        assert!(Language::TypeScript.has_adapter());
        assert!(!Language::Unsupported.has_adapter());
        assert!(!Language::Unknown.has_adapter());
    }

    #[test]
    fn language_strings_are_stable_and_lowercase() {
        // These strings are index column values and appear in rendered headers,
        // so they are part of the persisted contract (ARCHITECTURE §5).
        assert_eq!(Language::Go.as_str(), "go");
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::Tsx.as_str(), "tsx");
    }

    #[test]
    fn round_trips_through_serde() {
        let json = serde_json::to_string(&Language::TypeScript).expect("serialize");
        assert_eq!(json, "\"ts\"");
        let back: Language = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Language::TypeScript);
    }

    #[test]
    fn every_serde_name_agrees_with_as_str() {
        // These two spellings reach disk and stdout respectively; a divergence
        // would silently desynchronize the index from rendered headers.
        for language in [
            Language::Go,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Python,
            Language::Unsupported,
            Language::Unknown,
        ] {
            let json = serde_json::to_string(&language).expect("serialize");
            assert_eq!(json, format!("\"{}\"", language.as_str()));
        }
    }
}
