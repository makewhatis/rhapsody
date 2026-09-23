//! selection — the PURE harness/provider/model resolver (STUDIO-986; design record
//! `~/.rhapsody/docs/provider-auth-design.md` §2.3 / slice P4).
//!
//! Resolution is **field-wise**: each of `harness`, `provider` and `model` walks the same six-tier
//! precedence chain independently, so a ticket that names only a model still inherits its harness
//! and provider from the tiers below its own:
//!
//! ```text
//! 1. ticket                (the rhapsody:harness|provider|model labels, `rhapsody_config::routing`)
//! 2. per-harness review override   (review runs only, scoped to the harness the run resolved to)
//! 3. role/profile          (`profiles::ResolvedProfile`)
//! 4. teammate identity     (the roster entry; STUDIO-985)
//! 5. project effective default      (the ordinary config after the per-project overlay)
//! 6. global default        (`agent.backend` / `agent.provider` / `agent.model`)
//! ```
//!
//! The result is a fully owned [`ResolvedSelection`] carrying each field's [`Origin`] and, when an
//! explicit provider was selected, the pure non-secret [`ResolvedProviderPlan`] P5/PB5 consume. A
//! raw reusable API key is unrepresentable here, exactly as it is in the plan itself.
//!
//! # Purity is the contract
//!
//! This module performs **no I/O**: no Keychain/IPC, no version subprocess, no broker, no
//! filesystem, no tracker, and no network. Every input it needs is a borrowed, owned, non-secret
//! value handed in by the caller ([`SelectionRequest`]); there is deliberately no handle it could
//! use to read a credential or spawn a child. `resolve_selection` therefore cannot be the place a
//! provider is silently contacted, and a refusal it returns is a first-class value the caller
//! records before any spawn. The credential check is a LATER prepared-dispatch stage (PB7).
//!
//! # No fallback, ever
//!
//! An unknown or unimplemented harness is a typed [`SelectionRefusal::HarnessNotImplemented`]; it
//! never falls back to `agent.backend`. An explicit provider whose protocol the selected harness's
//! adapter cannot consume is [`SelectionRefusal::UnsupportedProtocol`]; a provider with no exact
//! model is [`SelectionRefusal::MissingModel`]. Every refusal is actionable text and none of them
//! is a silent downgrade to another harness, provider, model, or auth source.
//!
//! # What this slice owns (and what it does not)
//!
//! P4 produces VALUES plus ORIGINS and no I/O. It produces the pure [`ResolvedProviderPlan`] only;
//! lowering that plan into a move-only prepared broker session and dispatch custody is B5's
//! (`crates/provider-broker`), and manager/teammate execution wiring is P8. `harness_knobs` are not
//! constructed here: building a `HarnessKnobs` block is the config→agent materialization
//! `effective.rs` already owns, and reaching into it here would drag the tracker credential into a
//! type this module must keep clean.

use std::collections::BTreeMap;

use rhapsody_agent::{
    HarnessId, ProviderLimits, ProviderOrigins, ProviderProtocol, ResolvedProviderPlan,
    harness_id_for_name, harness_supports_protocol,
};
use rhapsody_config::providers::{
    BrokerLimits, ProviderDefinition, canonical_provider_id, validate_model_id,
};
use rhapsody_config::routing::{RoutingLabelError, TicketSelection, parse_ticket_selection};

/// Which tier supplied a resolved field. Values are stable, non-secret surface names used for
/// provenance/diagnostics; the same spelling reaches the console and the run ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The ticket's `rhapsody:harness|provider|model` label.
    Ticket,
    /// A per-harness `review.provider`/`review.model` override, on a review run.
    Review,
    /// The routed teammate's role/profile.
    Profile,
    /// The routed teammate's roster identity.
    Identity,
    /// The target project's effective default.
    Project,
    /// The installation-wide `agent.*` default.
    Global,
    /// The manager's own explicit tuple (never a teammate's).
    Manager,
    /// An implicit default with no configured value behind it (the manager's Claude default).
    Default,
}

impl Origin {
    /// The stable, non-secret provenance spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::Review => "review",
            Self::Profile => "profile",
            Self::Identity => "identity",
            Self::Project => "project",
            Self::Global => "global",
            Self::Manager => "manager",
            Self::Default => "default",
        }
    }
}

/// One tier's raw harness/provider/model selection. Empty strings mean "absent, inherit" — the
/// field-wise resolution walks to the next tier for exactly that field and no other, which is what
/// keeps a partially-filled tier from dragging its siblings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldSelection {
    pub harness: String,
    pub provider: String,
    pub model: String,
}

impl FieldSelection {
    /// Whether the tier names nothing at all.
    pub fn is_empty(&self) -> bool {
        self.harness.is_empty() && self.provider.is_empty() && self.model.is_empty()
    }

    /// The tier a fully-configured config names: `agent.backend`/`agent.provider`/`agent.model`.
    pub fn from_agent(agent: &rhapsody_config::Agent) -> FieldSelection {
        FieldSelection {
            harness: agent.backend.clone(),
            provider: agent.provider.clone(),
            model: agent.model.clone(),
        }
    }

    /// The ticket tier a parsed label selection contributes. Each field is independently `None`-or-
    /// present, which is what makes the ticket an ordinary field-wise tier rather than a whole-tuple
    /// override.
    pub fn from_ticket(sel: &TicketSelection) -> FieldSelection {
        FieldSelection {
            harness: sel.harness.clone().unwrap_or_default(),
            provider: sel.provider.clone().unwrap_or_default(),
            model: sel.model.clone().unwrap_or_default(),
        }
    }
}

