//! managerdecision — the manager DECISION CONTRACT (STUDIO-1010, the manager-agent program's third
//! ticket; design record `~/.rhapsody/docs/manager-agent-design.md` §6, §7.8, §8.1 and §8.2).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature at all, so nothing
//! here is a port; it is the additive Rhapsody surface the design record specifies, and every
//! production caller sits behind the ticketless-review gate (a review run only exists when Teams is
//! on and `review.mode: ticketless`).
//!
//! # What this module owns
//!
//! The whole of the manager's decision contract, as PURE functions with no I/O — no store, no
//! `gh`, no clock — so every rule in §6, §7.8 and §8 is a table test:
//!
//! 1. [`parse_decision`] — §6.1's STRICT block parser. Exactly one fenced
//!    `rhapsody-manager-decision` block, `deny_unknown_fields`-strict: unknown keys, duplicate
//!    keys, unknown variants, a field that does not apply to the variant (a JSON `null` counts as
//!    present, so it is invalid), a missing/empty required field, an empty list where a list is not
//!    allowed, an over-long string, a finding revision that does not exist or is not in the status
//!    the decision requires, and secret-shaped posted text are ALL refusals.
//! 2. [`preconditions`] — §6.2's deterministic preconditions (eligible rows for `RERUN_REVIEW`, the
//!    open blocking revisions a `route.fix` names).
//! 3. [`approval_eligibility`] — §6.4's six conditions for `APPROVE`, each reported by name so a
//!    fixture can be shown to fail for the condition it is about.
//! 4. [`eligible_rows`] — §6.2's eligible set (the live reviewer rows not approved at the current
//!    patch), which the `RERUN_REVIEW` effect re-requests.
//! 5. [`validate_final`] — §7.3's final-intervention restriction.
//! 6. [`revalidate`] — §8.1's outright supersession and §8.2's per-decision revalidation, returning
//!    one of `superseded`, `stale`, `still_valid` or `complete`, plus §7.8's threshold
//!    classification at activation.
//!
//! Nothing here WRITES a decision, activates one, or posts anything — the store, the applier and the
//! activation transaction are later tickets (M4 onward). The pure contract is what they consume.
//!
//! # Two facts this module is built to keep
//!
//! * **The manager is never a reviewer.** [`ManagerReviewRow::is_manager`] rows are filtered out of
//!   every live set by [`live_reviewer_rows`], so a manager row can never satisfy the quorum, meet
//!   an "every row had its turn" condition, or make an empty set approvable.
//! * **Classification is made at activation, never at launch.** [`revalidate`] reads the
//!   answered-exchange count NOW; the launch-time hint is carried for reporting only and can never
//!   keep a decision pre-threshold once the count says otherwise.

use rhapsody_store::{REVIEW_FINDING_OPEN, REVIEW_STATUS_DROPPED, ReviewCompleted};

use crate::reviewfindings::fenced_blocks;

/// The info string of the one fenced block a manager's final message must carry. The reviewer's
/// sibling is [`crate::reviewfindings::REVIEW_VERDICT_TAG`].
pub const MANAGER_DECISION_TAG: &str = "rhapsody-manager-decision";

/// The most characters a `rerun.note` may carry (§6.1).
pub const MAX_NOTE_CHARS: usize = 2_000;
/// The most characters an `instructions` or a `rationale` (or a dismissal rationale) may carry
/// (§6.1).
pub const MAX_TEXT_CHARS: usize = 4_000;

// ---------------------------------------------------------------------------------------------
// The parsed decision
// ---------------------------------------------------------------------------------------------

/// One finding revision a decision names (`finding` is the reviewer-scoped id, e.g. `alice:F1`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FindingRef {
    /// The reviewer-scoped finding id (`alice:F1`).
    pub finding: String,
    /// The revision of that finding.
    pub revision: i64,
}

/// One dismissal: a [`FindingRef`] with the manager's rationale for waiving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dismissal {
    /// The finding revision being dismissed.
    pub finding: FindingRef,
    /// Why it is being dismissed.
    pub rationale: String,
}

/// Which variant a decision is, and the payload that belongs to it (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionKind {
    /// Re-request a review round from the eligible rows (intersected with `reviewers` if given).
    RerunReview {
        /// The reviewers to restrict the re-request to; empty means all eligible rows.
        reviewers: Vec<String>,
        /// An optional note to post inside the mandatory explanation (≤ [`MAX_NOTE_CHARS`]).
        note: Option<String>,
    },
    /// Route the ticket back to its author with instructions.
    RouteToAuthor {
        /// The open blocking finding revisions the fix must name (one or more).
        fix: Vec<FindingRef>,
        /// What the author must do (≤ [`MAX_TEXT_CHARS`]).
        instructions: String,
    },
    /// A manager approval (§6.4, §6.5).
    Approve,
    /// Put the item on the human feed.
    Escalate {
        /// The question for the human.
        question: String,
        /// What the manager already checked.
        checked: String,
    },
}

/// A validated manager decision (§6.1). Constructed only by [`parse_decision`], so a variant's
/// payload is always present and a non-applicable field is never carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerDecision {
    /// The head the decision is bound to. Equals the current head at validation ([`head_is_current`]).
    pub head: String,
    /// The evidence revision echoed from the case packet.
    pub evidence_rev: i64,
    /// The finding revisions this decision dismisses (never on `ESCALATE`; may be empty).
    pub dismiss: Vec<Dismissal>,
    /// The manager's rationale (≤ [`MAX_TEXT_CHARS`]).
    pub rationale: String,
    /// The variant and its payload.
    pub kind: DecisionKind,
}

impl ManagerDecision {
    /// The variant name as the design spells it (`RERUN_REVIEW`, …).
    pub fn variant(&self) -> &'static str {
        match self.kind {
            DecisionKind::RerunReview { .. } => "RERUN_REVIEW",
            DecisionKind::RouteToAuthor { .. } => "ROUTE_TO_AUTHOR",
            DecisionKind::Approve => "APPROVE",
            DecisionKind::Escalate { .. } => "ESCALATE",
        }
    }
}

/// A finding revision known to the daemon, as the decision needs it to validate a `route.fix` or a
/// `dismiss`. Built by the caller from its finding ledger ([`rhapsody_store::ReviewFindingRow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFinding {
    /// The reviewer-scoped id (`alice:F1`).
    pub finding_id: String,
    /// The revision.
    pub revision: i64,
    /// The revision's status (`open`, `resolved`, `dismissed`, `settled`).
    pub status: String,
    /// Whether the revision blocks.
    pub blocking: bool,
}

/// Why a decision block was refused (§6.1). Every variant is a FAILED ATTEMPT: no part of the
/// block is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionError {
    /// Zero blocks or more than one.
    ZeroOrManyBlocks,
    /// The block body is not valid JSON.
    NotJson,
    /// The block body is JSON that is not an object.
    NotAnObject,
    /// A key appears twice.
    DuplicateKey(String),
    /// A field is unknown.
    UnknownField(String),
    /// `decision` is not one of the four variants.
    UnknownDecision,
    /// A field is present as JSON `null` (which counts as present, and is invalid).
    NullField(String),
    /// A required field is missing.
    MissingField(&'static str),
    /// A required field is empty.
    EmptyField(&'static str),
    /// A string exceeds its length limit.
    StringTooLong {
        /// The field name.
        field: &'static str,
        /// Its limit.
        max: usize,
    },
    /// An empty list where a list is not allowed.
    EmptyList(&'static str),
    /// A field that does not apply to the variant is present.
    FieldNotAllowed {
        /// The field name.
        field: &'static str,
        /// The variant it was given on.
        variant: &'static str,
    },
    /// A named finding revision does not exist.
    UnknownFinding(String),
    /// A named finding revision exists but is not an open blocking revision.
    WrongStatusFinding(String),
    /// Posted text matches the secret-shape scanner.
    SecretShape(&'static str),
    /// `RERUN_REVIEW` with no eligible rows (§6.2).
    NoEligibleRows,
    /// The decision's head is not the current head.
    HeadNotCurrent,
    /// The final intervention carries a decision other than `APPROVE`/`ESCALATE` (§7.3).
    FinalRestriction,
}

impl std::fmt::Display for DecisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecisionError::ZeroOrManyBlocks => {
                write!(
                    f,
                    "the message must contain exactly one {MANAGER_DECISION_TAG} block"
                )
            }
            DecisionError::NotJson => write!(f, "the decision block is not valid JSON"),
            DecisionError::NotAnObject => write!(f, "the decision block is not a JSON object"),
            DecisionError::DuplicateKey(k) => write!(f, "duplicate key: {k}"),
            DecisionError::UnknownField(k) => write!(f, "unknown field: {k}"),
            DecisionError::UnknownDecision => write!(f, "unknown decision variant"),
            DecisionError::NullField(k) => write!(f, "field {k} is null, which counts as present"),
            DecisionError::MissingField(k) => write!(f, "missing required field: {k}"),
            DecisionError::EmptyField(k) => write!(f, "empty field: {k}"),
            DecisionError::StringTooLong { field, max } => {
                write!(f, "{field} exceeds {max} characters")
            }
            DecisionError::EmptyList(k) => write!(f, "empty list where a list is not allowed: {k}"),
            DecisionError::FieldNotAllowed { field, variant } => {
                write!(f, "field {field} does not apply to {variant}")
            }
            DecisionError::UnknownFinding(id) => write!(f, "unknown finding revision: {id}"),
            DecisionError::WrongStatusFinding(id) => {
                write!(f, "finding revision is not open and blocking: {id}")
            }
            DecisionError::SecretShape(k) => write!(f, "secret-shaped text in {k}"),
            DecisionError::NoEligibleRows => {
                write!(f, "RERUN_REVIEW with no eligible rows is invalid")
            }
            DecisionError::HeadNotCurrent => {
                write!(f, "the decision's head is not the current head")
            }
            DecisionError::FinalRestriction => {
                write!(f, "the final intervention allows only APPROVE or ESCALATE")
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The strict JSON reader
// ---------------------------------------------------------------------------------------------

/// The parsed JSON tree, with OBJECTS HELD AS ORDERED PAIRS so a duplicate key can be seen at all:
/// `serde_json::Value` silently keeps the last of two same-named keys, which would make "a key
/// appears twice" undetectable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StrictJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictJson>),
    Object(Vec<(String, StrictJson)>),
}

impl StrictJson {
    fn parse(body: &str) -> Result<StrictJson, DecisionError> {
        let mut de = serde_json::Deserializer::from_str(body);
        let value = serde::Deserialize::deserialize(&mut de).map_err(|e| {
            // A duplicate key is reported by the visitor as a custom error; distinguish it from a
            // plain syntax error so the caller gets the precise refusal.
            let msg = e.to_string();
            if let Some(rest) = msg.strip_prefix("duplicate key: ") {
                // serde_json appends the source position; keep only the key.
                let key = rest.split(" at line").next().unwrap_or(rest);
                DecisionError::DuplicateKey(key.to_string())
            } else {
                DecisionError::NotJson
            }
        })?;
        de.end().map_err(|_| DecisionError::NotJson)?;
        Ok(value)
    }

    fn get(&self, key: &str) -> Option<&StrictJson> {
        match self {
            StrictJson::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn object(&self) -> Option<&[(String, StrictJson)]> {
        match self {
            StrictJson::Object(entries) => Some(entries),
            _ => None,
        }
    }
}

impl<'de> serde::Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = StrictJson;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictJson::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictJson::Number(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match i64::try_from(v) {
                    Ok(n) => Ok(StrictJson::Number(n.into())),
                    Err(_) => Err(E::custom("integer out of range")),
                }
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match serde_json::Number::from_f64(v) {
                    Some(n) => Ok(StrictJson::Number(n)),
                    None => Err(E::custom("non-finite number")),
                }
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson::String(v.to_string()))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictJson::String(v))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJson::Null)
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJson::Null)
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(StrictJson::Array(out))
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut out: Vec<(String, StrictJson)> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if out.iter().any(|(k, _)| *k == key) {
                        return Err(serde::de::Error::custom(format!("duplicate key: {key}")));
                    }
                    let value = map.next_value()?;
                    out.push((key, value));
                }
                Ok(StrictJson::Object(out))
            }
        }
        deserializer.deserialize_any(V)
    }
}

