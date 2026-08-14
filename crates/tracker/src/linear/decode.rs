//! Response-decoding helpers that reproduce Go `encoding/json`'s tolerance (STUDIO-406).
//!
//! The Go adapter decodes every Linear response with `encoding/json`, which has two behaviors this
//! adapter must match exactly, because Linear's schema declares far more fields nullable than a
//! response usually exercises:
//!
//!   * **A JSON `null` decoded into a `string` field yields the zero value `""`** — never an error.
//!     Rust's `String` REJECTS null, so every plain-`String` field in a response struct is a latent
//!     "invalid type: null, expected a string" that fails the WHOLE response. [`null_to_empty`] is
//!     the mirror; it is applied to every plain-`String` field in this module's response structs,
//!     not only the ones observed null in the wild.
//!   * A page of issues is decoded as a unit. [`IssueNodes`] deliberately DIVERGES from that: it
//!     decodes one node at a time so a single undecodable issue is dropped (and reported) rather
//!     than blanking its siblings. See its docs for why.

use super::RawIssue;
use serde::{Deserialize, Deserializer};

/// Deserializes a possibly-`null` GraphQL string into a `String` (null → `""`), the mirror of Go's
/// `encoding/json` decoding a JSON null into a `string` field as the zero value.
pub(super) fn null_to_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// One issue that failed to decode, kept for the caller to report.
#[derive(Debug, Clone)]
pub(super) struct DroppedNode {
    /// The issue's `identifier` (else its `id`, else `"<unidentified>"`) — recovered from the raw
    /// JSON so the warning can NAME the offending issue even though the node never decoded.
    pub label: String,
    /// The serde error, so the message says which field shape was unexpected.
    pub err: String,
}

/// A GraphQL `nodes: [Issue]` list decoded ONE ISSUE AT A TIME.
///
/// STUDIO-406: decoding the list as `Vec<RawIssue>` means one undecodable issue fails the entire
/// page — and because the poller logs a per-project fetch error and skips the project, a single bad
/// issue silently made every sibling undispatchable, indefinitely. Degrading instead of skipping
/// keeps the blast radius at one issue.
///
/// This is a deliberate divergence from Go, which decodes the page as a unit. It is unobservable on
/// a well-formed payload (identical result); it only changes behavior where Go would also have
/// failed the page.
#[derive(Debug, Default)]
pub(super) struct IssueNodes {
    /// The issues that decoded, in response order.
    pub kept: Vec<RawIssue>,
    /// The nodes that did not, in response order. Never empty without `kept` being short.
    pub dropped: Vec<DroppedNode>,
}

impl<'de> Deserialize<'de> for IssueNodes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Vec::<serde_json::Value>::deserialize(d)?;
        let mut out = IssueNodes {
            kept: Vec::with_capacity(raw.len()),
            dropped: Vec::new(),
        };
        for node in raw {
            // Recover a label BEFORE consuming the node, so a decode failure can still name it.
            let label = node
                .get("identifier")
                .and_then(serde_json::Value::as_str)
                .or_else(|| node.get("id").and_then(serde_json::Value::as_str))
                .unwrap_or("<unidentified>")
                .to_string();
            match serde_json::from_value::<RawIssue>(node) {
                Ok(iss) => out.kept.push(iss),
                Err(e) => out.dropped.push(DroppedNode {
                    label,
                    err: e.to_string(),
                }),
            }
        }
        Ok(out)
    }
}

impl IssueNodes {
    /// Logs one WARN per dropped issue, naming it, and returns how many were dropped. Called by
    /// each read path right after decoding a page. The issue is skipped for this tick only — the
    /// next poll re-fetches it, so a transient schema surprise self-heals once the field is typed.
    pub(super) fn warn_dropped(&self, op: &str) -> usize {
        for d in &self.dropped {
            tracing::warn!(
                issue = %d.label,
                err = %d.err,
                "issue could not be decoded and was skipped; its siblings are unaffected ({op})"
            );
        }
        self.dropped.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_good_nodes_and_names_the_bad_one() {
        let nodes: IssueNodes = serde_json::from_str(
            r#"[
              {"id":"1","identifier":"MT-1","title":"a"},
              {"id":"2","identifier":"MT-BAD","priority":"not-a-number"},
              {"id":"3","identifier":"MT-3","title":"c"}
            ]"#,
        )
        .expect("a bad node must not fail the list");
        let kept: Vec<&str> = nodes.kept.iter().map(RawIssue::identifier).collect();
        assert_eq!(kept, ["MT-1", "MT-3"]);
        assert_eq!(nodes.dropped.len(), 1);
        assert_eq!(nodes.dropped[0].label, "MT-BAD", "the bad issue is named");
        assert!(
            nodes.dropped[0].err.contains("expected"),
            "the serde error is preserved: {}",
            nodes.dropped[0].err
        );
        assert_eq!(nodes.warn_dropped("test"), 1);
    }

    #[test]
    fn labels_an_unidentifiable_node() {
        let nodes: IssueNodes =
            serde_json::from_str(r#"[{"priority":"bad"},{"id":"only-id","priority":"bad"}]"#)
                .expect("decode");
        assert!(nodes.kept.is_empty());
        let labels: Vec<&str> = nodes.dropped.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(
            labels,
            ["<unidentified>", "only-id"],
            "falls back id → sentinel"
        );
    }

    #[test]
    fn a_clean_page_drops_nothing() {
        let nodes: IssueNodes =
            serde_json::from_str(r#"[{"id":"1","identifier":"MT-1","title":"a"}]"#)
                .expect("decode");
        assert_eq!(nodes.kept.len(), 1);
        assert!(nodes.dropped.is_empty());
    }
}
