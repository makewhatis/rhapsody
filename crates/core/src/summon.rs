//! Summons matcher ported from Go `internal/core/summon.go`.

use regex::Regex;

/// The daemon's own summon token — the name Rhapsody ships under (STUDIO-603).
pub const SUMMON_TOKEN_RHAPSODY: &str = "@rhapsody";

/// The Go daemon's summon token, and still `decode`'s resolved default (that default is pinned by
/// the `api/config.json` parity golden and cannot be flipped here — see the STUDIO-603 PR body).
/// It stays accepted alongside [`SUMMON_TOKEN_RHAPSODY`] so an existing `@symphony` config, and any
/// in-flight `@symphony` comment, keeps summoning.
pub const SUMMON_TOKEN_SYMPHONY: &str = "@symphony";

/// Expands a configured summon token into the set of tokens that count as a summons.
///
/// The two brand spellings are synonyms OF EACH OTHER: configuring either accepts both, so a
/// daemon left on the shipped `@symphony` default answers to `@rhapsody`, and one configured
/// `@rhapsody` never misses an in-flight `@symphony` comment. A token that is neither brand is an
/// operator's deliberate choice and is matched VERBATIM — expanding it would make the daemon fire
/// on a differently-named bot's mentions, which is precisely what such a token is set to avoid.
pub fn summon_tokens(token: &str) -> Vec<&str> {
    // Case-insensitive, mirroring the `(?i)` the matcher itself is compiled with.
    if token.eq_ignore_ascii_case(SUMMON_TOKEN_RHAPSODY)
        || token.eq_ignore_ascii_case(SUMMON_TOKEN_SYMPHONY)
    {
        return vec![SUMMON_TOKEN_RHAPSODY, SUMMON_TOKEN_SYMPHONY];
    }
    vec![token]
}

/// The summons matcher the daemon actually runs: [`compile_summon_set`] over
/// [`summon_tokens(token)`](summon_tokens), so both brand spellings are accepted. This is the entry
/// point for the Linear comment path and the GitHub PR-comment path; [`compile_summon_re`] remains
/// the single-token Go-parity primitive underneath.
pub fn compile_summon_matcher(token: &str) -> Result<Regex, regex::Error> {
    compile_summon_set(&summon_tokens(token))
}

/// `compile_summon_re` builds the summons matcher for a token: it matches the token as a
/// standalone mention (preceded by start-or-whitespace, followed by end-or-non-word),
/// case-insensitive, so the token embedded in a URL / identifier / email never counts. Shared
/// by the Linear comment path and the GitHub PR-comment path so both match identically.
///
/// Go's source spells the boundaries with `\s` and `\w`, but Go's RE2 defines those as
/// ASCII-only (`\s == [\t\n\f\r ]`, `\w == [0-9A-Za-z_]`), whereas Rust's `regex` treats `\s`
/// and `\w` as Unicode by default. To keep matching byte-identical to the Go daemon, the
/// classes are written out explicitly here rather than as `\s`/`\w`.
///
/// Returns `Err` only if the pattern fails to compile, which cannot happen (the token is
/// escaped and the rest is a constant) — surfaced as a value rather than a `MustCompile`-style
/// panic to honor the crate's no-panic rule.
pub fn compile_summon_re(token: &str) -> Result<Regex, regex::Error> {
    compile_summon_set(&[token])
}

/// The set form of [`compile_summon_re`]: the same boundary rules with the tokens as an
/// alternation, so ANY of them counts as a standalone mention (STUDIO-603). A single-element set
/// compiles to the same language as [`compile_summon_re`]'s one-token pattern, keeping the Go
/// parity path byte-identical. Each token is `regex::escape`d, so an alternation metacharacter in
/// an operator's token is a literal. An empty set would match nothing useful, so it degrades to the
/// never-matching pattern rather than to "match everything".
pub fn compile_summon_set(tokens: &[&str]) -> Result<Regex, regex::Error> {
    if tokens.is_empty() {
        // `$.^` can never match; used only for the impossible empty-set call, never in production.
        return Regex::new(r"$.^");
    }
    let alt = tokens
        .iter()
        .map(|t| regex::escape(t))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(
        r"(?i)(?:^|[\t\n\f\r ])(?:{alt})(?:$|[^0-9A-Za-z_])"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `core.TestCompileSummonRe` (summon_test.go). The last two cases are parity
    // extensions beyond the Go test that lock in the ASCII-`\s`/`\w` decision documented on
    // `compile_summon_re`; both were confirmed against Go's RE2 engine (see the C1 PR body).
    #[test]
    fn compile_summon_re_matches_standalone_mentions() {
        let re = compile_summon_re("@symphony").expect("summon pattern compiles");
        let cases = [
            ("@symphony fix the CI error", true),
            ("hey @symphony please retry", true),
            ("trailing @symphony", true),
            ("see https://github.com/@symphony/repo for context", false), // embedded in URL
            ("email me at jp@symphony.dev", false),                       // embedded in identifier
            ("no mention here", false),
            // parity extensions (not in the Go test):
            ("@symphonyö", true), // trailing Unicode letter is a non-word boundary in Go's ASCII \w
            ("\u{00a0}@symphony", false), // leading non-breaking space is not Go's ASCII \s
        ];
        for (body, want) in cases {
            assert_eq!(re.is_match(body), want, "is_match({body:?})");
        }
    }

    // STUDIO-603: either brand spelling summons, whichever one is configured — so the shipped
    // `@symphony` default answers to `@rhapsody`, and an operator on `@rhapsody` never misses an
    // in-flight `@symphony` comment. The boundary rules are unchanged for both.
    #[test]
    fn brand_tokens_are_synonyms_in_both_directions() {
        for configured in ["@symphony", "@rhapsody", "@SYMPHONY", "@Rhapsody"] {
            let re = compile_summon_matcher(configured).expect("summon pattern compiles");
            let cases = [
                ("@symphony fix the CI error", true),
                ("@rhapsody fix the CI error", true),
                ("hey @RHAPSODY please retry", true), // still case-insensitive
                ("trailing @rhapsody", true),
                // the boundary rules still hold for the alias
                ("see https://github.com/@rhapsody/repo for context", false),
                ("email me at jp@rhapsody.dev", false),
                ("no mention here", false),
            ];
            for (body, want) in cases {
                assert_eq!(
                    re.is_match(body),
                    want,
                    "configured {configured:?}: is_match({body:?})"
                );
            }
        }
    }

    // A custom token is matched VERBATIM — it is NOT expanded to the brand pair, so a daemon
    // deliberately narrowed to `@bot` never fires on another bot's `@symphony` mentions.
    #[test]
    fn custom_token_is_not_expanded_to_the_brand_pair() {
        assert_eq!(summon_tokens("@bot"), vec!["@bot"]);
        let re = compile_summon_matcher("@bot").expect("summon pattern compiles");
        assert!(re.is_match("@bot please retry"));
        assert!(
            !re.is_match("@symphony please retry"),
            "a custom token must not answer to the brand tokens"
        );
        assert!(
            !re.is_match("@rhapsody please retry"),
            "a custom token must not answer to the brand tokens"
        );
    }

    // A token carrying a regex metacharacter stays a literal in the alternation (escape parity
    // with the single-token builder).
    #[test]
    fn set_escapes_alternation_metacharacters() {
        let re = compile_summon_set(&["@a|b", "@c"]).expect("summon pattern compiles");
        assert!(re.is_match("@a|b hello"), "the token is matched literally");
        assert!(!re.is_match("@a hello"), "`|` must not split the token");
        assert!(re.is_match("@c hello"));
    }
}