// ---------------------------------------------------------------------------------------------
// §6.1 — the parser
// ---------------------------------------------------------------------------------------------

/// Reads the ONE `rhapsody-manager-decision` block out of the manager run's final message and
/// validates every rule in §6.1 against the finding revisions the daemon knows.
///
/// `known` is the finding ledger; a `route.fix` or `dismiss` that names a revision not in it, or one
/// that is not `open` and blocking, is refused.
pub fn parse_decision(
    result_text: &str,
    known: &[KnownFinding],
) -> Result<ManagerDecision, DecisionError> {
    parse_decision_with(result_text, known, true)
}

/// Re-parse a decision block that was ALREADY validated before it was stored (§7.5). The §6.1
/// open-status requirement is SKIPPED: a `route.fix` or `dismiss` revision that has since been
/// resolved is exactly the evidence movement §8.2 exists to judge, not a parse failure. Enforcing
/// it here would leave the intervention pinned to the active index forever, because the stored
/// body never changes while the ledger does — the loop would `continue` on every sweep.
pub fn parse_stored_decision(
    result_text: &str,
    known: &[KnownFinding],
) -> Result<ManagerDecision, DecisionError> {
    parse_decision_with(result_text, known, false)
}

fn parse_decision_with(
    result_text: &str,
    known: &[KnownFinding],
    require_open: bool,
) -> Result<ManagerDecision, DecisionError> {
    let blocks = fenced_blocks(result_text, MANAGER_DECISION_TAG);
    let [body] = blocks.as_slice() else {
        return Err(DecisionError::ZeroOrManyBlocks);
    };
    let root = StrictJson::parse(body)?;
    let entries = root.object().ok_or(DecisionError::NotAnObject)?;

    // Reject an unknown key, a null-valued field, and gather the present top-level keys. A null
    // counts as PRESENT, so it is invalid for every field — including a field that would otherwise
    // not apply to the variant (the §6.1 rule stated directly).
    const ALLOWED: &[&str] = &[
        "decision",
        "head",
        "evidence_rev",
        "rerun",
        "route",
        "dismiss",
        "escalate",
        "rationale",
    ];
    for (key, value) in entries {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(DecisionError::UnknownField(key.clone()));
        }
        if matches!(value, StrictJson::Null) {
            return Err(DecisionError::NullField(key.clone()));
        }
    }

    let decision_name = entries
        .iter()
        .find(|(k, _)| k == "decision")
        .map(|(_, v)| v)
        .ok_or(DecisionError::MissingField("decision"))?;
    let StrictJson::String(decision_name) = decision_name else {
        return Err(DecisionError::MissingField("decision"));
    };
    let variant: &'static str = match decision_name.as_str() {
        "RERUN_REVIEW" => "RERUN_REVIEW",
        "ROUTE_TO_AUTHOR" => "ROUTE_TO_AUTHOR",
        "APPROVE" => "APPROVE",
        "ESCALATE" => "ESCALATE",
        _ => return Err(DecisionError::UnknownDecision),
    };

    // The fields that apply to each variant. `head`, `evidence_rev` and `rationale` apply to all.
    let common = ["head", "evidence_rev", "rationale"];
    let applies: &[&str] = match variant {
        "RERUN_REVIEW" => &["rerun", "dismiss"],
        "ROUTE_TO_AUTHOR" => &["route", "dismiss"],
        "APPROVE" => &["dismiss"],
        "ESCALATE" => &["escalate"],
        _ => &[],
    };
    for (key, _) in entries {
        if key == "decision" {
            continue;
        }
        let allowed = applies.contains(&key.as_str()) || common.contains(&key.as_str());
        if !allowed {
            return Err(DecisionError::FieldNotAllowed {
                field: leak_key(key),
                variant,
            });
        }
    }

    let head = required_string(&root, "head")?;
    let evidence_rev = required_nonneg_int(&root, "evidence_rev")?;
    let rationale = required_text(&root, "rationale", "rationale")?;

    let dismiss = match root.get("dismiss") {
        Some(v) => parse_dismissals(v, known, require_open)?,
        None => Vec::new(),
    };

    let kind = match variant {
        "RERUN_REVIEW" => {
            let (reviewers, note) = match root.get("rerun") {
                Some(v) => parse_rerun(v)?,
                None => (Vec::new(), None),
            };
            DecisionKind::RerunReview { reviewers, note }
        }
        "ROUTE_TO_AUTHOR" => {
            let route = root
                .get("route")
                .ok_or(DecisionError::MissingField("route"))?;
            let (fix, instructions) = parse_route(route, known, require_open)?;
            DecisionKind::RouteToAuthor { fix, instructions }
        }
        "APPROVE" => DecisionKind::Approve,
        "ESCALATE" => {
            let escalate = root
                .get("escalate")
                .ok_or(DecisionError::MissingField("escalate"))?;
            let (question, checked) = parse_escalate(escalate)?;
            DecisionKind::Escalate { question, checked }
        }
        _ => return Err(DecisionError::UnknownDecision),
    };

    Ok(ManagerDecision {
        head,
        evidence_rev,
        dismiss,
        rationale,
        kind,
    })
}

/// `&'static str` for the field names this module matches on, so [`DecisionError`] stays cheap. An
/// unrecognised key can only reach here if it was not in `ALLOWED`, which is handled earlier.
fn leak_key(key: &str) -> &'static str {
    match key {
        "decision" => "decision",
        "head" => "head",
        "evidence_rev" => "evidence_rev",
        "rerun" => "rerun",
        "route" => "route",
        "dismiss" => "dismiss",
        "escalate" => "escalate",
        "rationale" => "rationale",
        _ => "field",
    }
}

fn required_string(root: &StrictJson, key: &'static str) -> Result<String, DecisionError> {
    match root.get(key) {
        None => Err(DecisionError::MissingField(key)),
        Some(StrictJson::String(s)) if s.trim().is_empty() => Err(DecisionError::EmptyField(key)),
        Some(StrictJson::String(s)) => Ok(s.clone()),
        Some(_) => Err(DecisionError::MissingField(key)),
    }
}

fn required_nonneg_int(root: &StrictJson, key: &'static str) -> Result<i64, DecisionError> {
    match root.get(key) {
        None => Err(DecisionError::MissingField(key)),
        Some(StrictJson::Number(n)) => n
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or(DecisionError::MissingField(key)),
        Some(_) => Err(DecisionError::MissingField(key)),
    }
}

/// A required, non-empty, length-limited, secret-scanned string.
fn required_text(
    root: &StrictJson,
    key: &'static str,
    label: &'static str,
) -> Result<String, DecisionError> {
    let s = required_string(root, key)?;
    if s.chars().count() > MAX_TEXT_CHARS {
        return Err(DecisionError::StringTooLong {
            field: label,
            max: MAX_TEXT_CHARS,
        });
    }
    if contains_secret_shape(&s) {
        return Err(DecisionError::SecretShape(label));
    }
    Ok(s)
}

fn parse_rerun(rerun: &StrictJson) -> Result<(Vec<String>, Option<String>), DecisionError> {
    let entries = rerun.object().ok_or(DecisionError::FieldNotAllowed {
        field: "rerun",
        variant: "RERUN_REVIEW",
    })?;
    let mut reviewers = Vec::new();
    let mut note = None;
    for (key, value) in entries {
        match key.as_str() {
            "reviewers" => {
                let StrictJson::Array(items) = value else {
                    return Err(DecisionError::EmptyList("rerun.reviewers"));
                };
                if items.is_empty() {
                    return Err(DecisionError::EmptyList("rerun.reviewers"));
                }
                for item in items {
                    match item {
                        StrictJson::String(s) if !s.trim().is_empty() => reviewers.push(s.clone()),
                        StrictJson::Null => {
                            return Err(DecisionError::NullField("rerun.reviewers".to_string()));
                        }
                        _ => return Err(DecisionError::EmptyField("rerun.reviewers")),
                    }
                }
            }
            "note" => {
                let StrictJson::String(s) = value else {
                    return Err(DecisionError::MissingField("rerun.note"));
                };
                if s.trim().is_empty() {
                    return Err(DecisionError::EmptyField("rerun.note"));
                }
                if s.chars().count() > MAX_NOTE_CHARS {
                    return Err(DecisionError::StringTooLong {
                        field: "note",
                        max: MAX_NOTE_CHARS,
                    });
                }
                if contains_secret_shape(s) {
                    return Err(DecisionError::SecretShape("note"));
                }
                note = Some(s.clone());
            }
            other => return Err(DecisionError::UnknownField(format!("rerun.{other}"))),
        }
    }
    Ok((reviewers, note))
}