/// The per-harness review override, for a review run only. `harness` is the harness the override
/// was written for; the resolver refuses when it does not match the harness the run actually
/// resolved to, so a review configured for one harness is never applied to another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewOverride {
    pub harness: String,
    pub provider: String,
    pub model: String,
}

/// The six tiers, each a plain non-secret selection. The review tier is `None` for an
/// implementation run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionTiers {
    pub ticket: FieldSelection,
    /// Review runs only. Already the per-harness override for the reviewer's harness.
    pub review: Option<ReviewOverride>,
    pub profile: FieldSelection,
    pub identity: FieldSelection,
    pub project: FieldSelection,
    pub global: FieldSelection,
}

/// Everything a resolution needs, all borrowed and non-secret.
pub struct SelectionRequest<'a> {
    pub tiers: SelectionTiers,
    /// The provider registry the selected provider id is looked up in: the target project's
    /// effective provider set after the normal overlay.
    pub providers: &'a BTreeMap<String, ProviderDefinition>,
    /// The selected harness's effective turn deadline in milliseconds, used only to lower a
    /// provider's configured broker limits into the plan's explicit values.
    pub turn_deadline_ms: u64,
}

/// Where each resolved field came from. `provider`/`model` are `None` only when the field resolved
/// to nothing at all (a legacy/native-login harness with no explicit model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionOrigins {
    pub harness: Origin,
    pub provider: Option<Origin>,
    pub model: Option<Origin>,
}

/// The fully owned resolution: the harness (typed + its recorded name), the stable provider id when
/// one was selected (`""` for the legacy/native-login path), the pure non-secret provider plan, the
/// model, and every field's origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub harness: HarnessId,
    /// The harness NAME (`"claude"`/`"opencode"`), kept beside the typed id so provenance and
    /// diagnostics can record the selection without re-deriving a string from the enum.
    pub harness_name: String,
    /// The canonical provider id, or `""` when no explicit Rhapsody provider was selected.
    pub provider_id: String,
    /// The pure plan for an explicit provider; `None` on the legacy/native-login branch.
    pub provider: Option<ResolvedProviderPlan>,
    /// The exact model, or `None` when the branch deliberately preserves the CLI default.
    pub model: Option<String>,
    pub origins: SelectionOrigins,
}

/// A typed, actionable selection refusal. Every variant is a first-class outcome recorded before a
/// child can be spawned — never a fallback to another harness/provider/model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionRefusal {
    /// Nothing in any tier named a harness, so there is no safe default to run.
    #[error("selection_refusal: no harness is configured in any tier")]
    MissingHarness,
    /// A harness this build has no runner for (known-but-unimplemented, e.g. `codex`, or unknown).
    #[error(
        "selection_refusal: harness {name:?} is not implemented by this build; refusing rather than \
         falling back to another harness"
    )]
    HarnessNotImplemented { name: String },
    /// A provider id outside §2.2's canonical syntax.
    #[error("selection_refusal: provider {id:?} is not a canonical provider id: {reason}")]
    InvalidProvider { id: String, reason: String },
    /// A provider id that is not in the target project's effective registry.
    #[error("selection_refusal: provider {id:?} is not configured for this project")]
    UnknownProvider { id: String },
    /// The selected harness's adapter cannot consume the provider's protocol.
    #[error(
        "selection_refusal: harness {harness:?} cannot consume provider protocol {protocol:?}; \
         refusing rather than falling back to another provider or harness"
    )]
    UnsupportedProtocol { harness: String, protocol: String },
    /// An explicit provider with no exact model.
    #[error("selection_refusal: an explicit provider requires an exact model")]
    MissingModel,
    /// A model outside §2.2's transport bounds.
    #[error("selection_refusal: model {model:?} is not a valid model id: {reason}")]
    InvalidModel { model: String, reason: String },
    /// A review override configured for a harness the run did not resolve to.
    #[error(
        "selection_refusal: the review override is configured for harness {configured:?}, but this \
         review resolved to {selected:?}; refusing rather than applying it to the wrong run"
    )]
    ReviewHarnessMismatch {
        configured: String,
        selected: String,
    },
    /// A non-Claude manager harness that names no provider.
    #[error(
        "selection_refusal: the manager harness {harness:?} must name an explicit provider; the \
         default Claude manager is the only native-login manager"
    )]
    ManagerProviderRequired { harness: String },
    /// A non-Claude manager harness that names no exact model.
    #[error("selection_refusal: the manager harness {harness:?} must name an exact model")]
    ManagerModelRequired { harness: String },
    /// A reserved ticket routing label is malformed, over-length, or two distinct labels conflict.
    /// Surfaced through the resolver so a conflict is refused in the same first-class way as every
    /// other selection error, before any lower tier is consulted.
    #[error("selection_refusal: {0}")]
    RoutingLabel(#[from] RoutingLabelError),
}

