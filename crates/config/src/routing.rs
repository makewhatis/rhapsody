//! Reserved ticket-label routing namespaces (STUDIO-985; design record
//! `~/.rhapsody/docs/provider-auth-design.md` §2.3/§P3).
//!
//! Tickets already carry routing metadata in Linear labels: `rhapsody:@<name>` names an identity
//! (STUDIO-643) and `rhapsody:solo` opts a ticket out of the team entirely (STUDIO-669). A3 adds
//! three more namespaces to the same shared `rhapsody:` prefix:
//!
//! ```text
//! rhapsody:harness/<harness-id>
//! rhapsody:provider/<stable-provider-id>
//! rhapsody:model/<exact-model-id>
//! ```
//!
//! [`parse_ticket_selection`] parses and validates the three, and [`is_routing_label`] separates
//! them — plus the identity assignment label — from the CAPABILITY labels the dispatcher otherwise
//! strips out of `rhapsody:*` and looks up in the BO-11 registry. That split is the whole point:
//! `rhapsody:@alice` is an assignment and `rhapsody:harness/opencode` is routing metadata, not
//! practices an agent should be told to follow, so neither may reach capability collection.
//!
//! This module is **schema only** (P3): it resolves nothing, reads no credential and spawns no
//! runner. Every value it produces is a plain string a later slice (P4) resolves field-wise.

use crate::harness::HARNESS_NAMES;
use crate::providers::{canonical_provider_id, validate_model_id};

/// The Tier-0 identity-assignment prefix (`rhapsody:@alice`). Duplicated from the orchestrator's
/// own `IDENTITY_LABEL_PREFIX` on purpose: the two must agree, and this one is the copy the
/// capability collector and the label parser share.
pub const IDENTITY_LABEL_PREFIX: &str = "rhapsody:@";

/// The harness routing namespace (STUDIO-985).
pub const HARNESS_LABEL_PREFIX: &str = "rhapsody:harness/";
/// The provider routing namespace (STUDIO-985).
pub const PROVIDER_LABEL_PREFIX: &str = "rhapsody:provider/";
/// The model routing namespace (STUDIO-985). The suffix is the exact remainder and may contain `/`.
pub const MODEL_LABEL_PREFIX: &str = "rhapsody:model/";

/// Every label prefix that names routing metadata rather than a capability. A label in one of these
/// namespaces is excluded from capability collection (`ripgrep 'rhapsody:'` the design's existing
/// collector, which otherwise reads `@` as a capability name that silently no-ops).
pub const ROUTING_LABEL_PREFIXES: &[&str] = &[
    IDENTITY_LABEL_PREFIX,
    HARNESS_LABEL_PREFIX,
    PROVIDER_LABEL_PREFIX,
    MODEL_LABEL_PREFIX,
];

/// The longest label the tracker accepts. A ticket whose routing label exceeds this cannot be
/// stored on the ticket at all, so the dispatch is refused rather than silently losing the field.
///
/// The value is the tracker's own bound: Linear label names are limited to 255 characters. It is
/// deliberately shorter than [`crate::providers::MODEL_ID_MAX_BYTES`], so a model id legal in a
/// provider definition may still be too long to express on a ticket — that case refuses, which is
/// the contract the design asks for ("a label exceeding the tracker's supported length refuses the
/// dispatch").
pub const MAX_TICKET_LABEL_LEN: usize = 255;

/// The harness/provider/model a ticket names through its labels, each independently `None` when
/// absent so a later resolver can inherit the empty fields field-wise (§2.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TicketSelection {
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Why a ticket's routing labels were refused. Every variant is a dispatch refusal — a routing
/// field can never degrade to "run without it" the way unreadable profile prose can, because a
/// silently-dropped harness or provider is exactly the wrong-runner fallback the design forbids.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoutingLabelError {
    /// A label longer than the tracker can hold. Silently truncating would route to the wrong
    /// value, so the whole selection refuses.
    #[error(
        "routing_label_error: label {label:?} is {} characters, over the tracker's {max}-character \
         label limit",
        label.chars().count()
    )]
    TooLong { label: String, max: usize },
    /// `rhapsody:harness/` with nothing after the slash.
    #[error("routing_label_error: label {label:?} has an empty suffix; write `{prefix}<value>`")]
    EmptySuffix { label: String, prefix: &'static str },
    /// Two DISTINCT labels for the same field. Identical duplicates are harmless and allowed.
    #[error(
        "routing_label_error: conflicting {field} labels {first:?} and {second:?}; a ticket may \
         name {field} at most once"
    )]
    Conflict {
        field: &'static str,
        first: String,
        second: String,
    },
    /// A provider label that is not a canonical provider id (§2.2) — including anything that looks
    /// like a credential, which can never be a provider id.
    #[error(
        "routing_label_error: provider label {value:?} is not a canonical provider id: {reason}"
    )]
    InvalidProvider { value: String, reason: String },
    /// A model label outside the model-id transport bounds (§2.2).
    #[error("routing_label_error: model label {value:?} is not a valid model id: {reason}")]
    InvalidModel { value: String, reason: String },
}