fn parse_route(
    route: &StrictJson,
    known: &[KnownFinding],
    require_open: bool,
) -> Result<(Vec<FindingRef>, String), DecisionError> {
    let entries = route.object().ok_or(DecisionError::FieldNotAllowed {
        field: "route",
        variant: "ROUTE_TO_AUTHOR",
    })?;
    let mut fix = None;
    let mut instructions = None;
    for (key, value) in entries {
        match key.as_str() {
            "fix" => {
                let StrictJson::Array(items) = value else {
                    return Err(DecisionError::EmptyList("route.fix"));
                };
                if items.is_empty() {
                    return Err(DecisionError::EmptyList("route.fix"));
                }
                let mut refs = Vec::new();
                for item in items {
                    refs.push(parse_finding_ref(item)?);
                }
                for r in &refs {
                    require_open_blocking(r, known, require_open)?;
                }
                fix = Some(refs);
            }
            "instructions" => {
                let StrictJson::String(s) = value else {
                    return Err(DecisionError::MissingField("route.instructions"));
                };
                if s.trim().is_empty() {
                    return Err(DecisionError::EmptyField("instructions"));
                }
                if s.chars().count() > MAX_TEXT_CHARS {
                    return Err(DecisionError::StringTooLong {
                        field: "instructions",
                        max: MAX_TEXT_CHARS,
                    });
                }
                if contains_secret_shape(s) {
                    return Err(DecisionError::SecretShape("instructions"));
                }
                instructions = Some(s.clone());
            }
            other => return Err(DecisionError::UnknownField(format!("route.{other}"))),
        }
    }
    let fix = fix.ok_or(DecisionError::MissingField("route.fix"))?;
    let instructions = instructions.ok_or(DecisionError::MissingField("route.instructions"))?;
    Ok((fix, instructions))
}

fn parse_escalate(escalate: &StrictJson) -> Result<(String, String), DecisionError> {
    let entries = escalate.object().ok_or(DecisionError::FieldNotAllowed {
        field: "escalate",
        variant: "ESCALATE",
    })?;
    let mut question = None;
    let mut checked = None;
    for (key, value) in entries {
        match key.as_str() {
            "question" | "checked" => {
                let StrictJson::String(s) = value else {
                    return Err(DecisionError::MissingField(if key == "question" {
                        "escalate.question"
                    } else {
                        "escalate.checked"
                    }));
                };
                if s.trim().is_empty() {
                    return Err(DecisionError::EmptyField(if key == "question" {
                        "question"
                    } else {
                        "checked"
                    }));
                }
                if s.chars().count() > MAX_TEXT_CHARS {
                    return Err(DecisionError::StringTooLong {
                        field: if key == "question" {
                            "question"
                        } else {
                            "checked"
                        },
                        max: MAX_TEXT_CHARS,
                    });
                }
                if contains_secret_shape(s) {
                    return Err(DecisionError::SecretShape(if key == "question" {
                        "question"
                    } else {
                        "checked"
                    }));
                }
                if key == "question" {
                    question = Some(s.clone());
                } else {
                    checked = Some(s.clone());
                }
            }
            other => return Err(DecisionError::UnknownField(format!("escalate.{other}"))),
        }
    }
    let question = question.ok_or(DecisionError::MissingField("escalate.question"))?;
    let checked = checked.ok_or(DecisionError::MissingField("escalate.checked"))?;
    Ok((question, checked))
}

fn parse_dismissals(
    dismiss: &StrictJson,
    known: &[KnownFinding],
    require_open: bool,
) -> Result<Vec<Dismissal>, DecisionError> {
    let StrictJson::Array(items) = dismiss else {
        return Err(DecisionError::EmptyList("dismiss"));
    };
    if items.is_empty() {
        return Err(DecisionError::EmptyList("dismiss"));
    }
    let mut out = Vec::new();
    for item in items {
        let entries = item.object().ok_or(DecisionError::EmptyList("dismiss"))?;
        let mut finding = None;
        let mut rationale = None;
        for (key, value) in entries {
            match key.as_str() {
                "finding" | "revision" => {
                    finding = Some(merge_finding_ref(finding, key, value)?);
                }
                "rationale" => {
                    let StrictJson::String(s) = value else {
                        return Err(DecisionError::MissingField("dismiss.rationale"));
                    };
                    if s.trim().is_empty() {
                        return Err(DecisionError::EmptyField("rationale"));
                    }
                    if s.chars().count() > MAX_TEXT_CHARS {
                        return Err(DecisionError::StringTooLong {
                            field: "rationale",
                            max: MAX_TEXT_CHARS,
                        });
                    }
                    if contains_secret_shape(s) {
                        return Err(DecisionError::SecretShape("rationale"));
                    }
                    rationale = Some(s.clone());
                }
                other => return Err(DecisionError::UnknownField(format!("dismiss.{other}"))),
            }
        }
        let finding = finding.ok_or(DecisionError::MissingField("dismiss.finding"))?;
        let rationale = rationale.ok_or(DecisionError::MissingField("dismiss.rationale"))?;
        require_open_blocking(&finding, known, require_open)?;
        out.push(Dismissal { finding, rationale });
    }
    Ok(out)
}

/// Reads one `{finding, revision}` pair from a dismissal, accumulating the two keys into one
/// [`FindingRef`] whatever order they appear in.
fn merge_finding_ref(
    current: Option<FindingRef>,
    key: &str,
    value: &StrictJson,
) -> Result<FindingRef, DecisionError> {
    let mut r = current.unwrap_or(FindingRef {
        finding: String::new(),
        revision: i64::MIN,
    });
    match key {
        "finding" => match value {
            StrictJson::String(s) if !s.trim().is_empty() => r.finding = s.clone(),
            _ => return Err(DecisionError::EmptyField("finding")),
        },
        "revision" => match value {
            StrictJson::Number(n) => match n.as_i64() {
                Some(v) => r.revision = v,
                None => return Err(DecisionError::MissingField("dismiss.revision")),
            },
            _ => return Err(DecisionError::MissingField("dismiss.revision")),
        },
        other => return Err(DecisionError::UnknownField(format!("dismiss.{other}"))),
    }
    Ok(r)
}

fn parse_finding_ref(item: &StrictJson) -> Result<FindingRef, DecisionError> {
    let entries = item.object().ok_or(DecisionError::EmptyList("route.fix"))?;
    let mut ref_out = FindingRef {
        finding: String::new(),
        revision: i64::MIN,
    };
    for (key, value) in entries {
        match key.as_str() {
            "finding" => match value {
                StrictJson::String(s) if !s.trim().is_empty() => ref_out.finding = s.clone(),
                _ => return Err(DecisionError::EmptyField("finding")),
            },
            "revision" => match value {
                StrictJson::Number(n) => match n.as_i64() {
                    Some(v) => ref_out.revision = v,
                    None => return Err(DecisionError::MissingField("route.revision")),
                },
                _ => return Err(DecisionError::MissingField("route.revision")),
            },
            other => return Err(DecisionError::UnknownField(format!("fix.{other}"))),
        }
    }
    if ref_out.finding.is_empty() || ref_out.revision == i64::MIN {
        return Err(DecisionError::MissingField("route.fix"));
    }
    Ok(ref_out)
}

/// A `route.fix` or a `dismiss` must name an existing revision that is `open` and blocking (§6.1,
/// §6.3). A resolved, settled or dismissed revision is the wrong status; an unknown one does not
/// exist.
fn require_open_blocking(
    r: &FindingRef,
    known: &[KnownFinding],
    require_open: bool,
) -> Result<(), DecisionError> {
    if !require_open {
        return Ok(()); // a stored, already-validated decision: §8.2 judges status, not this parser
    }
    let id = format!("{}@r{}", r.finding, r.revision);
    match known
        .iter()
        .find(|k| k.finding_id == r.finding && k.revision == r.revision)
    {
        None => Err(DecisionError::UnknownFinding(id)),
        Some(k) if !k.blocking || k.status != REVIEW_FINDING_OPEN => {
            Err(DecisionError::WrongStatusFinding(id))
        }
        Some(_) => Ok(()),
    }
}

/// The secret-shape scanner (§6.1). A conservative PREFIX/known-marker check rather than an entropy
/// heuristic: a 40-hex SHA is not a secret, and a false positive here refuses a legitimate decision,
/// so the scanner fires only on shapes that can only be a credential.
fn contains_secret_shape(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "lin_api_",
        "sk-",
        "sk_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "AKIA",
        "AIza",
    ];
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return true;
    }
    text.split_whitespace()
        .any(|tok| PREFIXES.iter().any(|p| tok.starts_with(p)))
}

// ---------------------------------------------------------------------------------------------
// §6.2 — preconditions
// ---------------------------------------------------------------------------------------------

/// The inputs the deterministic preconditions read. `eligible_rows` is the count from
/// [`eligible_rows`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PreconditionInputs {
    /// Live reviewer rows not approved at the current patch (§6.2).
    pub eligible_rows: usize,
}

/// §6.2's deterministic preconditions. `ROUTE_TO_AUTHOR`'s "fix names one or more open blocking
/// revisions" was already enforced at parse ([`parse_decision`]); `ESCALATE`'s `question`/`checked`
/// likewise. What remains here is `RERUN_REVIEW`'s "at least one eligible row".
pub fn preconditions(
    decision: &ManagerDecision,
    inputs: &PreconditionInputs,
) -> Result<(), DecisionError> {
    match decision.kind {
        DecisionKind::RerunReview { .. } if inputs.eligible_rows == 0 => {
            Err(DecisionError::NoEligibleRows)
        }
        _ => Ok(()),
    }
}

/// Whether the decision's head IS the current head. A decision naming any other head is a stale
/// head and a failed attempt (F8's rejection of `31ee051`).
pub fn head_is_current(
    decision: &ManagerDecision,
    current_head: &str,
) -> Result<(), DecisionError> {
    if !current_head.is_empty() && decision.head == current_head {
        Ok(())
    } else {
        Err(DecisionError::HeadNotCurrent)
    }
}