/// Resolves one dispatch's harness/provider/model field-wise (§2.3) and refuses exactly the
/// combinations no adapter can honor. Pure: no I/O of any kind.
pub fn resolve_selection(
    req: &SelectionRequest<'_>,
) -> Result<ResolvedSelection, SelectionRefusal> {
    let t = &req.tiers;

    // Harness: ticket > profile > identity > project > global. A review override never supplies the
    // harness; it is SCOPED BY it (that is the per-harness half of the review tier).
    let harness_pair = pick(&[
        (t.ticket.harness.as_str(), Origin::Ticket),
        (t.profile.harness.as_str(), Origin::Profile),
        (t.identity.harness.as_str(), Origin::Identity),
        (t.project.harness.as_str(), Origin::Project),
        (t.global.harness.as_str(), Origin::Global),
    ])
    .ok_or(SelectionRefusal::MissingHarness)?;

    // The review tier, field-wise, refusing a mismatch before it can contribute a value.
    let review_provider = review_field(t, &harness_pair.0, |r| &r.provider)?;
    let review_model = review_field(t, &harness_pair.0, |r| &r.model)?;

    // Provider/model: ticket > review > profile > identity > project > global, each field on its
    // own. The review tier slots in between the ticket and the profile exactly as §2.3 orders it.
    let provider_pair = pick(&[(t.ticket.provider.as_str(), Origin::Ticket)])
        .or(review_provider)
        .or_else(|| {
            pick(&[
                (t.profile.provider.as_str(), Origin::Profile),
                (t.identity.provider.as_str(), Origin::Identity),
                (t.project.provider.as_str(), Origin::Project),
                (t.global.provider.as_str(), Origin::Global),
            ])
        });
    let model_pair = pick(&[(t.ticket.model.as_str(), Origin::Ticket)])
        .or(review_model)
        .or_else(|| {
            pick(&[
                (t.profile.model.as_str(), Origin::Profile),
                (t.identity.model.as_str(), Origin::Identity),
                (t.project.model.as_str(), Origin::Project),
                (t.global.model.as_str(), Origin::Global),
            ])
        });

    finish_selection(
        harness_pair.0,
        harness_pair.1,
        provider_pair,
        model_pair,
        req.providers,
        req.turn_deadline_ms,
    )
}

/// The label-first entry point: parse the ticket's reserved `rhapsody:harness|provider|model`
/// labels, fold them into the ticket tier, and resolve. A malformed, over-length, or conflicting set
/// is refused here — the ticket-label tier's own refusal — before any lower tier is consulted. This
/// is the single pure call the dispatch path makes, so there is no second place that could re-read
/// the labels and disagree.
pub fn resolve_ticket_labels(
    labels: &[String],
    tiers: SelectionTiers,
    providers: &BTreeMap<String, ProviderDefinition>,
    turn_deadline_ms: u64,
) -> Result<ResolvedSelection, SelectionRefusal> {
    let parsed = parse_ticket_selection(labels)?;
    let mut tiers = tiers;
    tiers.ticket = FieldSelection::from_ticket(&parsed);
    resolve_selection(&SelectionRequest {
        tiers,
        providers,
        turn_deadline_ms,
    })
}

/// Resolves the MANAGER's own tuple. Independent of every teammate: it reads only the manager's
/// configured fields, so an installation whose teammates all moved to another harness still gets the
/// Claude manager D6 specifies. Absent `harness` means `claude`; absent `model` preserves the CLI
/// default (it does NOT inherit `claude.model` or any teammate's model). An explicit non-Claude
/// harness must name both a provider and a model.
pub fn resolve_manager_selection(
    manager: &FieldSelection,
    providers: &BTreeMap<String, ProviderDefinition>,
    turn_deadline_ms: u64,
) -> Result<ResolvedSelection, SelectionRefusal> {
    let harness_pair = if manager.harness.is_empty() {
        ("claude".to_string(), Origin::Default)
    } else {
        (manager.harness.clone(), Origin::Manager)
    };
    let harness = harness_id_for_name(&harness_pair.0).ok_or_else(|| {
        SelectionRefusal::HarnessNotImplemented {
            name: harness_pair.0.clone(),
        }
    })?;
    let provider_pair =
        (!manager.provider.is_empty()).then(|| (manager.provider.clone(), Origin::Manager));
    let model_pair = (!manager.model.is_empty()).then(|| (manager.model.clone(), Origin::Manager));
    if harness != HarnessId::Claude {
        if provider_pair.is_none() {
            return Err(SelectionRefusal::ManagerProviderRequired {
                harness: harness_pair.0.clone(),
            });
        }
        if model_pair.is_none() {
            return Err(SelectionRefusal::ManagerModelRequired {
                harness: harness_pair.0.clone(),
            });
        }
    }
    finish_selection(
        harness_pair.0,
        harness_pair.1,
        provider_pair,
        model_pair,
        providers,
        turn_deadline_ms,
    )
}