/// Whether `label` belongs to one of the reserved routing namespaces. The capability collector
/// excludes exactly these; everything else that starts `rhapsody:` is still a capability name.
pub fn is_routing_label(label: &str) -> bool {
    ROUTING_LABEL_PREFIXES.iter().any(|p| label.starts_with(p))
}

/// The capability names a ticket's labels contribute: every `rhapsody:*` label EXCEPT the reserved
/// routing namespaces, with the `rhapsody:` prefix stripped. Non-`rhapsody:` labels are ignored
/// (they are ordinary topics, not capabilities).
///
/// This is the one filter the collector uses, so `rhapsody:@alice`, `rhapsody:harness/opencode`,
/// `rhapsody:provider/fireworks` and `rhapsody:model/...` can never enter capability collection —
/// they are assignments and routing metadata, not practices.
pub fn capability_label_names<'a, I>(labels: I) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    labels
        .into_iter()
        .filter(|l| !is_routing_label(l))
        .filter_map(|l| l.strip_prefix("rhapsody:"))
        .collect()
}

/// Whether `name` is a harness config validation recognizes (claude/codex/opencode). An unknown
/// non-empty harness is a routing-field error, never a silent fallback to `agent.backend`.
pub fn is_known_harness(name: &str) -> bool {
    HARNESS_NAMES.contains(&name)
}

/// Validates an EXPLICIT harness/provider/model tuple's syntax (`provider-auth-design.md` §2.2/§2.3,
/// STUDIO-985). Empty fields mean "inherit" and are never an error. Returns the first reason as a
/// plain string so each caller wraps it in its own error type (`ProfileError::Routing`,
/// `TeamsError::Invalid`, a `RosterIssue`).
pub fn validate_routing_fields(harness: &str, provider: &str, model: &str) -> Result<(), String> {
    if !harness.is_empty() && !is_known_harness(harness) {
        return Err(format!(
            "harness {harness:?} is not a recognized harness ({})",
            HARNESS_NAMES.join(", ")
        ));
    }
    if !provider.is_empty() {
        canonical_provider_id(provider).map_err(|reason| {
            format!("provider {provider:?} is not a canonical provider id: {reason}")
        })?;
    }
    if !model.is_empty() {
        validate_model_id(model)
            .map_err(|reason| format!("model {model:?} is not a valid model id: {reason}"))?;
    }
    Ok(())
}