/// §7.3's final-intervention restriction: when `final_intervention` is set, only `APPROVE` (with an
/// optional `dismiss`) or `ESCALATE` is allowed.
pub fn validate_final(
    decision: &ManagerDecision,
    final_intervention: bool,
) -> Result<(), DecisionError> {
    if final_intervention
        && !matches!(
            decision.kind,
            DecisionKind::Approve | DecisionKind::Escalate { .. }
        )
    {
        return Err(DecisionError::FinalRestriction);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// §6.4 — approval eligibility
// ---------------------------------------------------------------------------------------------

/// One watch row as approval eligibility sees it: the reviewer's identity, whether the row is the
/// MANAGER (never a reviewer), its status, its last completed review, and whether the run was served
/// a covering diff for the completed sha (§5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerReviewRow {
    /// The teammate identity on the watch row.
    pub reviewer: String,
    /// Whether this row is the manager, who is **never** among the live rows (§3.1, §6.4).
    pub is_manager: bool,
    /// The transient watch status. [`REVIEW_STATUS_DROPPED`] means dead.
    pub status: String,
    /// The last review that completed with a verdict for this row, or `None`.
    pub completed: Option<ReviewCompleted>,
    /// Whether the run was served a covering diff/interdiff to the current head for the completed
    /// sha (§5.5 condition 3). Meaningless without a completion.
    pub diff_covered: bool,
}

impl ManagerReviewRow {
    /// Whether the row is live: not `dropped`.
    pub fn live(&self) -> bool {
        self.status != REVIEW_STATUS_DROPPED
    }
}

/// The LIVE REVIEWER rows: non-dropped rows that are not the manager. The manager is filtered out
/// HERE, once, so no condition below can count it.
pub fn live_reviewer_rows(rows: &[ManagerReviewRow]) -> Vec<&ManagerReviewRow> {
    rows.iter().filter(|r| !r.is_manager && r.live()).collect()
}

/// The ELIGIBLE rows for `RERUN_REVIEW` (§6.2): live reviewer rows NOT approved at the current
/// patch (§5.4).
pub fn eligible_rows<'a>(
    rows: &'a [ManagerReviewRow],
    generation: i64,
    current_patch_id: &str,
) -> Vec<&'a ManagerReviewRow> {
    live_reviewer_rows(rows)
        .into_iter()
        .filter(|r| {
            !crate::reviewevidence::completion_approved_at_current_patch(
                r.completed.as_ref(),
                generation,
                current_patch_id,
            )
        })
        .collect()
}

/// The approval conditions (§6.4), each checked by the daemon and each reported by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalCondition {
    /// 0 — the round threshold has been reached in this generation.
    Threshold,
    /// 1 — the reviewer set meets the quorum (and is not empty).
    Quorum,
    /// 2 — every live row has had its turn (a completed verdict in this generation).
    EveryRowHadTurn,
    /// 3 — diff coverage for every live row's completed sha.
    DiffCoverage,
    /// 4 — every open blocking revision is resolved or dismissed in this decision.
    FindingsResolved,
    /// 5 — every live row has read the current patch, or this is the final intervention.
    RowsAtCurrentPatch,
}

impl ApprovalCondition {
    /// The condition's §6.4 number.
    pub fn number(self) -> u8 {
        match self {
            ApprovalCondition::Threshold => 0,
            ApprovalCondition::Quorum => 1,
            ApprovalCondition::EveryRowHadTurn => 2,
            ApprovalCondition::DiffCoverage => 3,
            ApprovalCondition::FindingsResolved => 4,
            ApprovalCondition::RowsAtCurrentPatch => 5,
        }
    }
}

/// The inputs to §6.4's six conditions.
#[derive(Debug)]
pub struct ApprovalInputs<'a> {
    /// This pull request's watch rows (the manager's own row may be present; it is filtered out).
    pub rows: &'a [ManagerReviewRow],
    /// `review.required` members — each must have a live row.
    pub required: &'a [String],
    /// `review.effective_reviewers()`.
    pub effective_reviewers: usize,
    /// The current loop generation.
    pub generation: i64,
    /// The current head's patch-id.
    pub current_patch_id: &'a str,
    /// Condition 0: the threshold has been reached, counted as answered exchanges.
    pub threshold_reached: bool,
    /// Whether this is the generation's final intervention (D8 / condition 5's alternative).
    pub final_intervention: bool,
    /// The open blocking finding revisions on the pull request.
    pub open_blocking: &'a [FindingRef],
    /// The finding revisions this decision dismisses.
    pub dismissed: &'a [FindingRef],
}

/// §6.4's six conditions for `APPROVE`. `Ok(())` means eligible; `Err(conditions)` lists every
/// condition that FAILED, so a fixture can be shown to fail for the condition it is about.
pub fn approval_eligibility(inputs: &ApprovalInputs<'_>) -> Result<(), Vec<ApprovalCondition>> {
    let live = live_reviewer_rows(inputs.rows);
    let mut failed = Vec::new();

    // 0 — the threshold.
    if !inputs.threshold_reached {
        failed.push(ApprovalCondition::Threshold);
    }

    // 1 — quorum, and an EMPTY set is never approvable.
    let quorum = !live.is_empty()
        && live.len() >= inputs.effective_reviewers
        && inputs.required.iter().all(|name| {
            live.iter()
                .any(|r| r.reviewer.eq_ignore_ascii_case(name.trim()))
        });
    if !quorum {
        failed.push(ApprovalCondition::Quorum);
    }

    // 2 — every live row has had its turn in THIS generation.
    let every_turn = live.iter().all(|r| {
        r.completed
            .as_ref()
            .is_some_and(|c| c.generation == inputs.generation)
    });
    if !every_turn {
        failed.push(ApprovalCondition::EveryRowHadTurn);
    }

    // 3 — diff coverage for every DISTINCT completed sha among the live rows.
    let mut covered_shas: Vec<&str> = Vec::new();
    let mut coverage = true;
    for r in &live {
        let Some(c) = r.completed.as_ref() else {
            coverage = false;
            continue;
        };
        if c.sha.is_empty() {
            continue;
        }
        if covered_shas.contains(&c.sha.as_str()) {
            continue;
        }
        covered_shas.push(&c.sha);
        if !r.diff_covered {
            coverage = false;
        }
    }
    if !coverage {
        failed.push(ApprovalCondition::DiffCoverage);
    }

    // 4 — every open blocking revision is resolved or dismissed in this same decision.
    let all_dismissed = inputs
        .open_blocking
        .iter()
        .all(|o| inputs.dismissed.contains(o));
    if !all_dismissed {
        failed.push(ApprovalCondition::FindingsResolved);
    }

    // 5 — every live row's completed patch-id equals the current patch, OR this is the final
    // intervention.
    let at_patch = inputs.final_intervention
        || live.iter().all(|r| {
            r.completed
                .as_ref()
                .is_some_and(|c| !c.patch_id.is_empty() && c.patch_id == inputs.current_patch_id)
        });
    if !at_patch {
        failed.push(ApprovalCondition::RowsAtCurrentPatch);
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

// ---------------------------------------------------------------------------------------------
// §7.8 / §8 — revalidation
// ---------------------------------------------------------------------------------------------

/// §7.8's threshold classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdPhase {
    /// Before the round threshold: `RERUN_REVIEW`, `ROUTE_TO_AUTHOR` and `ESCALATE` are allowed,
    /// `APPROVE` is not.
    PreThreshold,
    /// At or past the round threshold: an intervention consumes a post-threshold slot.
    PostThreshold,
}

/// §7.8's classification from the ANSWERED-EXCHANGE count (STUDIO-1004's definition) and the
/// configured threshold. `0` means the threshold is trivially reached.
pub fn threshold_phase(answered_exchanges: i64, adjudicate_after_rounds: i64) -> ThresholdPhase {
    if answered_exchanges >= adjudicate_after_rounds {
        ThresholdPhase::PostThreshold
    } else {
        ThresholdPhase::PreThreshold
    }
}

/// The outcome of revalidating a saved decision against current state (§8.1, §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revalidation {
    /// Nothing is applied: the generation, authority, hold, open state or enablement changed.
    Superseded,
    /// The evidence or threshold classification changed and a new run is needed.
    Stale,
    /// The decision still holds; apply it.
    StillValid,
    /// The decision's goal is already met (a `RERUN_REVIEW` whose rows are all approved now).
    Complete,
}

/// The inputs to [`revalidate`]. `before_*` is the state the decision was decided against; the rest
/// is the state NOW (§8.1–§8.2, §7.8).
#[derive(Debug)]
pub struct RevalidationInputs<'a> {
    /// The saved decision being revalidated.
    pub decision: &'a ManagerDecision,
    /// The generation the decision was bound to.
    pub before_generation: i64,
    /// The generation now.
    pub after_generation: i64,
    /// `review_authority` is still `act`.
    pub authority_act: bool,
    /// The pull request is still open.
    pub pr_open: bool,
    /// The `rhapsody:human` hold set has been read (fail closed when it has not).
    pub hold_known: bool,
    /// A hold is applied.
    pub hold_applied: bool,
    /// The manager is enabled.
    pub manager_enabled: bool,
    /// The evidence revision the decision was bound to.
    pub before_evidence_rev: i64,
    /// The evidence revision now.
    pub after_evidence_rev: i64,
    /// The decision's patch-id is still the current one.
    pub patch_id_unchanged: bool,
    /// `RERUN_REVIEW`: live reviewer rows not approved at the current patch.
    pub eligible_rows: usize,
    /// `RERUN_REVIEW`: every live reviewer row is approved at the current patch.
    pub all_rows_approved: bool,
    /// `ROUTE_TO_AUTHOR`: every `route.fix` revision is still `open`.
    pub route_fix_still_open: bool,
    /// `APPROVE`: a review has completed since the decision's evidence revision.
    pub review_completed_since: bool,
    /// `APPROVE`: the finding set is unchanged since the decision.
    pub finding_set_unchanged: bool,
    /// `APPROVE`: §6.4 still holds in full, evaluated now.
    pub approval_still_eligible: bool,
    /// §7.8: the phase recorded at launch — the CASE-PACKET HINT, never used to charge.
    pub phase_at_launch: ThresholdPhase,
    /// §7.8: answered exchanges now (STUDIO-1004).
    pub answered_exchanges: i64,
    /// §7.8: `review.adjudicate_after_rounds`.
    pub adjudicate_after_rounds: i64,
    /// §7.8: post-threshold interventions already applied in this generation.
    pub interventions_applied: i64,
    /// §7.3: `manager.max_interventions`.
    pub max_interventions: i64,
    /// §7.3: this intervention was launched as the final one.
    pub final_intervention: bool,
}