/// The shared tail: validate the harness name, resolve an explicit provider into a pure
/// [`ResolvedProviderPlan`] (checking the A2/H5 registry for protocol compatibility), and validate
/// an explicit model. Used by both the ticket/review path and the manager path so the two cannot
/// disagree about compatibility.
fn finish_selection(
    harness_name: String,
    harness_origin: Origin,
    provider_pair: Option<(String, Origin)>,
    model_pair: Option<(String, Origin)>,
    providers: &BTreeMap<String, ProviderDefinition>,
    turn_deadline_ms: u64,
) -> Result<ResolvedSelection, SelectionRefusal> {
    let harness = harness_id_for_name(&harness_name).ok_or_else(|| {
        SelectionRefusal::HarnessNotImplemented {
            name: harness_name.clone(),
        }
    })?;

    // A model with no explicit provider is the legacy/native-login branch: the CLI default is
    // preserved when absent, and an explicitly-named model only has to satisfy the transport bounds.
    let Some((provider_id, provider_origin)) = provider_pair else {
        if let Some((model, _)) = &model_pair {
            validate_model_id(model).map_err(|reason| SelectionRefusal::InvalidModel {
                model: model.clone(),
                reason,
            })?;
        }
        let model = model_pair.clone().map(|(m, _)| m);
        let model_origin = model_pair.map(|(_, o)| o);
        return Ok(ResolvedSelection {
            harness,
            harness_name,
            provider_id: String::new(),
            provider: None,
            model,
            origins: SelectionOrigins {
                harness: harness_origin,
                provider: None,
                model: model_origin,
            },
        });
    };

    // The exact model is mandatory on the explicit-provider branch, and is validated before the
    // registry lookup so a malformed model refuses with its own reason.
    let (model, model_origin) = model_pair.ok_or(SelectionRefusal::MissingModel)?;
    validate_model_id(&model).map_err(|reason| SelectionRefusal::InvalidModel {
        model: model.clone(),
        reason,
    })?;

    canonical_provider_id(&provider_id).map_err(|reason| SelectionRefusal::InvalidProvider {
        id: provider_id.clone(),
        reason,
    })?;
    let def = providers
        .get(&provider_id)
        .ok_or_else(|| SelectionRefusal::UnknownProvider {
            id: provider_id.clone(),
        })?;

    // The registry is the ONE compatibility switch: the harness adapter declares which protocols it
    // can consume. An unknown protocol name and an unsupported harness/protocol pair are the same
    // typed refusal, because neither is consumable.
    let protocol = ProviderProtocol::from_name(&def.protocol).ok_or_else(|| {
        SelectionRefusal::UnsupportedProtocol {
            harness: harness_name.clone(),
            protocol: def.protocol.clone(),
        }
    })?;
    if !harness_supports_protocol(harness, protocol) {
        return Err(SelectionRefusal::UnsupportedProtocol {
            harness: harness_name.clone(),
            protocol: def.protocol.clone(),
        });
    }

    // The plan carries only stable, non-secret metadata plus the canonical binding identity — never
    // the credential itself (which is read by PB7's prepared-dispatch stage).
    let normalized_endpoint =
        def.normalized_base_url()
            .map_err(|reason| SelectionRefusal::InvalidProvider {
                id: provider_id.clone(),
                reason,
            })?;
    let credential_binding = def
        .credential_binding()
        .map_err(|reason| SelectionRefusal::InvalidProvider {
            id: provider_id.clone(),
            reason,
        })?
        .identity();
    let plan = ResolvedProviderPlan {
        stable_id: provider_id.clone(),
        protocol,
        normalized_endpoint,
        allow_insecure_http: def.allow_insecure_http,
        credential_binding,
        credential_ref: def.credential.source.clone(),
        limits: lower_limits(&def.broker_limits, turn_deadline_ms),
        model: model.clone(),
        origins: ProviderOrigins {
            provider: provider_origin.as_str().to_string(),
            model: model_origin.as_str().to_string(),
        },
    };
    Ok(ResolvedSelection {
        harness,
        harness_name,
        provider_id,
        provider: Some(plan),
        model: Some(model),
        origins: SelectionOrigins {
            harness: harness_origin,
            provider: Some(provider_origin),
            model: Some(model_origin),
        },
    })
}

/// The per-harness review tier for one field: `Ok(None)` when unset, a refusal when it is configured
/// for a DIFFERENT harness than the run resolved to, else the value plus its [`Origin::Review`].
/// The mismatch check is per field, so a review block that configures neither provider nor model
/// cannot refuse a run merely because it names a harness somewhere else.
fn review_field(
    tiers: &SelectionTiers,
    selected_harness: &str,
    field: impl Fn(&ReviewOverride) -> &String,
) -> Result<Option<(String, Origin)>, SelectionRefusal> {
    let Some(review) = &tiers.review else {
        return Ok(None);
    };
    let value = field(review);
    if value.is_empty() {
        return Ok(None);
    }
    if !review.harness.is_empty() && review.harness != selected_harness {
        return Err(SelectionRefusal::ReviewHarnessMismatch {
            configured: review.harness.clone(),
            selected: selected_harness.to_string(),
        });
    }
    Ok(Some((value.clone(), Origin::Review)))
}

/// The first non-empty candidate, with its origin. Field-wise inheritance is this function: a tier
/// that supplies nothing for one field simply does not appear for that field.
fn pick(candidates: &[(&str, Origin)]) -> Option<(String, Origin)> {
    candidates
        .iter()
        .find(|(value, _)| !value.is_empty())
        .map(|(value, origin)| ((*value).to_string(), *origin))
}

