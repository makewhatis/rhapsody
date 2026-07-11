//! Per-issue workspace directory-name derivation (`sanitize.go`, upstream §4.2, §9.5).

/// A per-issue filesystem workspace (upstream §4.1.4). The parity mirror of Go's `Workspace`:
/// `Path` (an absolute path string, kept as a `String` because the whole layer manipulates paths
/// lexically as Go's `path/filepath` does), `Key` (the sanitized identifier used as the directory
/// name), and `CreatedNow` (true only when the directory was created during this call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Absolute workspace path.
    pub path: String,
    /// Sanitized issue identifier (directory name).
    pub key: String,
    /// True only if the directory was created during this call.
    pub created_now: bool,
}

/// Reports whether `c` is permitted verbatim in a workspace directory name: the `[A-Za-z0-9._-]`
/// character class Go's `nonKeyChar` regexp is the negation of.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
}

/// Derives a workspace directory name from an issue identifier by replacing every character outside
/// `[A-Za-z0-9._-]` with `'_'` (upstream §4.2, §9.5). A sanitized result of `""`, `"."` or `".."`
/// is unsafe as a directory name (the latter two resolve to the workspace root or its parent under
/// a path join), so it is replaced with `"_"` to keep every key confined to its own subdirectory.
///
/// Mirrors Go's rune-wise `nonKeyChar.ReplaceAllString`: each disallowed Unicode scalar becomes a
/// single `'_'`.
pub fn sanitize_key(identifier: &str) -> String {
    let key: String = identifier
        .chars()
        .map(|c| if is_key_char(c) { c } else { '_' })
        .collect();
    match key.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror of TestSanitizeKey (sanitize_test.go).
    #[test]
    fn sanitize_key_cases() {
        let cases = [
            ("MT-649", "MT-649"),
            ("abc.def_ghi-1", "abc.def_ghi-1"),
            ("team/issue 1", "team_issue_1"),
            ("a@b#c$d", "a_b_c_d"),
            ("../escape", ".._escape"),
            // Results that would collapse to the root or its parent are replaced with a safe "_"
            // so Remove can never resolve to the workspace root.
            ("", "_"),
            (".", "_"),
            ("..", "_"),
            ("/", "_"), // sanitizes to "_" via the char class, not the dangerous "" path
        ];
        for (input, want) in cases {
            let got = sanitize_key(input);
            assert_eq!(got, want, "sanitize_key({input:?})");
        }
    }
}