/// §8.1's outright supersession and §8.2's per-decision revalidation, with §7.8's threshold
/// classification evaluated at activation.
pub fn revalidate(inputs: &RevalidationInputs<'_>) -> Revalidation {
    // §8.1 — superseded outright: nothing applied, no new run.
    if inputs.after_generation != inputs.before_generation {
        return Revalidation::Superseded;
    }
    if !inputs.authority_act || !inputs.pr_open || !inputs.manager_enabled {
        return Revalidation::Superseded;
    }
    if !inputs.hold_known || inputs.hold_applied {
        return Revalidation::Superseded;
    }

    let kind = &inputs.decision.kind;
    let is_escalate = matches!(kind, DecisionKind::Escalate { .. });

    // §7.8 — the classification is made NOW. The launch hint is carried for reporting, and the
    // charge is the later of the two; because the answered-exchange count only grows, a decision
    // that was post-threshold at launch is still post-threshold now, and one that crossed the
    // threshold in between is charged post-threshold here.
    let now = threshold_phase(inputs.answered_exchanges, inputs.adjudicate_after_rounds);
    let charge = if inputs.phase_at_launch == ThresholdPhase::PostThreshold
        || now == ThresholdPhase::PostThreshold
    {
        ThresholdPhase::PostThreshold
    } else {
        ThresholdPhase::PreThreshold
    };
    if charge == ThresholdPhase::PostThreshold
        && !inputs.final_intervention
        && inputs.interventions_applied + 1 >= inputs.max_interventions
    {
        // At `applied = N − 1` the next post-threshold intervention had to be launched final, and a
        // non-final RERUN/ROUTE is not allowed there: activation fails as `stale` so the sweep
        // re-plans with `final: true`. `APPROVE` is re-checked under §6.4 condition 0 instead.
        if matches!(
            kind,
            DecisionKind::RerunReview { .. } | DecisionKind::RouteToAuthor { .. }
        ) {
            return Revalidation::Stale;
        }
    }

    // §8.2 — a new patch-id makes any unapplied decision except ESCALATE stale.
    if !inputs.patch_id_unchanged && !is_escalate {
        return Revalidation::Stale;
    }

    // No evidence moved: nothing further to re-evaluate.
    if inputs.after_evidence_rev == inputs.before_evidence_rev
        && now == ThresholdPhase::PreThreshold
    {
        return Revalidation::StillValid;
    }
    if inputs.after_evidence_rev == inputs.before_evidence_rev {
        // The evidence is unchanged but the threshold classification moved; for APPROVE that only
        // matters through §6.4 condition 0, which `approval_still_eligible` carries.
        if !matches!(kind, DecisionKind::Approve) {
            return Revalidation::StillValid;
        }
    }

    match kind {
        DecisionKind::Escalate { .. } => Revalidation::StillValid,
        DecisionKind::RerunReview { .. } => {
            if inputs.eligible_rows > 0 {
                Revalidation::StillValid
            } else if inputs.all_rows_approved {
                Revalidation::Complete
            } else {
                Revalidation::Stale
            }
        }
        DecisionKind::RouteToAuthor { .. } => {
            if inputs.route_fix_still_open {
                Revalidation::StillValid
            } else {
                Revalidation::Stale
            }
        }
        DecisionKind::Approve => {
            if !inputs.review_completed_since
                && inputs.finding_set_unchanged
                && inputs.approval_still_eligible
            {
                Revalidation::StillValid
            } else {
                Revalidation::Stale
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{
        REVIEW_COMPLETION_APPROVE, REVIEW_FINDING_RESOLVED, REVIEW_STATUS_IN_FLIGHT,
    };

    fn completion(verdict: &str, generation: i64, sha: &str, patch_id: &str) -> ReviewCompleted {
        ReviewCompleted {
            generation,
            sha: sha.to_string(),
            patch_id: patch_id.to_string(),
            verdict: verdict.to_string(),
        }
    }

    fn known(finding_id: &str, revision: i64, status: &str, blocking: bool) -> KnownFinding {
        KnownFinding {
            finding_id: finding_id.to_string(),
            revision,
            status: status.to_string(),
            blocking,
        }
    }

    fn wrap(block: &str) -> String {
        format!("some prose\n\n```{MANAGER_DECISION_TAG}\n{block}\n```\n\nHANDOFF: done\n")
    }

    fn block_json(v: serde_json::Value) -> String {
        wrap(&v.to_string())
    }

    // --- §6.1 the parser ---------------------------------------------------------------------

    #[test]
    fn parses_each_variant() {
        let rerun = block_json(serde_json::json!({
            "decision": "RERUN_REVIEW",
            "head": "87aa044",
            "evidence_rev": 3,
            "rerun": {"reviewers": ["sol"], "note": "check the shutdown path"},
            "rationale": "rows last completed earlier"
        }));
        let d = parse_decision(&rerun, &[]).expect("rerun parses");
        assert_eq!(d.variant(), "RERUN_REVIEW");
        assert_eq!(d.head, "87aa044");
        assert_eq!(d.evidence_rev, 3);
        assert_eq!(
            d.kind,
            DecisionKind::RerunReview {
                reviewers: vec!["sol".to_string()],
                note: Some("check the shutdown path".to_string()),
            }
        );

        let route = block_json(serde_json::json!({
            "decision": "ROUTE_TO_AUTHOR",
            "head": "h",
            "evidence_rev": 1,
            "route": {"fix": [{"finding": "alice:F1", "revision": 1}], "instructions": "fix it"},
            "rationale": "open finding"
        }));
        let d = parse_decision(&route, &[known("alice:F1", 1, REVIEW_FINDING_OPEN, true)])
            .expect("route parses");
        assert_eq!(d.variant(), "ROUTE_TO_AUTHOR");

        let approve = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1, "rationale": "all read"
        }));
        assert_eq!(
            parse_decision(&approve, &[]).expect("approve parses").kind,
            DecisionKind::Approve
        );

        let escalate = block_json(serde_json::json!({
            "decision": "ESCALATE", "head": "h", "evidence_rev": 1,
            "escalate": {"question": "which base?", "checked": "compared both diffs"},
            "rationale": "needs a human"
        }));
        assert_eq!(
            parse_decision(&escalate, &[])
                .expect("escalate parses")
                .variant(),
            "ESCALATE"
        );
    }

    #[test]
    fn rejects_zero_or_many_blocks() {
        assert_eq!(
            parse_decision("no block here", &[]),
            Err(DecisionError::ZeroOrManyBlocks)
        );
        let two = format!("{}\n{}", wrap("{\"decision\":\"APPROVE\"}"), wrap("{}"));
        assert_eq!(
            parse_decision(&two, &[]),
            Err(DecisionError::ZeroOrManyBlocks)
        );
    }

    #[test]
    fn rejects_bad_json_and_non_objects() {
        assert_eq!(
            parse_decision(&wrap("not json"), &[]),
            Err(DecisionError::NotJson)
        );
        assert_eq!(
            parse_decision(&wrap("[1,2]"), &[]),
            Err(DecisionError::NotAnObject)
        );
    }

    #[test]
    fn rejects_a_duplicate_key() {
        let text = wrap(
            "{\"decision\":\"APPROVE\",\"head\":\"h\",\"head\":\"h2\",\"evidence_rev\":1,\"rationale\":\"r\"}",
        );
        assert_eq!(
            parse_decision(&text, &[]),
            Err(DecisionError::DuplicateKey("head".to_string()))
        );
    }

    #[test]
    fn rejects_unknown_field_and_variant() {
        let unknown = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1, "rationale": "r", "extra": 1
        }));
        assert_eq!(
            parse_decision(&unknown, &[]),
            Err(DecisionError::UnknownField("extra".to_string()))
        );

        let bad_variant = block_json(serde_json::json!({
            "decision": "MERGE", "head": "h", "evidence_rev": 1, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&bad_variant, &[]),
            Err(DecisionError::UnknownDecision)
        );
    }

    /// **The `null` rule.** `null` counts as PRESENT, so a field that does not apply to the variant
    /// is invalid whether it is absent or null — and a null on an actually-applicable field is
    /// invalid too. MUTATION: treat `null` as absent in the parser and this fails.
    #[test]
    fn rejects_null_for_a_field_that_does_not_apply() {
        // `rerun` does not apply to APPROVE. Present as null must be refused, not ignored.
        let text = wrap(
            "{\"decision\":\"APPROVE\",\"head\":\"h\",\"evidence_rev\":1,\"rerun\":null,\"rationale\":\"r\"}",
        );
        assert_eq!(
            parse_decision(&text, &[]),
            Err(DecisionError::NullField("rerun".to_string()))
        );
        // And an empty OBJECT where it does not apply is the plain not-allowed case.
        let empty_obj = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1, "rerun": {}, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&empty_obj, &[]),
            Err(DecisionError::FieldNotAllowed {
                field: "rerun",
                variant: "APPROVE"
            })
        );
    }

    #[test]
    fn rejects_a_field_that_does_not_apply() {
        for (variant, extra) in [
            ("RERUN_REVIEW", ("route", serde_json::json!({}))),
            (
                "ROUTE_TO_AUTHOR",
                ("rerun", serde_json::json!({"note": "x"})),
            ),
            ("ESCALATE", ("dismiss", serde_json::json!([]))),
            (
                "APPROVE",
                (
                    "escalate",
                    serde_json::json!({"question": "q", "checked": "c"}),
                ),
            ),
        ] {
            let mut obj = serde_json::json!({
                "decision": variant, "head": "h", "evidence_rev": 1, "rationale": "r"
            });
            obj[extra.0] = extra.1;
            let err = parse_decision(&block_json(obj), &[]).expect_err("must refuse");
            assert!(
                matches!(err, DecisionError::FieldNotAllowed { .. }),
                "{variant} + {}: got {err:?}",
                extra.0
            );
        }
    }

    #[test]
    fn rejects_missing_or_empty_required_fields() {
        let missing = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1
        }));
        assert_eq!(
            parse_decision(&missing, &[]),
            Err(DecisionError::MissingField("rationale"))
        );
        let empty = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "  ", "evidence_rev": 1, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&empty, &[]),
            Err(DecisionError::EmptyField("head"))
        );
        let no_decision = block_json(serde_json::json!({
            "head": "h", "evidence_rev": 1, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&no_decision, &[]),
            Err(DecisionError::MissingField("decision"))
        );
    }

    #[test]
    fn rejects_an_empty_list_where_a_list_is_not_allowed() {
        let empty_dismiss = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1, "dismiss": [], "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&empty_dismiss, &[]),
            Err(DecisionError::EmptyList("dismiss"))
        );
        let empty_fix = block_json(serde_json::json!({
            "decision": "ROUTE_TO_AUTHOR", "head": "h", "evidence_rev": 1,
            "route": {"fix": [], "instructions": "x"}, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&empty_fix, &[]),
            Err(DecisionError::EmptyList("route.fix"))
        );
        let empty_reviewers = block_json(serde_json::json!({
            "decision": "RERUN_REVIEW", "head": "h", "evidence_rev": 1,
            "rerun": {"reviewers": []}, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&empty_reviewers, &[]),
            Err(DecisionError::EmptyList("rerun.reviewers"))
        );
    }

    #[test]
    fn rejects_strings_over_their_limits() {
        let long_note = "n".repeat(MAX_NOTE_CHARS + 1);
        let note = block_json(serde_json::json!({
            "decision": "RERUN_REVIEW", "head": "h", "evidence_rev": 1,
            "rerun": {"note": long_note}, "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&note, &[]),
            Err(DecisionError::StringTooLong {
                field: "note",
                max: MAX_NOTE_CHARS
            })
        );
        let long_rationale = "r".repeat(MAX_TEXT_CHARS + 1);
        let rationale = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1, "rationale": long_rationale
        }));
        assert_eq!(
            parse_decision(&rationale, &[]),
            Err(DecisionError::StringTooLong {
                field: "rationale",
                max: MAX_TEXT_CHARS
            })
        );
    }

    #[test]
    fn rejects_unknown_or_wrong_status_finding_revisions() {
        let text = block_json(serde_json::json!({
            "decision": "ROUTE_TO_AUTHOR", "head": "h", "evidence_rev": 1,
            "route": {
                "fix": [{"finding": "alice:F1", "revision": 1}],
                "instructions": "fix"
            },
            "rationale": "r"
        }));
        assert_eq!(
            parse_decision(&text, &[]),
            Err(DecisionError::UnknownFinding("alice:F1@r1".to_string()))
        );
        // A resolved revision is the wrong status.
        assert_eq!(
            parse_decision(
                &text,
                &[known("alice:F1", 1, REVIEW_FINDING_RESOLVED, true)]
            ),
            Err(DecisionError::WrongStatusFinding("alice:F1@r1".to_string()))
        );
        // An open but NON-blocking revision is also wrong.
        assert_eq!(
            parse_decision(&text, &[known("alice:F1", 1, REVIEW_FINDING_OPEN, false)]),
            Err(DecisionError::WrongStatusFinding("alice:F1@r1".to_string()))
        );
        // The right one parses.
        assert!(parse_decision(&text, &[known("alice:F1", 1, REVIEW_FINDING_OPEN, true)]).is_ok());
    }

    /// `dismiss` is allowed on `RERUN_REVIEW`, `ROUTE_TO_AUTHOR` and `APPROVE`, and each parses
    /// with its rationale recorded.
    #[test]
    fn accepts_dismissals_where_they_apply() {
        let known = [known("sol:B8", 1, REVIEW_FINDING_OPEN, true)];
        let dismiss = serde_json::json!([
            {"finding": "sol:B8", "revision": 1, "rationale": "already handled"}
        ]);
        for variant in ["RERUN_REVIEW", "ROUTE_TO_AUTHOR", "APPROVE"] {
            let mut obj = serde_json::json!({
                "decision": variant, "head": "h", "evidence_rev": 1,
                "dismiss": dismiss.clone(), "rationale": "r"
            });
            if variant == "RERUN_REVIEW" {
                obj["rerun"] = serde_json::json!({});
            }
            if variant == "ROUTE_TO_AUTHOR" {
                obj["route"] = serde_json::json!({
                    "fix": [{"finding": "sol:B8", "revision": 1}],
                    "instructions": "x"
                });
            }
            let d = parse_decision(&block_json(obj), &known)
                .unwrap_or_else(|e| panic!("{variant} with a dismissal must parse: {e:?}"));
            assert_eq!(d.dismiss.len(), 1, "{variant}");
            assert_eq!(d.dismiss[0].finding.finding, "sol:B8");
            assert_eq!(d.dismiss[0].finding.revision, 1);
        }
    }

    #[test]
    fn rejects_secret_shaped_posted_text() {
        let text = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1,
            "rationale": "used ghp_0123456789abcdef to fetch"
        }));
        assert_eq!(
            parse_decision(&text, &[]),
            Err(DecisionError::SecretShape("rationale"))
        );
        // Control: a plain 40-hex SHA is not secret-shaped.
        let sha = block_json(serde_json::json!({
            "decision": "APPROVE", "head": "h", "evidence_rev": 1,
            "rationale": "reviewed 87aa04487aa04487aa04487aa04487aa0440000"
        }));
        assert!(parse_decision(&sha, &[]).is_ok());
    }

    // --- §6.2 preconditions -------------------------------------------------------------------

    #[test]
    fn rerun_needs_an_eligible_row() {
        let d = parse_decision(
            &block_json(serde_json::json!({
                "decision": "RERUN_REVIEW", "head": "h", "evidence_rev": 1, "rationale": "r"
            })),
            &[],
        )
        .expect("parses");
        assert_eq!(
            preconditions(&d, &PreconditionInputs { eligible_rows: 0 }),
            Err(DecisionError::NoEligibleRows)
        );
        assert!(preconditions(&d, &PreconditionInputs { eligible_rows: 1 }).is_ok());
    }

    #[test]
    fn a_stale_head_is_refused() {
        let d = parse_decision(
            &block_json(serde_json::json!({
                "decision": "RERUN_REVIEW", "head": "31ee051", "evidence_rev": 1, "rationale": "r"
            })),
            &[],
        )
        .expect("parses");
        assert_eq!(
            head_is_current(&d, "b02fc72"),
            Err(DecisionError::HeadNotCurrent)
        );
        assert!(head_is_current(&d, "31ee051").is_ok());
    }

    #[test]
    fn the_final_intervention_allows_only_approve_or_escalate() {
        let rerun = parse_decision(
            &block_json(serde_json::json!({
                "decision": "RERUN_REVIEW", "head": "h", "evidence_rev": 1, "rationale": "r"
            })),
            &[],
        )
        .expect("parses");
        assert_eq!(
            validate_final(&rerun, true),
            Err(DecisionError::FinalRestriction)
        );
        assert!(validate_final(&rerun, false).is_ok());
        let approve = parse_decision(
            &block_json(serde_json::json!({
                "decision": "APPROVE", "head": "h", "evidence_rev": 1, "rationale": "r"
            })),
            &[],
        )
        .expect("parses");
        assert!(validate_final(&approve, true).is_ok());
    }

    // --- §6.4 eligibility ---------------------------------------------------------------------

    fn row(
        reviewer: &str,
        status: &str,
        completed: Option<ReviewCompleted>,
        diff_covered: bool,
    ) -> ManagerReviewRow {
        ManagerReviewRow {
            reviewer: reviewer.to_string(),
            is_manager: false,
            status: status.to_string(),
            completed,
            diff_covered,
        }
    }

    fn base_approval<'a>(
        rows: &'a [ManagerReviewRow],
        required: &'a [String],
        open_blocking: &'a [FindingRef],
        dismissed: &'a [FindingRef],
    ) -> ApprovalInputs<'a> {
        ApprovalInputs {
            rows,
            required,
            effective_reviewers: 1,
            generation: 1,
            current_patch_id: "pid",
            threshold_reached: true,
            final_intervention: false,
            open_blocking,
            dismissed,
        }
    }

    #[test]
    fn an_approved_eligible_set_passes_all_six() {
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            true,
        )];
        assert_eq!(
            approval_eligibility(&base_approval(&rows, &[], &[], &[])),
            Ok(())
        );
    }

    /// **Mutation guard: the manager is never a reviewer.** A single MANAGER row plus no reviewer
    /// must fail quorum, so counting the manager row as a live reviewer (the mutation) would make
    /// this pass.
    #[test]
    fn the_manager_row_never_satisfies_quorum() {
        let rows = vec![ManagerReviewRow {
            reviewer: "manager".to_string(),
            is_manager: true,
            status: REVIEW_STATUS_IN_FLIGHT.to_string(),
            completed: Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            diff_covered: true,
        }];
        let failed = approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("no");
        assert!(failed.contains(&ApprovalCondition::Quorum));
    }

    #[test]
    fn an_empty_set_is_never_approvable() {
        let failed = approval_eligibility(&base_approval(&[], &[], &[], &[])).expect_err("no");
        assert!(failed.contains(&ApprovalCondition::Quorum));
    }

    #[test]
    fn every_condition_can_fail_by_name() {
        // condition 0
        let good_rows = [row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            true,
        )];
        let mut inputs = base_approval(&good_rows, &[], &[], &[]);
        inputs.threshold_reached = false;
        assert_eq!(
            approval_eligibility(&inputs).expect_err("0"),
            vec![ApprovalCondition::Threshold]
        );

        // condition 1: below quorum
        inputs.threshold_reached = true;
        inputs.effective_reviewers = 2;
        assert!(
            approval_eligibility(&inputs)
                .expect_err("1")
                .contains(&ApprovalCondition::Quorum)
        );

        // condition 2: no completed review in this generation
        let rows = [row("sol", "reviewed", None, true)];
        let failed = approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("2");
        assert!(failed.contains(&ApprovalCondition::EveryRowHadTurn));

        // condition 3: not diff covered
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            false,
        )];
        let failed = approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("3");
        assert!(failed.contains(&ApprovalCondition::DiffCoverage));

        // condition 4: an open blocking finding not dismissed
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            true,
        )];
        let open = [FindingRef {
            finding: "sol:B8".to_string(),
            revision: 1,
        }];
        let failed = approval_eligibility(&base_approval(&rows, &[], &open, &[])).expect_err("4");
        assert!(failed.contains(&ApprovalCondition::FindingsResolved));
        // …and dismissing it in this decision satisfies it.
        assert_eq!(
            approval_eligibility(&base_approval(&rows, &[], &open, &open)),
            Ok(())
        );

        // condition 5: rows have not read the current patch
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "OTHER")),
            true,
        )];
        let failed = approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("5");
        assert!(failed.contains(&ApprovalCondition::RowsAtCurrentPatch));
        // …unless this is the final intervention.
        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.final_intervention = true;
        assert_eq!(approval_eligibility(&inputs), Ok(()));
    }

    #[test]
    fn required_members_must_have_a_live_row() {
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            true,
        )];
        let required = ["jimmy".to_string()];
        let failed = approval_eligibility(&base_approval(&rows, &required, &[], &[]))
            .expect_err("jimmy missing");
        assert!(failed.contains(&ApprovalCondition::Quorum));
    }

    /// **A truncated review does not satisfy condition 2.** A truncated round writes no completion,
    /// so the row has not had its turn however its transient status reads.
    #[test]
    fn a_truncated_review_does_not_count_as_having_had_a_turn() {
        let rows = vec![row(
            "sol",
            rhapsody_store::REVIEW_STATUS_TRUNCATED,
            None,
            false,
        )];
        let failed = approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("no");
        assert!(failed.contains(&ApprovalCondition::EveryRowHadTurn));
    }

    #[test]
    fn a_dropped_row_is_not_live() {
        let rows = vec![row(
            "sol",
            REVIEW_STATUS_DROPPED,
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "sha", "pid")),
            true,
        )];
        let failed =
            approval_eligibility(&base_approval(&rows, &[], &[], &[])).expect_err("no live");
        assert!(failed.contains(&ApprovalCondition::Quorum));
    }

    // --- eligible rows (§6.2 / F5, F6, F9) ----------------------------------------------------

    #[test]
    fn eligible_rows_are_the_ones_not_approved_at_the_current_patch() {
        let rows = vec![
            row(
                "sol",
                "reviewed",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, "old", "OLD_PATCH")),
                true,
            ),
            row(
                "jimmy",
                "approved",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, "head", "pid")),
                true,
            ),
        ];
        let eligible = eligible_rows(&rows, 1, "pid");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].reviewer, "sol", "F5: only sol is re-requested");

        // F9: statuses reset to `requested`, but the completed review still approves the patch.
        let rows = vec![
            row(
                "alice",
                "requested",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, "head", "pid")),
                true,
            ),
            row(
                "bob",
                "requested",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, "head", "pid")),
                true,
            ),
        ];
        assert!(
            eligible_rows(&rows, 1, "pid").is_empty(),
            "F9: no eligible rows"
        );
    }

    #[test]
    fn a_clear_generation_invalidates_the_approval() {
        let rows = vec![row(
            "sol",
            "approved",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "head", "pid")),
            true,
        )];
        assert_eq!(eligible_rows(&rows, 2, "pid").len(), 1);
    }

    // --- §7.8 threshold -----------------------------------------------------------------------

    #[test]
    fn phase_is_computed_from_answered_exchanges() {
        assert_eq!(threshold_phase(2, 3), ThresholdPhase::PreThreshold);
        assert_eq!(threshold_phase(3, 3), ThresholdPhase::PostThreshold);
        assert_eq!(threshold_phase(5, 3), ThresholdPhase::PostThreshold);
        assert_eq!(threshold_phase(0, 0), ThresholdPhase::PostThreshold);
    }

    // --- §8 revalidation ----------------------------------------------------------------------

    fn decision(kind: DecisionKind) -> ManagerDecision {
        ManagerDecision {
            head: "h".to_string(),
            evidence_rev: 10,
            dismiss: Vec::new(),
            rationale: "r".to_string(),
            kind,
        }
    }

    fn base_reval<'a>(d: &'a ManagerDecision) -> RevalidationInputs<'a> {
        RevalidationInputs {
            decision: d,
            before_generation: 1,
            after_generation: 1,
            authority_act: true,
            pr_open: true,
            hold_known: true,
            hold_applied: false,
            manager_enabled: true,
            before_evidence_rev: 10,
            after_evidence_rev: 10,
            patch_id_unchanged: true,
            eligible_rows: 1,
            all_rows_approved: false,
            route_fix_still_open: true,
            review_completed_since: false,
            finding_set_unchanged: true,
            approval_still_eligible: true,
            phase_at_launch: ThresholdPhase::PreThreshold,
            answered_exchanges: 0,
            adjudicate_after_rounds: 5,
            interventions_applied: 0,
            max_interventions: 3,
            final_intervention: false,
        }
    }

    #[test]
    fn supersession_covers_every_section_8_1_cause() {
        let d = decision(DecisionKind::Approve);
        for mutate in [
            (|i: &mut RevalidationInputs| i.after_generation = 2) as fn(&mut RevalidationInputs),
            |i| i.authority_act = false,
            |i| i.pr_open = false,
            |i| i.manager_enabled = false,
            |i| i.hold_known = false,
            |i| i.hold_applied = true,
        ] {
            let mut inputs = base_reval(&d);
            mutate(&mut inputs);
            assert_eq!(
                revalidate(&inputs),
                Revalidation::Superseded,
                "a §8.1 cause must supersede outright"
            );
        }
    }

    #[test]
    fn evidence_unchanged_is_still_valid() {
        let d = decision(DecisionKind::Approve);
        assert_eq!(revalidate(&base_reval(&d)), Revalidation::StillValid);
    }

    #[test]
    fn a_new_patch_id_stales_everything_but_escalate() {
        for kind in [
            DecisionKind::RerunReview {
                reviewers: vec![],
                note: None,
            },
            DecisionKind::RouteToAuthor {
                fix: vec![],
                instructions: "x".to_string(),
            },
            DecisionKind::Approve,
        ] {
            let d = decision(kind);
            let mut inputs = base_reval(&d);
            inputs.patch_id_unchanged = false;
            inputs.after_evidence_rev = 11;
            assert_eq!(revalidate(&inputs), Revalidation::Stale);
        }
        let d = decision(DecisionKind::Escalate {
            question: "q".to_string(),
            checked: "c".to_string(),
        });
        let mut inputs = base_reval(&d);
        inputs.patch_id_unchanged = false;
        inputs.after_evidence_rev = 11;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);
    }

    #[test]
    fn rerun_revalidation_complete_or_stale() {
        let d = decision(DecisionKind::RerunReview {
            reviewers: vec![],
            note: None,
        });
        let mut inputs = base_reval(&d);
        inputs.after_evidence_rev = 11;
        inputs.eligible_rows = 1;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);

        inputs.eligible_rows = 0;
        inputs.all_rows_approved = true;
        assert_eq!(revalidate(&inputs), Revalidation::Complete);

        inputs.all_rows_approved = false;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);
    }

    #[test]
    fn route_revalidation_needs_the_fix_still_open() {
        let d = decision(DecisionKind::RouteToAuthor {
            fix: vec![],
            instructions: "x".to_string(),
        });
        let mut inputs = base_reval(&d);
        inputs.after_evidence_rev = 11;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);
        inputs.route_fix_still_open = false;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);
    }

    #[test]
    fn approve_revalidation_needs_everything_unchanged_and_eligible() {
        let d = decision(DecisionKind::Approve);
        let mut inputs = base_reval(&d);
        inputs.after_evidence_rev = 11;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);

        inputs.review_completed_since = true;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);
        inputs.review_completed_since = false;

        inputs.finding_set_unchanged = false;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);
        inputs.finding_set_unchanged = true;

        inputs.approval_still_eligible = false;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);
    }

    /// **§7.8, the threshold-crossing mutation.** The decision was PRE-threshold at launch but the
    /// count crossed the threshold before activation, at `applied = N − 1`, and the decision is not
    /// final. An implementation charging the LAUNCH phase would leave this `StillValid`; the
    /// classification is made at activation, so it is `Stale` (re-plan as final).
    #[test]
    fn a_threshold_crossing_at_the_last_slot_stales_a_non_final_runtime_decision() {
        let d = decision(DecisionKind::RerunReview {
            reviewers: vec![],
            note: None,
        });
        let mut inputs = base_reval(&d);
        inputs.phase_at_launch = ThresholdPhase::PreThreshold;
        inputs.answered_exchanges = 5; // crossed
        inputs.adjudicate_after_rounds = 5;
        inputs.interventions_applied = 2; // = max_interventions - 1
        inputs.max_interventions = 3;
        inputs.final_intervention = false;
        assert_eq!(revalidate(&inputs), Revalidation::Stale);

        // Launched final: allowed.
        inputs.final_intervention = true;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);

        // Not at the last slot: allowed.
        inputs.final_intervention = false;
        inputs.interventions_applied = 0;
        assert_eq!(revalidate(&inputs), Revalidation::StillValid);
    }

    // --- §15.2 tier-2 fixtures F1–F9 ----------------------------------------------------------

    /// Build a `RERUN_REVIEW` decision and its acceptance.
    fn rerun_decision(head: &str, note: Option<&str>) -> ManagerDecision {
        parse_decision(
            &block_json(serde_json::json!({
                "decision": "RERUN_REVIEW",
                "head": head,
                "evidence_rev": 1,
                "rerun": note.map(|n| serde_json::json!({"note": n})).unwrap_or(serde_json::json!({})),
                "rationale": "reason"
            })),
            &[],
        )
        .expect("rerun parses")
    }

    /// F1 `#212`: rows last completed at `829a28f`; head `87aa044`; before threshold.
    /// Accept `RERUN_REVIEW`; reject `APPROVE` on condition 0.
    #[test]
    fn f1_before_threshold() {
        let head = "87aa044";
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(
                rhapsody_store::REVIEW_COMPLETION_CHANGES,
                1,
                "829a28f",
                "old",
            )),
            true,
        )];
        let d = rerun_decision(head, None);
        assert!(head_is_current(&d, head).is_ok());
        assert_eq!(eligible_rows(&rows, 1, "cur").len(), 1);
        assert!(preconditions(&d, &PreconditionInputs { eligible_rows: 1 }).is_ok());

        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.current_patch_id = "cur";
        inputs.threshold_reached = false;
        let failed = approval_eligibility(&inputs).expect_err("F1 rejects APPROVE");
        assert!(failed.contains(&ApprovalCondition::Threshold), "{failed:?}");
    }

    /// F2 `#213`: rows at `ad09f06`/`60af0ff`; head `568f8fa`; threshold reached, not final.
    /// Accept `RERUN_REVIEW`; reject `APPROVE` on condition 5.
    #[test]
    fn f2_rows_have_not_read_the_current_patch() {
        let head = "568f8fa";
        let rows = vec![
            row(
                "sol",
                "reviewed",
                Some(completion(
                    rhapsody_store::REVIEW_COMPLETION_CHANGES,
                    1,
                    "ad09f06",
                    "p-old-1",
                )),
                true,
            ),
            row(
                "jimmy",
                "reviewed",
                Some(completion(
                    rhapsody_store::REVIEW_COMPLETION_CHANGES,
                    1,
                    "60af0ff",
                    "p-old-2",
                )),
                true,
            ),
        ];
        let d = rerun_decision(head, None);
        assert_eq!(eligible_rows(&rows, 1, "p-new").len(), 2);
        assert!(preconditions(&d, &PreconditionInputs { eligible_rows: 2 }).is_ok());

        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.current_patch_id = "p-new";
        inputs.final_intervention = false;
        let failed = approval_eligibility(&inputs).expect_err("F2 rejects APPROVE");
        assert!(
            failed.contains(&ApprovalCondition::RowsAtCurrentPatch),
            "{failed:?}"
        );
    }

    /// F3 `#216`: rows at `375349d`; head `2b09f2a`; threshold reached, not final. Same as F2.
    #[test]
    fn f3_rows_have_not_read_the_current_patch() {
        let head = "2b09f2a";
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(
                rhapsody_store::REVIEW_COMPLETION_CHANGES,
                1,
                "375349d",
                "p-old",
            )),
            true,
        )];
        let d = rerun_decision(head, None);
        assert!(head_is_current(&d, head).is_ok());
        assert_eq!(eligible_rows(&rows, 1, "p-new").len(), 1);
        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.current_patch_id = "p-new";
        let failed = approval_eligibility(&inputs).expect_err("F3 rejects APPROVE");
        assert!(failed.contains(&ApprovalCondition::RowsAtCurrentPatch));
    }

    /// F4 `#213`: rows at `d196719`; head `c589e1f`; no open finding at head.
    /// Accept `RERUN_REVIEW` with a `note`; reject `ROUTE_TO_AUTHOR` naming no open revision.
    #[test]
    fn f4_route_to_author_names_no_open_revision() {
        let head = "c589e1f";
        let d = rerun_decision(head, Some("please look again"));
        assert!(head_is_current(&d, head).is_ok());

        let route = parse_decision(
            &block_json(serde_json::json!({
                "decision": "ROUTE_TO_AUTHOR",
                "head": head,
                "evidence_rev": 1,
                "route": {"fix": [{"finding": "sol:B8", "revision": 1}], "instructions": "fix"},
                "rationale": "r"
            })),
            &[],
        );
        assert_eq!(
            route,
            Err(DecisionError::UnknownFinding("sol:B8@r1".to_string())),
            "F4: no open finding at head, so route.fix names nothing valid"
        );
    }

    /// F5 `#212`: sol's last completed review is older; jimmy approved at the current patch.
    /// Accept `RERUN_REVIEW`; the effect re-requests sol only.
    #[test]
    fn f5_only_the_unapproved_row_is_eligible() {
        let rows = vec![
            row(
                "sol",
                "reviewed",
                Some(completion(
                    rhapsody_store::REVIEW_COMPLETION_CHANGES,
                    1,
                    "old",
                    "OLD",
                )),
                true,
            ),
            row(
                "jimmy",
                "approved",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, "cur", "pid")),
                true,
            ),
        ];
        let eligible = eligible_rows(&rows, 1, "pid");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].reviewer, "sol");
    }

    /// F6 `#213`: both last completed `approve` at `e2c52c1`; head `d17d0b7`; identical patch-id;
    /// threshold reached. Accept `APPROVE`; reject `RERUN_REVIEW` (no eligible rows).
    #[test]
    fn f6_identical_patch_id_is_approved() {
        let rows = vec![
            row(
                "sol",
                "approved",
                Some(completion(
                    REVIEW_COMPLETION_APPROVE,
                    1,
                    "e2c52c1",
                    "same-pid",
                )),
                true,
            ),
            row(
                "jimmy",
                "approved",
                Some(completion(
                    REVIEW_COMPLETION_APPROVE,
                    1,
                    "e2c52c1",
                    "same-pid",
                )),
                true,
            ),
        ];
        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.current_patch_id = "same-pid";
        assert_eq!(
            approval_eligibility(&inputs),
            Ok(()),
            "F6: every row approved at the current patch"
        );
        assert!(
            eligible_rows(&rows, 1, "same-pid").is_empty(),
            "F6: no eligible rows, so RERUN_REVIEW is invalid"
        );
        let d = rerun_decision("d17d0b7", None);
        assert_eq!(
            preconditions(&d, &PreconditionInputs { eligible_rows: 0 }),
            Err(DecisionError::NoEligibleRows)
        );
    }

    /// F7 `#214`: `alice:F1@r1`, `alice:F2@r1`, `jimmy:B5@r1` open and blocking.
    /// Accept `ROUTE_TO_AUTHOR` whose `fix` names all three; reject an unknown/resolved revision and
    /// an `APPROVE` that does not dismiss them.
    #[test]
    fn f7_route_must_name_every_open_revision() {
        let head = "h";
        let findings = vec![
            known("alice:F1", 1, REVIEW_FINDING_OPEN, true),
            known("alice:F2", 1, REVIEW_FINDING_OPEN, true),
            known("jimmy:B5", 1, REVIEW_FINDING_OPEN, true),
        ];
        let good = parse_decision(
            &block_json(serde_json::json!({
                "decision": "ROUTE_TO_AUTHOR", "head": head, "evidence_rev": 1,
                "route": {"fix": [
                    {"finding": "alice:F1", "revision": 1},
                    {"finding": "alice:F2", "revision": 1},
                    {"finding": "jimmy:B5", "revision": 1}
                ], "instructions": "address all three"},
                "rationale": "three findings"
            })),
            &findings,
        );
        assert!(good.is_ok(), "F7: a route naming all three is accepted");

        let resolved = parse_decision(
            &block_json(serde_json::json!({
                "decision": "ROUTE_TO_AUTHOR", "head": head, "evidence_rev": 1,
                "route": {"fix": [{"finding": "alice:F1", "revision": 1}], "instructions": "x"},
                "rationale": "r"
            })),
            &[known("alice:F1", 1, REVIEW_FINDING_RESOLVED, true)],
        );
        assert_eq!(
            resolved,
            Err(DecisionError::WrongStatusFinding("alice:F1@r1".to_string()))
        );

        // APPROVE without dismissing the open blocking findings fails condition 4.
        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(REVIEW_COMPLETION_APPROVE, 1, "h", "pid")),
            true,
        )];
        let open = [
            FindingRef {
                finding: "alice:F1".to_string(),
                revision: 1,
            },
            FindingRef {
                finding: "alice:F2".to_string(),
                revision: 1,
            },
            FindingRef {
                finding: "jimmy:B5".to_string(),
                revision: 1,
            },
        ];
        let failed = approval_eligibility(&base_approval(&rows, &[], &open, &[])).expect_err("F7");
        assert!(failed.contains(&ApprovalCondition::FindingsResolved));
    }

    /// F8 `#210`: head `b02fc72`, sol last completed `0052489`. Accept `RERUN_REVIEW` at the current
    /// head; reject a decision naming head `31ee051` (stale head) and `APPROVE` (condition 5).
    #[test]
    fn f8_stale_head_and_unread_current_patch() {
        let current = "b02fc72";
        let stale = rerun_decision("31ee051", None);
        assert_eq!(
            head_is_current(&stale, current),
            Err(DecisionError::HeadNotCurrent)
        );

        let d = rerun_decision(current, None);
        assert!(head_is_current(&d, current).is_ok());

        let rows = vec![row(
            "sol",
            "reviewed",
            Some(completion(
                rhapsody_store::REVIEW_COMPLETION_CHANGES,
                1,
                "0052489",
                "p-old",
            )),
            true,
        )];
        let mut inputs = base_approval(&rows, &[], &[], &[]);
        inputs.current_patch_id = "p-new";
        let failed = approval_eligibility(&inputs).expect_err("F8 rejects APPROVE");
        assert!(failed.contains(&ApprovalCondition::RowsAtCurrentPatch));
    }

    /// F9 `#216`: all three last completed `approve` at `e4f615e` = head; statuses reset to
    /// `requested`. Accept `APPROVE` (condition 5 holds per §5.4); reject `RERUN_REVIEW`.
    #[test]
    fn f9_reintroduction_approval_survives() {
        let head = "e4f615e";
        let rows = vec![
            row(
                "alice",
                "requested",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, head, "pid")),
                true,
            ),
            row(
                "bob",
                "requested",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, head, "pid")),
                true,
            ),
            row(
                "carol",
                "requested",
                Some(completion(REVIEW_COMPLETION_APPROVE, 1, head, "pid")),
                true,
            ),
        ];
        assert_eq!(
            approval_eligibility(&base_approval(&rows, &[], &[], &[])),
            Ok(()),
            "F9: the recorded completed review approves the current patch whatever status says"
        );
        assert!(
            eligible_rows(&rows, 1, "pid").is_empty(),
            "F9: no eligible rows"
        );
        let d = rerun_decision(head, None);
        assert_eq!(
            preconditions(&d, &PreconditionInputs { eligible_rows: 0 }),
            Err(DecisionError::NoEligibleRows)
        );
    }

    /// A dismissal is recorded with its rationale and never stands alone: the parser requires a
    /// `decision`, and every variant that carries a dismissal still carries its own name.
    #[test]
    fn dismiss_is_never_a_decision_on_its_own() {
        let text = wrap(
            "{\"dismiss\":[{\"finding\":\"sol:B8\",\"revision\":1,\"rationale\":\"r\"}],\"rationale\":\"r\",\"head\":\"h\",\"evidence_rev\":1}",
        );
        assert_eq!(
            parse_decision(&text, &[known("sol:B8", 1, REVIEW_FINDING_OPEN, true)]),
            Err(DecisionError::MissingField("decision"))
        );
    }
}