/// Lowers a config [`BrokerLimits`] block into the plan's explicit [`ProviderLimits`], resolving the
/// optional capability lifetime against the selected harness's turn deadline exactly as the config
/// layer does — so the plan PB5 consumes carries no unresolved `Option`.
fn lower_limits(limits: &BrokerLimits, turn_deadline_ms: u64) -> ProviderLimits {
    ProviderLimits {
        forwarded_requests_per_turn: limits.forwarded_requests_per_turn,
        denied_requests_before_revocation: limits.denied_requests_before_revocation,
        concurrent_upstream_requests_per_turn: limits.concurrent_upstream_requests_per_turn,
        json_request_bytes: limits.json_request_bytes,
        aggregate_request_bytes_per_turn: limits.aggregate_request_bytes_per_turn,
        response_bytes_per_request: limits.response_bytes_per_request,
        aggregate_response_bytes_per_turn: limits.aggregate_response_bytes_per_turn,
        requested_output_tokens_per_request: limits.requested_output_tokens_per_request,
        reserved_token_units_per_turn: limits.reserved_token_units_per_turn,
        reserved_token_units_per_session: limits.reserved_token_units_per_session,
        capability_lifetime_ms: limits.effective_capability_lifetime_ms(turn_deadline_ms),
        max_reserved_token_units_per_utc_day: limits.max_reserved_token_units_per_utc_day,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_config::providers::CredentialSource;

    const DEADLINE: u64 = 3_600_000;

    fn def(id: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.to_string(),
            protocol: rhapsody_config::PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: String::new(),
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            allow_insecure_http: false,
            credential: CredentialSource {
                source: rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: BrokerLimits::default(),
        }
    }

    fn registry(defs: &[ProviderDefinition]) -> BTreeMap<String, ProviderDefinition> {
        defs.iter().map(|d| (d.id.clone(), d.clone())).collect()
    }

    fn field(harness: &str, provider: &str, model: &str) -> FieldSelection {
        FieldSelection {
            harness: harness.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    fn resolve(
        tiers: SelectionTiers,
        providers: &BTreeMap<String, ProviderDefinition>,
    ) -> Result<ResolvedSelection, SelectionRefusal> {
        resolve_selection(&SelectionRequest {
            tiers,
            providers,
            turn_deadline_ms: DEADLINE,
        })
    }

    /// The old-vs-new compatibility pin: with no provider configured anywhere, the resolver's
    /// legacy branch must agree byte-for-byte with the pre-existing `agent.backend`/`claude.model`
    /// behavior — same harness, no explicit provider, the configured model.
    #[test]
    fn legacy_claude_selection_is_unchanged() {
        let providers = registry(&[]);
        let got = resolve(
            SelectionTiers {
                global: field("claude", "", "claude-opus-5"),
                ..Default::default()
            },
            &providers,
        )
        .expect("legacy claude resolves");
        assert_eq!(got.harness, HarnessId::Claude);
        assert_eq!(got.harness_name, "claude");
        assert!(got.provider.is_none(), "no provider is invented");
        assert_eq!(got.provider_id, "");
        assert_eq!(got.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(got.origins.harness, Origin::Global);
        assert!(got.origins.provider.is_none());
        assert_eq!(got.origins.model, Some(Origin::Global));
    }

    #[test]
    fn legacy_opencode_native_login_is_unchanged() {
        let providers = registry(&[]);
        let got = resolve(
            SelectionTiers {
                global: field("opencode", "", "accounts/fireworks/models/x"),
                ..Default::default()
            },
            &providers,
        )
        .expect("legacy opencode resolves through its native login");
        assert_eq!(got.harness, HarnessId::Opencode);
        assert!(got.provider.is_none());
        assert_eq!(got.model.as_deref(), Some("accounts/fireworks/models/x"));
    }

    /// **The tuple-wise mutation guard.** One explicit field (the ticket's model) must NOT drag its
    /// siblings from the same tier: harness still comes from the profile and provider from the
    /// project. A resolver that picks a whole tier once would return the ticket's empty harness.
    #[test]
    fn resolution_is_field_wise_not_tuple_wise() {
        let providers = registry(&[def("fireworks")]);
        let got = resolve(
            SelectionTiers {
                ticket: field("", "", "model/from/ticket"),
                profile: field("opencode", "", ""),
                project: field("", "fireworks", ""),
                global: field("claude", "", "global-model"),
                ..Default::default()
            },
            &providers,
        )
        .expect("a partially-filled ticket inherits each empty field independently");
        assert_eq!(got.harness, HarnessId::Opencode);
        assert_eq!(got.origins.harness, Origin::Profile);
        assert_eq!(got.provider_id, "fireworks");
        assert_eq!(got.origins.provider, Some(Origin::Project));
        assert_eq!(got.model.as_deref(), Some("model/from/ticket"));
        assert_eq!(got.origins.model, Some(Origin::Ticket));
    }

    struct Case {
        name: &'static str,
        ticket: FieldSelection,
        review: Option<ReviewOverride>,
        profile: FieldSelection,
        identity: FieldSelection,
        project: FieldSelection,
        global: FieldSelection,
        harness: HarnessId,
        provider: &'static str,
        provider_origin: Option<Origin>,
        model: &'static str,
        model_origin: Option<Origin>,
        harness_origin: Origin,
    }

    impl Case {
        fn harness_name(&self) -> &'static str {
            match self.harness {
                HarnessId::Claude => "claude",
                HarnessId::Opencode => "opencode",
            }
        }
    }

    /// A table over every tier: the winning tier for each field and its origin, checked
    /// independently. Every row is a different point on the precedence chain.
    #[test]
    fn precedence_matrix_covers_every_tier() {
        let providers = registry(&[def("fireworks"), def("openrouter")]);
        let g = field("claude", "", ""); // a bare claude global default
        let cases = vec![
            Case {
                name: "ticket wins all three",
                ticket: field("opencode", "fireworks", "m-ticket"),
                review: None,
                profile: field("claude", "", "m-profile"),
                identity: FieldSelection::default(),
                project: FieldSelection::default(),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "fireworks",
                provider_origin: Some(Origin::Ticket),
                model: "m-ticket",
                model_origin: Some(Origin::Ticket),
                harness_origin: Origin::Ticket,
            },
            Case {
                name: "review override beats profile/identity/project/global",
                ticket: FieldSelection::default(),
                review: Some(ReviewOverride {
                    harness: "opencode".to_string(),
                    provider: "openrouter".to_string(),
                    model: "review-model".to_string(),
                }),
                profile: field("opencode", "fireworks", "profile-model"),
                identity: field("", "", "identity-model"),
                project: field("", "", "project-model"),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "openrouter",
                provider_origin: Some(Origin::Review),
                model: "review-model",
                model_origin: Some(Origin::Review),
                harness_origin: Origin::Profile,
            },
            Case {
                name: "ticket beats the review override for the fields it names",
                ticket: field("", "", "ticket-model"),
                review: Some(ReviewOverride {
                    harness: "opencode".to_string(),
                    provider: "fireworks".to_string(),
                    model: "review-model".to_string(),
                }),
                profile: field("opencode", "", ""),
                identity: FieldSelection::default(),
                project: FieldSelection::default(),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "fireworks",
                provider_origin: Some(Origin::Review),
                model: "ticket-model",
                model_origin: Some(Origin::Ticket),
                harness_origin: Origin::Profile,
            },
            Case {
                name: "profile beats identity/project/global",
                ticket: FieldSelection::default(),
                review: None,
                profile: field("opencode", "fireworks", "m-profile"),
                identity: field("claude", "openrouter", "m-identity"),
                project: FieldSelection::default(),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "fireworks",
                provider_origin: Some(Origin::Profile),
                model: "m-profile",
                model_origin: Some(Origin::Profile),
                harness_origin: Origin::Profile,
            },
            Case {
                name: "identity beats project/global",
                ticket: FieldSelection::default(),
                review: None,
                profile: FieldSelection::default(),
                identity: field("opencode", "openrouter", "m-identity"),
                project: field("claude", "fireworks", "m-project"),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "openrouter",
                provider_origin: Some(Origin::Identity),
                model: "m-identity",
                model_origin: Some(Origin::Identity),
                harness_origin: Origin::Identity,
            },
            Case {
                name: "project beats global",
                ticket: FieldSelection::default(),
                review: None,
                profile: FieldSelection::default(),
                identity: FieldSelection::default(),
                project: field("opencode", "fireworks", "m-project"),
                global: g.clone(),
                harness: HarnessId::Opencode,
                provider: "fireworks",
                provider_origin: Some(Origin::Project),
                model: "m-project",
                model_origin: Some(Origin::Project),
                harness_origin: Origin::Project,
            },
            Case {
                name: "global is the last resort",
                ticket: FieldSelection::default(),
                review: None,
                profile: FieldSelection::default(),
                identity: FieldSelection::default(),
                project: FieldSelection::default(),
                global: g.clone(),
                harness: HarnessId::Claude,
                provider: "",
                provider_origin: None,
                model: "",
                model_origin: None,
                harness_origin: Origin::Global,
            },
        ];
        for c in cases {
            let expected_harness_name = c.harness_name();
            let got = resolve(
                SelectionTiers {
                    ticket: c.ticket,
                    review: c.review,
                    profile: c.profile,
                    identity: c.identity,
                    project: c.project,
                    global: c.global,
                },
                &providers,
            )
            .unwrap_or_else(|e| panic!("{}: {e}", c.name));
            assert_eq!(got.harness, c.harness, "{}: harness", c.name);
            assert_eq!(
                got.harness_name, expected_harness_name,
                "{}: harness name",
                c.name
            );
            assert_eq!(got.provider_id, c.provider, "{}: provider", c.name);
            assert_eq!(
                got.origins.provider, c.provider_origin,
                "{}: provider origin",
                c.name
            );
            assert_eq!(
                got.model.as_deref().unwrap_or(""),
                c.model,
                "{}: model",
                c.name
            );
            assert_eq!(
                got.origins.model, c.model_origin,
                "{}: model origin",
                c.name
            );
            assert_eq!(
                got.origins.harness, c.harness_origin,
                "{}: harness origin",
                c.name
            );
        }
    }

    /// A review override written for a harness the review did not resolve to must refuse, never be
    /// applied to the wrong run and never silently degrade to the reviewer's own model.
    #[test]
    fn review_override_for_a_different_harness_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                profile: field("opencode", "", "profile-model"),
                review: Some(ReviewOverride {
                    harness: "claude".to_string(),
                    provider: String::new(),
                    model: "claude-opus-5".to_string(),
                }),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("a review model configured for claude must not apply to an opencode reviewer");
        assert_eq!(
            err,
            SelectionRefusal::ReviewHarnessMismatch {
                configured: "claude".to_string(),
                selected: "opencode".to_string(),
            }
        );
    }

    /// An empty review block (neither field set) never refuses, even if it names another harness.
    #[test]
    fn empty_review_override_is_inert() {
        let providers = registry(&[]);
        let got = resolve(
            SelectionTiers {
                profile: field("opencode", "", "m"),
                review: Some(ReviewOverride {
                    harness: "claude".to_string(),
                    provider: String::new(),
                    model: String::new(),
                }),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect("an empty review block changes nothing");
        assert_eq!(got.model.as_deref(), Some("m"));
        assert_eq!(got.origins.model, Some(Origin::Profile));
    }

    /// **The review-stability guard.** Once resolved, the review selection does not depend on the
    /// routed teammate's own model: changing the teammate tier cannot move a resolved review field.
    /// This is what "computed once and reused by later dispatch" protects.
    #[test]
    fn review_selection_does_not_move_with_the_routed_teammate() {
        let providers = registry(&[def("fireworks")]);
        let with_cheap_teammate = resolve(
            SelectionTiers {
                profile: field("opencode", "fireworks", "cheap-profile-model"),
                review: Some(ReviewOverride {
                    harness: "opencode".to_string(),
                    provider: "fireworks".to_string(),
                    model: "premium-review-model".to_string(),
                }),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect("resolves");
        let with_expensive_teammate = resolve(
            SelectionTiers {
                profile: field("opencode", "fireworks", "expensive-profile-model"),
                review: Some(ReviewOverride {
                    harness: "opencode".to_string(),
                    provider: "fireworks".to_string(),
                    model: "premium-review-model".to_string(),
                }),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect("resolves");
        assert_eq!(with_cheap_teammate.model, with_expensive_teammate.model);
        assert_eq!(
            with_cheap_teammate.model.as_deref(),
            Some("premium-review-model")
        );
        assert_eq!(with_cheap_teammate.origins.model, Some(Origin::Review));
        assert_eq!(with_cheap_teammate, with_expensive_teammate);
    }

    /// **The fallback mutation guard.** An unimplemented harness refuses; if the resolver fell back
    /// to `agent.backend` this would return Claude instead of an error.
    #[test]
    fn unimplemented_harness_refuses_instead_of_falling_back() {
        let providers = registry(&[]);
        for name in ["codex", "goose", "openai", "CLAUDE"] {
            let err = resolve(
                SelectionTiers {
                    ticket: field(name, "", ""),
                    global: field("claude", "", ""),
                    ..Default::default()
                },
                &providers,
            )
            .expect_err(name);
            assert_eq!(
                err,
                SelectionRefusal::HarnessNotImplemented {
                    name: name.to_string()
                },
                "{name}"
            );
        }
    }

    /// The A2/H5 registry is the compatibility switch: Claude's adapter consumes no provider
    /// protocol in v1, so an explicit provider on Claude refuses rather than opening a direct-key
    /// path.
    #[test]
    fn explicit_provider_on_claude_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                ticket: field("claude", "fireworks", "some-model"),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("claude has no provider adapter in v1");
        assert_eq!(
            err,
            SelectionRefusal::UnsupportedProtocol {
                harness: "claude".to_string(),
                protocol: "openai-compatible".to_string(),
            }
        );
    }

    #[test]
    fn explicit_provider_requires_an_exact_model() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                ticket: field("opencode", "fireworks", ""),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("no model");
        assert_eq!(err, SelectionRefusal::MissingModel);
    }

    #[test]
    fn unknown_provider_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                ticket: field("opencode", "openrouter", "m"),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("openrouter is not configured");
        assert_eq!(
            err,
            SelectionRefusal::UnknownProvider {
                id: "openrouter".to_string()
            }
        );
    }

    #[test]
    fn non_canonical_provider_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                ticket: field("opencode", "Fireworks", "m"),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("uppercase is not canonical");
        assert!(
            matches!(err, SelectionRefusal::InvalidProvider { .. }),
            "{err}"
        );
    }

    #[test]
    fn invalid_model_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err = resolve(
            SelectionTiers {
                ticket: field("opencode", "fireworks", " padded "),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect_err("surrounding whitespace is a transport violation");
        assert!(
            matches!(err, SelectionRefusal::InvalidModel { .. }),
            "{err}"
        );
    }

    #[test]
    fn missing_harness_refuses() {
        let providers = registry(&[]);
        let err = resolve(SelectionTiers::default(), &providers).expect_err("no harness anywhere");
        assert_eq!(err, SelectionRefusal::MissingHarness);
    }

    /// The explicit-provider happy path: the plan is built from the registry entry, carries the
    /// canonical binding identity (never a value), and lowers the limits against the deadline.
    #[test]
    fn explicit_provider_builds_a_pure_plan_with_origins() {
        let mut d = def("fireworks");
        d.allow_insecure_http = false;
        d.broker_limits.capability_lifetime_ms = None;
        let providers = registry(&[d.clone()]);
        let got = resolve(
            SelectionTiers {
                ticket: field("opencode", "fireworks", "accounts/fireworks/models/x"),
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
        )
        .expect("opencode + openai-compatible + model resolves");
        assert_eq!(got.provider_id, "fireworks");
        assert_eq!(got.origins.provider, Some(Origin::Ticket));
        let plan = got.provider.expect("a plan");
        assert_eq!(plan.stable_id, "fireworks");
        assert_eq!(plan.protocol, ProviderProtocol::OpenAiCompatible);
        assert_eq!(
            plan.normalized_endpoint,
            d.normalized_base_url().expect("normalized")
        );
        assert_eq!(
            plan.credential_binding,
            d.credential_binding().expect("binding").identity()
        );
        assert_eq!(
            plan.credential_ref,
            rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN
        );
        // The lowered lifetime is the derived default (min(1h, deadline)) — never an Option.
        assert_eq!(
            plan.limits.capability_lifetime_ms,
            BrokerLimits::default().effective_capability_lifetime_ms(DEADLINE)
        );
        assert_eq!(plan.model, "accounts/fireworks/models/x");
        assert_eq!(plan.origins.provider, "ticket");
        assert_eq!(plan.origins.model, "ticket");
    }

    /// The manager's Claude default is independent of every teammate: the function takes no
    /// teammate input at all, and an empty tuple resolves to the native-login Claude manager with
    /// the CLI default model (NOT `claude.model`, NOT a teammate's model).
    #[test]
    fn manager_defaults_to_independent_claude() {
        let providers = registry(&[def("fireworks")]);
        let got = resolve_manager_selection(&FieldSelection::default(), &providers, DEADLINE)
            .expect("the default manager resolves");
        assert_eq!(got.harness, HarnessId::Claude);
        assert_eq!(got.harness_name, "claude");
        assert_eq!(got.origins.harness, Origin::Default);
        assert!(got.provider.is_none());
        assert!(
            got.model.is_none(),
            "the CLI default is preserved, not inherited"
        );
        assert!(got.origins.model.is_none());
    }

    #[test]
    fn manager_non_claude_requires_provider_and_model() {
        let providers = registry(&[def("fireworks")]);
        assert_eq!(
            resolve_manager_selection(&field("opencode", "", ""), &providers, DEADLINE)
                .expect_err("no provider"),
            SelectionRefusal::ManagerProviderRequired {
                harness: "opencode".to_string()
            }
        );
        assert_eq!(
            resolve_manager_selection(&field("opencode", "fireworks", ""), &providers, DEADLINE)
                .expect_err("no model"),
            SelectionRefusal::ManagerModelRequired {
                harness: "opencode".to_string()
            }
        );
    }

    #[test]
    fn manager_explicit_opencode_builds_its_own_plan() {
        let providers = registry(&[def("fireworks")]);
        let got =
            resolve_manager_selection(&field("opencode", "fireworks", "m"), &providers, DEADLINE)
                .expect("an explicit manager tuple resolves");
        assert_eq!(got.harness, HarnessId::Opencode);
        assert_eq!(got.origins.harness, Origin::Manager);
        assert_eq!(got.provider_id, "fireworks");
        assert_eq!(got.origins.provider, Some(Origin::Manager));
        assert_eq!(got.model.as_deref(), Some("m"));
        assert!(got.provider.is_some());
    }

    #[test]
    fn manager_claude_with_a_provider_refuses() {
        let providers = registry(&[def("fireworks")]);
        let err =
            resolve_manager_selection(&field("claude", "fireworks", "m"), &providers, DEADLINE)
                .expect_err("claude has no provider adapter");
        assert!(
            matches!(err, SelectionRefusal::UnsupportedProtocol { .. }),
            "{err}"
        );
    }

    #[test]
    fn manager_unimplemented_harness_refuses() {
        let providers = registry(&[]);
        let err = resolve_manager_selection(&field("codex", "", ""), &providers, DEADLINE)
            .expect_err("codex is not implemented");
        assert_eq!(
            err,
            SelectionRefusal::HarnessNotImplemented {
                name: "codex".to_string()
            }
        );
    }

    /// Resolution is deterministic and owned: the same request twice yields byte-identical results,
    /// with no hidden state between calls.
    #[test]
    fn resolution_is_deterministic_and_pure() {
        let providers = registry(&[def("fireworks")]);
        let tiers = SelectionTiers {
            ticket: field("opencode", "fireworks", "m"),
            global: field("claude", "", ""),
            ..Default::default()
        };
        let a = resolve(tiers.clone(), &providers).expect("a");
        let b = resolve(tiers, &providers).expect("b");
        assert_eq!(a, b);
    }

    /// `from_agent` reads the three normalized global fields — the global tier's source.
    #[test]
    fn from_agent_reads_the_global_tier() {
        let agent = rhapsody_config::Agent {
            backend: "opencode".to_string(),
            provider: "fireworks".to_string(),
            model: "m".to_string(),
            max_concurrent_agents: 0,
            max_concurrent_reviews: None,
            max_run_tokens: 0,
            max_turns: 0,
            max_retry_backoff_ms: 0,
            max_concurrent_agents_by_state: Default::default(),
            handoff_drain_grace_ms: 0,
        };
        assert_eq!(
            FieldSelection::from_agent(&agent),
            field("opencode", "fireworks", "m")
        );
    }

    /// The label-first entry point resolves the three reserved namespaces end to end.
    #[test]
    fn ticket_labels_resolve_through_the_parser() {
        let providers = registry(&[def("fireworks")]);
        let got = resolve_ticket_labels(
            &labels(&[
                "rhapsody:harness/opencode",
                "rhapsody:provider/fireworks",
                "rhapsody:model/accounts/fireworks/models/x",
            ]),
            SelectionTiers {
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
            DEADLINE,
        )
        .expect("valid labels resolve");
        assert_eq!(got.harness, HarnessId::Opencode);
        assert_eq!(got.provider_id, "fireworks");
        assert_eq!(got.model.as_deref(), Some("accounts/fireworks/models/x"));
        assert_eq!(got.origins.harness, Origin::Ticket);
        assert_eq!(got.origins.provider, Some(Origin::Ticket));
        assert_eq!(got.origins.model, Some(Origin::Ticket));
    }

    /// A conflicting or malformed label set is refused by the resolver itself, before any lower
    /// tier is consulted — the ticket-label tier earns the same first-class refusal as everything
    /// else, never a partial resolution.
    #[test]
    fn conflicting_ticket_labels_refuse_before_resolution() {
        let providers = registry(&[def("fireworks"), def("openrouter")]);
        let err = resolve_ticket_labels(
            &labels(&[
                "rhapsody:provider/fireworks",
                "rhapsody:provider/openrouter",
            ]),
            SelectionTiers {
                global: field("claude", "", ""),
                ..Default::default()
            },
            &providers,
            DEADLINE,
        )
        .expect_err("two distinct provider labels must refuse");
        assert!(
            matches!(
                err,
                SelectionRefusal::RoutingLabel(RoutingLabelError::Conflict { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().starts_with("selection_refusal:"));
    }

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    /// **The purity guard.** `selection.rs` must never grow an I/O path — no filesystem, process,
    /// network, or Keychain call. The needles are built with `concat!` so this test's own source
    /// cannot satisfy the scan it performs.
    #[test]
    fn selection_module_has_no_io_tokens() {
        const FORBIDDEN: &[&str] = &[
            concat!("std::", "fs"),
            concat!("std::", "process"),
            concat!("std::", "net"),
            concat!("req", "west"),
            concat!("key", "ring"),
            concat!("tokio::", "process"),
            concat!("Command", "::new"),
        ];
        let src = include_str!("selection.rs");
        for needle in FORBIDDEN {
            assert!(
                !src.contains(needle),
                "selection.rs must stay a pure resolver; found an I/O token"
            );
        }
    }
}
