//! providerprep — the production prepared-dispatch resolver (PB7, STUDIO-1002; design record
//! `~/.rhapsody/docs/provider-broker-design.md` §10.1). Rhapsody-only; no Go counterpart.
//!
//! The control loop resolves only the pure harness/provider/model selection and inserts a
//! reservation; this module is the off-loop half that turns that pure selection into one dispatch's
//! move-only broker custody. Concretely, for a [`PreparationRequest`] it:
//!
//! 1. runs the pure [`crate::selection::resolve_ticket_labels`] on the tiers the loop handed over
//!    (no I/O, no credential) and derives the canonical [`ResolvedProviderPlan`];
//! 2. on the legacy/native-login branch (no explicit provider) returns `Ready` with **no** custody,
//!    so the dispatch stays byte-identical to a daemon built before the feature;
//! 3. for an explicit provider, delegates to a [`PreparedProviderSource`] — implemented at the
//!    composition root, where the credential owner adapter and the broker registrar live — which
//!    reads the bound credential through the P0c/P1 abstraction and registers the plan with the
//!    broker, returning the opaque [`PreparedProvider`] and the credential revision it observed.
//!
//! Every non-`Present` credential state and every broker/owner failure becomes a typed
//! [`RefusalReason`]; no reusable key, binding, or owner revision ever leaves the source.

use std::sync::Arc;

use async_trait::async_trait;
use rhapsody_agent::{PreparedProvider, ResolvedProviderPlan};
use tokio::sync::OwnedSemaphorePermit;

use crate::prepare::{
    PreparationCompletion, PreparationOutcome, PreparationRequest, PreparationResolver,
    PreparedDispatch, PreparedSelection, RefusalReason,
};

/// The off-loop operation that opens one dispatch's broker custody from a pure plan: read the bound
/// credential and register it with the broker. Implemented at the composition root (`rhapsodyd`),
/// where the credential owner adapter and the broker registrar live; the orchestrator only
/// orchestrates and never learns a secret.
#[async_trait]
pub trait PreparedProviderSource: Send + Sync {
    /// Read the bound credential for `plan` and, when it is `Present` under the plan's exact
    /// binding, register it with the broker. Returns the move-only custody plus the opaque owner
    /// revision observed. Every non-`Present` state and every broker/owner failure is a typed
    /// [`RefusalReason`]; a claimed-but-unusable credential is never downgraded to another mode.
    async fn open_provider(
        &self,
        plan: &ResolvedProviderPlan,
    ) -> Result<OpenedProvider, ProviderRefusal>;
}

/// A successfully opened dispatch: the opaque broker custody and the non-secret credential revision
/// the read observed. The revision is an opaque counter, never a credential.
#[derive(Debug)]
pub struct OpenedProvider {
    pub provider: PreparedProvider,
    pub revision: String,
}

/// A typed refusal from opening a provider, carrying the opaque owner revision observed so the
/// refusal gate re-arms on a changed credential revision (design §12) rather than treating every
/// refusal as identical. The revision is non-secret and empty only when a read never reached the
/// owner.
#[derive(Debug)]
pub struct ProviderRefusal {
    pub reason: RefusalReason,
    pub revision: String,
}

/// The production [`PreparationResolver`]: pure selection on the request's tiers, then one call to
/// the injected [`PreparedProviderSource`]. The keychain/IPC read and the broker registration both
/// happen inside that source, off the control task.
pub struct ProviderPreparationResolver {
    source: Arc<dyn PreparedProviderSource>,
}

impl ProviderPreparationResolver {
    pub fn new(source: Arc<dyn PreparedProviderSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl PreparationResolver for ProviderPreparationResolver {
    async fn prepare(
        &self,
        req: &PreparationRequest,
        permit: OwnedSemaphorePermit,
    ) -> PreparationCompletion {
        // The permit is released by this drop; the work below performs no `spawn_blocking` that
        // outlives the future, so holding it past the return is unnecessary.
        drop(permit);
        let selection = match crate::selection::resolve_ticket_labels(
            &req.labels,
            req.tiers.clone(),
            &req.providers,
            req.turn_deadline_ms,
        ) {
            Ok(selection) => selection,
            Err(e) => {
                return preparation_refusal(
                    RefusalReason::SelectionRefused(e.to_string()),
                    PreparedSelection::default(),
                );
            }
        };
        let resolved = PreparedSelection {
            harness: selection.harness_name.clone(),
            model: selection.model.clone().unwrap_or_default(),
            provider: selection.provider_id.clone(),
        };
        let Some(plan) = selection.provider else {
            // The legacy/native-login branch: no explicit provider was selected, so there is no
            // credential to read and no broker session to open. `Ready` with no custody keeps the
            // dispatch on the ordinary shared runner, byte-identical to before the feature.
            return PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    selection.harness_name,
                    selection.model.unwrap_or_default(),
                    selection.provider_id,
                    String::new(),
                )),
                observed_revision: String::new(),
                resolved,
            };
        };
        let harness = selection.harness_name;
        let provider_id = plan.stable_id.clone();
        match self.source.open_provider(&plan).await {
            Ok(opened) => {
                let dispatch = PreparedDispatch::new(
                    harness,
                    plan.model.clone(),
                    provider_id,
                    opened.revision.clone(),
                )
                .with_provider(opened.provider, plan);
                PreparationCompletion {
                    outcome: PreparationOutcome::Ready(dispatch),
                    observed_revision: opened.revision,
                    resolved,
                }
            }
            Err(ProviderRefusal { reason, revision }) => PreparationCompletion {
                outcome: PreparationOutcome::Refused(reason),
                observed_revision: revision,
                resolved,
            },
        }
    }
}

