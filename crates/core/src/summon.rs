//! Summons matcher ported from Go `internal/core/summon.go`.

use regex::Regex;

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
    Regex::new(&format!(
        r"(?i)(?:^|[\t\n\f\r ]){}(?:$|[^0-9A-Za-z_])",
        regex::escape(token)
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
}