/// Parses the three reserved routing namespaces out of a ticket's labels (STUDIO-985, §2.3).
///
/// Refuses — never partially resolves — on a label longer than [`MAX_TICKET_LABEL_LEN`], an empty
/// suffix, two distinct labels for one field, a non-canonical provider id, or an out-of-bounds
/// model id. Identical duplicate labels are allowed (Linear can carry a label twice).
pub fn parse_ticket_selection(labels: &[String]) -> Result<TicketSelection, RoutingLabelError> {
    let mut out = TicketSelection::default();
    for raw in labels {
        // Deliberately NOT trimmed: a suffix's surrounding whitespace is a transport-bound violation
        // the model-id rules must keep refusing, and a label carrying stray whitespace is simply not
        // the routing label it resembles.
        let label = raw.as_str();
        if !label.is_empty() && label.len() > MAX_TICKET_LABEL_LEN {
            return Err(RoutingLabelError::TooLong {
                label: label.to_string(),
                max: MAX_TICKET_LABEL_LEN,
            });
        }
        for (prefix, field) in [
            (HARNESS_LABEL_PREFIX, "harness"),
            (PROVIDER_LABEL_PREFIX, "provider"),
            (MODEL_LABEL_PREFIX, "model"),
        ] {
            let Some(value) = label.strip_prefix(prefix) else {
                continue;
            };
            if value.is_empty() {
                return Err(RoutingLabelError::EmptySuffix {
                    label: label.to_string(),
                    prefix,
                });
            }
            match field {
                "provider" => {
                    canonical_provider_id(value).map_err(|reason| {
                        RoutingLabelError::InvalidProvider {
                            value: value.to_string(),
                            reason,
                        }
                    })?;
                }
                "model" => {
                    validate_model_id(value).map_err(|reason| RoutingLabelError::InvalidModel {
                        value: value.to_string(),
                        reason,
                    })?;
                }
                _ => {}
            }
            let slot = match field {
                "harness" => &mut out.harness,
                "provider" => &mut out.provider,
                _ => &mut out.model,
            };
            match slot {
                None => *slot = Some(value.to_string()),
                Some(existing) if existing == value => {}
                Some(existing) => {
                    return Err(RoutingLabelError::Conflict {
                        field: match field {
                            "harness" => "harness",
                            "provider" => "provider",
                            _ => "model",
                        },
                        first: existing.clone(),
                        second: value.to_string(),
                    });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn routing_prefixes_are_all_recognized() {
        assert!(is_routing_label("rhapsody:@alice"));
        assert!(is_routing_label("rhapsody:harness/opencode"));
        assert!(is_routing_label("rhapsody:provider/fireworks"));
        assert!(is_routing_label(
            "rhapsody:model/accounts/fireworks/models/x"
        ));
    }

    #[test]
    fn capability_labels_are_not_routing() {
        assert!(!is_routing_label("rhapsody:code-review"));
        assert!(!is_routing_label("rhapsody:solo"));
        assert!(!is_routing_label("bug"));
    }

    /// **The mutation guard**: a `rhapsody:@alice` assignment must never reach capability
    /// collection, nor may any of the three new namespaces. Deleting the routing filter turns this
    /// test red because `@alice` would appear in the collected names.
    #[test]
    fn capability_collection_excludes_routing_labels() {
        let got = capability_label_names([
            "rhapsody:@alice",
            "rhapsody:harness/opencode",
            "rhapsody:provider/fireworks",
            "rhapsody:model/accounts/fireworks/models/deepseek-v4p1-flash",
            "rhapsody:code-review",
            "rust",
            "rhapsody:solo",
        ]);
        assert_eq!(got, vec!["code-review", "solo"]);
        assert!(
            !got.iter().any(|n| n.starts_with('@')),
            "an identity label leaked into capability collection"
        );
    }

    #[test]
    fn parses_each_namespace_and_allows_slashes_in_a_model() {
        let sel = parse_ticket_selection(&labels(&[
            "rhapsody:harness/opencode",
            "rhapsody:provider/fireworks",
            "rhapsody:model/accounts/fireworks/models/deepseek-v4p1-flash",
        ]))
        .expect("valid");
        assert_eq!(sel.harness.as_deref(), Some("opencode"));
        assert_eq!(sel.provider.as_deref(), Some("fireworks"));
        assert_eq!(
            sel.model.as_deref(),
            Some("accounts/fireworks/models/deepseek-v4p1-flash")
        );
    }

    #[test]
    fn absent_namespaces_are_none() {
        let sel = parse_ticket_selection(&labels(&["rhapsody:code-review", "rust"])).expect("ok");
        assert_eq!(sel, TicketSelection::default());
    }

    #[test]
    fn identical_duplicate_labels_are_allowed() {
        let sel = parse_ticket_selection(&labels(&[
            "rhapsody:harness/opencode",
            "rhapsody:harness/opencode",
        ]))
        .expect("identical duplicates are harmless");
        assert_eq!(sel.harness.as_deref(), Some("opencode"));
    }

    #[test]
    fn conflicting_labels_refuse() {
        let err = parse_ticket_selection(&labels(&[
            "rhapsody:provider/fireworks",
            "rhapsody:provider/openrouter",
        ]))
        .expect_err("conflict");
        assert!(matches!(
            err,
            RoutingLabelError::Conflict {
                field: "provider",
                ..
            }
        ));
        assert!(err.to_string().starts_with("routing_label_error:"));
    }

    #[test]
    fn empty_suffix_refuses() {
        for bad in ["rhapsody:harness/", "rhapsody:provider/", "rhapsody:model/"] {
            let err =
                parse_ticket_selection(&labels(&[bad])).expect_err("empty suffix must refuse");
            assert!(
                matches!(err, RoutingLabelError::EmptySuffix { .. }),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn over_long_label_refuses() {
        let huge = format!("rhapsody:model/{}", "m".repeat(MAX_TICKET_LABEL_LEN));
        let err = parse_ticket_selection(&labels(&[&huge])).expect_err("too long");
        assert!(matches!(err, RoutingLabelError::TooLong { .. }), "{err}");
    }

    /// A provider value is a canonical operator-chosen id, not a free string. A typical secret
    /// (`sk-live:ABC/123`, uppercase, `:`/`/`) is outside the charset and refuses; and there is no
    /// credential namespace at all, so a credential has no label to travel in.
    #[test]
    fn a_non_canonical_provider_label_refuses() {
        let err = parse_ticket_selection(&labels(&["rhapsody:provider/sk-live:ABC/123"]))
            .expect_err("not a canonical id");
        assert!(matches!(err, RoutingLabelError::InvalidProvider { .. }));
    }

    #[test]
    fn non_canonical_provider_labels_refuse() {
        for bad in ["Fireworks", "fire works", "1fireworks", ""] {
            let a = format!("rhapsody:provider/{bad}");
            if bad.is_empty() {
                continue;
            }
            let err = parse_ticket_selection(&labels(&[&a])).expect_err(bad);
            assert!(
                matches!(err, RoutingLabelError::InvalidProvider { .. }),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn model_label_with_surrounding_whitespace_refuses() {
        let err = parse_ticket_selection(&labels(&["rhapsody:model/ spaced "]))
            .expect_err("transport bound");
        assert!(
            matches!(err, RoutingLabelError::InvalidModel { .. }),
            "{err}"
        );
    }

    #[test]
    fn known_harnesses_are_known() {
        assert!(is_known_harness("claude"));
        assert!(is_known_harness("opencode"));
        assert!(!is_known_harness("goose"));
        assert!(!is_known_harness(""));
    }
}