/// A typed zero-turn refusal completion with an empty observed revision (the selection refused
/// before any credential was reached) and an empty resolved selection.
fn preparation_refusal(
    reason: RefusalReason,
    resolved: PreparedSelection,
) -> PreparationCompletion {
    PreparationCompletion {
        outcome: PreparationOutcome::Refused(reason),
        observed_revision: String::new(),
        resolved,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rhapsody_agent::{ProviderLimits, ProviderOrigins, ProviderProtocol};
    use rhapsody_provider_broker::{
        BoundCredentialLease, Broker, OsRandom, SessionPolicy, SystemClock,
    };
    use tokio::sync::{OwnedSemaphorePermit, Semaphore};

    use super::*;
    use crate::prepare::{PreparationKey, PreparationOutcome, RefusalReason};

    fn plan() -> ResolvedProviderPlan {
        ResolvedProviderPlan {
            stable_id: "fireworks".to_string(),
            protocol: ProviderProtocol::OpenAiCompatible,
            normalized_endpoint: "https://api.fireworks.ai/inference/v1".to_string(),
            allow_insecure_http: false,
            credential_binding:
                "fireworks|openai-chat-completions-bearer-v1|https://api.fireworks.ai/inference/v1"
                    .to_string(),
            credential_ref: "keychain".to_string(),
            limits: ProviderLimits::default(),
            model: "accounts/fireworks/models/x".to_string(),
            origins: ProviderOrigins {
                provider: "ticket".to_string(),
                model: "ticket".to_string(),
            },
        }
    }

    /// A real, registered broker custody for the plan above — the move-only payload a successful
    /// source returns.
    fn opened(broker: &Broker, revision: &str) -> OpenedProvider {
        let registration_plan = rhapsody_agent::lower_provider_plan(&plan()).expect("lowered");
        let binding = registration_plan.binding().expect("binding");
        let lease =
            BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease");
        let policy = SessionPolicy::new(*registration_plan.limits()).expect("policy");
        let registration = broker
            .registrar()
            .register_session(registration_plan, lease, policy)
            .expect("register");
        OpenedProvider {
            provider: PreparedProvider::from_registration(
                "fireworks".to_string(),
                ProviderProtocol::OpenAiCompatible,
                registration,
            ),
            revision: revision.to_string(),
        }
    }

    /// A source that answers once with a scripted result, so a single resolver call is enough.
    struct ScriptedSource {
        answer: Mutex<Option<Result<OpenedProvider, ProviderRefusal>>>,
    }

    #[async_trait]
    impl PreparedProviderSource for ScriptedSource {
        async fn open_provider(
            &self,
            _plan: &ResolvedProviderPlan,
        ) -> Result<OpenedProvider, ProviderRefusal> {
            self.answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .expect("the source is asked at most once")
        }
    }

    fn request(
        labels: &[&str],
        providers: std::collections::BTreeMap<String, rhapsody_config::ProviderDefinition>,
    ) -> PreparationRequest {
        PreparationRequest {
            key: PreparationKey::Ticket {
                issue_id: "1".to_string(),
            },
            selection: "ticket|1".to_string(),
            config_generation: 0,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            tiers: crate::selection::SelectionTiers {
                global: crate::selection::FieldSelection {
                    harness: "opencode".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            providers,
            turn_deadline_ms: 3_600_000,
        }
    }

    fn permit() -> OwnedSemaphorePermit {
        std::sync::Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("permit")
    }

    async fn run(
        source: ScriptedSource,
        labels: &[&str],
        providers: std::collections::BTreeMap<String, rhapsody_config::ProviderDefinition>,
    ) -> PreparationCompletion {
        ProviderPreparationResolver::new(std::sync::Arc::new(source))
            .prepare(&request(labels, providers), permit())
            .await
    }

    fn fireworks() -> rhapsody_config::ProviderDefinition {
        rhapsody_config::ProviderDefinition {
            id: "fireworks".to_string(),
            protocol: "openai-compatible".to_string(),
            display_name: String::new(),
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            allow_insecure_http: false,
            credential: rhapsody_config::CredentialSource {
                source: rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: rhapsody_config::BrokerLimits::default(),
        }
    }

    fn registry(
        ids: &[&str],
    ) -> std::collections::BTreeMap<String, rhapsody_config::ProviderDefinition> {
        let mut out = std::collections::BTreeMap::new();
        for id in ids {
            let mut d = fireworks();
            d.id = (*id).to_string();
            out.insert((*id).to_string(), d);
        }
        out
    }

    /// No provider selected ⇒ `Ready` with NO custody. This is the legacy/native-login branch the
    /// compatibility guarantee rests on; a mutation that invented a provider (or a custody) here
    /// would make every no-provider dispatch brokered and red this test.
    #[tokio::test]
    async fn no_provider_resolves_ready_without_custody() {
        let source = ScriptedSource {
            answer: Mutex::new(None),
        };
        let completion = run(source, &[], registry(&[])).await;
        let PreparationOutcome::Ready(dispatch) = completion.outcome else {
            panic!("expected Ready, got {:?}", completion.outcome);
        };
        assert!(
            !dispatch.has_custody(),
            "no provider means no broker custody"
        );
        assert!(completion.observed_revision.is_empty());
    }

    /// An explicit provider that opens successfully crosses ONLY an opaque custody handle: the
    /// completion is `Ready` with custody and the source's revision, and the plan is carried for the
    /// dispatch-time factory. The mutation guard is dropping the `.with_provider(..)` call.
    #[tokio::test]
    async fn explicit_provider_crosses_move_only_custody() {
        let broker = Broker::new(
            "http://127.0.0.1:0/v1",
            std::sync::Arc::new(SystemClock::new()),
            std::sync::Arc::new(OsRandom::new()),
        )
        .expect("broker");
        let source = ScriptedSource {
            answer: Mutex::new(Some(Ok(opened(&broker, "rev-7")))),
        };
        let completion = run(
            source,
            &["rhapsody:provider/fireworks", "rhapsody:model/m"],
            registry(&["fireworks"]),
        )
        .await;
        let PreparationOutcome::Ready(dispatch) = completion.outcome else {
            panic!("expected Ready, got {:?}", completion.outcome);
        };
        assert!(
            dispatch.has_custody(),
            "an explicit provider carries custody"
        );
        assert_eq!(dispatch.credential_revision, "rev-7");
        assert_eq!(completion.observed_revision, "rev-7");
        assert_eq!(completion.resolved.provider, "fireworks");
    }

    /// Every typed refusal the source can return passes through unchanged, so the loop's zero-turn
    /// refusal path sees the right reason and the gate can re-arm on the revision.
    #[tokio::test]
    async fn typed_refusals_pass_through_with_revision() {
        for (reason, code) in [
            (RefusalReason::CredentialAbsent, "credential_absent"),
            (
                RefusalReason::CredentialDeniedOrLocked,
                "credential_denied_or_locked",
            ),
            (RefusalReason::CredentialMalformed, "credential_malformed"),
            (RefusalReason::BindingMismatch, "binding_mismatch"),
            (RefusalReason::OwnerUnavailable, "owner_unavailable"),
            (RefusalReason::OwnerUnauthorized, "owner_unauthorized"),
            (
                RefusalReason::ProviderBrokerUnavailable,
                "provider_broker_unavailable",
            ),
        ] {
            let source = ScriptedSource {
                answer: Mutex::new(Some(Err(ProviderRefusal {
                    reason: reason.clone(),
                    revision: "rev-9".to_string(),
                }))),
            };
            let completion = run(
                source,
                &["rhapsody:provider/fireworks", "rhapsody:model/m"],
                registry(&["fireworks"]),
            )
            .await;
            let PreparationOutcome::Refused(got) = completion.outcome else {
                panic!("expected a refusal");
            };
            assert_eq!(got.code(), code);
            assert_eq!(
                completion.observed_revision, "rev-9",
                "the refusal carries the observed revision so the gate re-arms on a change"
            );
        }
    }

    /// A selection refusal (an unsupported harness/provider combination) is a typed zero-turn
    /// refusal and never reaches the credential source — the mutation guard is calling the source
    /// before the pure selection.
    #[tokio::test]
    async fn selection_refusal_becomes_a_typed_refusal() {
        let source = ScriptedSource {
            answer: Mutex::new(None),
        };
        // The ticket names a provider the project does not configure, so selection refuses.
        let completion = run(
            source,
            &["rhapsody:provider/openrouter", "rhapsody:model/m"],
            registry(&["fireworks"]),
        )
        .await;
        let PreparationOutcome::Refused(reason) = completion.outcome else {
            panic!("expected a refusal");
        };
        assert_eq!(reason.code(), "selection_refused");
        assert!(completion.observed_revision.is_empty());
    }
}
