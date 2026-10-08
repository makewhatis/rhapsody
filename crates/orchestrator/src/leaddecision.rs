//! Tech-lead decision contract (STUDIO-1136). No Go counterpart.

pub use rhapsody_store::LeadDecisionRow;
use serde::{Deserialize, Serialize};

pub const LEAD_DECISION_TAG: &str = "rhapsody-lead-decision";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum LeadAction {
    Resolve {
        reason: String,
    },
    RouteBack {
        ticket: String,
        answer: String,
    },
    Requeue {
        ticket: String,
    },
    ClearReview {
        pr: String,
    },
    Reassign {
        ticket: String,
        identity: String,
    },
    Commission {
        kind: String,
        question: String,
        hypothesis: String,
    },
    AuthorizeCredential {
        ticket: String,
        rule: String,
    },
    Escalate {
        need: String,
    },
}

pub struct GuardCtx {
    pub ticket: String,
    pub pr: Option<String>,
    pub route_backs: i64,
    pub stale: bool,
    pub identities: Vec<String>,
}

pub fn parse_lead_decision(text: &str) -> Result<Vec<LeadAction>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Block {
        actions: Vec<LeadAction>,
    }
    let blocks = crate::reviewfindings::fenced_blocks(text, LEAD_DECISION_TAG);
    if blocks.len() != 1 {
        return Err("expected exactly one lead decision block".into());
    }
    if blocks[0].len() > 32_000 {
        return Err("lead block exceeds 32000 bytes".into());
    }
    // Do not publish serde errors: they may echo an unknown credential-shaped value.
    let block: Block = serde_json::from_str(&blocks[0])
        .map_err(|_| "invalid lead action or fields".to_string())?;
    if block.actions.is_empty() || block.actions.len() > 8 {
        return Err("expected 1 to 8 lead actions".into());
    }
    for action in &block.actions {
        for text in action.strings() {
            if text.trim().is_empty() || text.chars().count() > 4000 {
                return Err("lead action has empty or overlong text".into());
            }
            if crate::managerdecision::contains_secret_shape(text) {
                return Err("lead action contains secret-shaped text".into());
            }
        }
    }
    Ok(block.actions)
}

impl LeadAction {
    fn strings(&self) -> Vec<&str> {
        match self {
            Self::Resolve { reason } => vec![reason],
            Self::RouteBack { ticket, answer } => vec![ticket, answer],
            Self::Requeue { ticket } => vec![ticket],
            Self::ClearReview { pr } => vec![pr],
            Self::Reassign { ticket, identity } => vec![ticket, identity],
            Self::Commission {
                kind,
                question,
                hypothesis,
            } => vec![kind, question, hypothesis],
            Self::AuthorizeCredential { ticket, rule } => vec![ticket, rule],
            Self::Escalate { need } => vec![need],
        }
    }
}

/// Grants the named policy as instructions only. The lead never reads, copies or writes a key.
pub fn credential_rule(rule: &str) -> Option<&'static str> {
    match rule {
        "openai-refresh-blank-copy" => Some(
            "Standing rule openai-refresh-blank-copy: a read-only, access-only copy of the operator's OpenAI login is permitted with refresh: \"\" (field present, value empty). Never refresh, never copy back, never log contents; only hashes and expiry may be reported.",
        ),
        "fireworks-key-measurement" => Some(
            "Standing rule fireworks-key-measurement: use the operator-provided Fireworks key only for the scoped cost measurement, within configured USD caps. Never mint a key, enable credits, persist or log the key; report measurement results only.",
        ),
        _ => None,
    }
}

pub fn guard(a: &LeadAction, ctx: &GuardCtx) -> Result<(), String> {
    if ctx.stale {
        return Err("stale subject; re-queue with fresh evidence".into());
    }
    let ticket = match a {
        LeadAction::RouteBack { ticket, .. }
        | LeadAction::Requeue { ticket }
        | LeadAction::Reassign { ticket, .. }
        | LeadAction::AuthorizeCredential { ticket, .. } => Some(ticket),
        _ => None,
    };
    if ticket.is_some_and(|t| !t.eq_ignore_ascii_case(&ctx.ticket) || ctx.ticket.is_empty()) {
        return Err("lead action targets another ticket".into());
    }
    match a {
        LeadAction::RouteBack { .. } if ctx.route_backs > 0 => {
            return Err("second route_back refused; commission or escalate".into());
        }
        LeadAction::ClearReview { pr }
            if !ctx.pr.as_ref().is_some_and(|p| p.eq_ignore_ascii_case(pr)) =>
        {
            return Err("lead action targets another PR".into());
        }
        LeadAction::Reassign { identity, .. } if !ctx.identities.contains(identity) => {
            return Err("reassign identity is not in this installation's roster".into());
        }
        LeadAction::Commission { kind, .. }
            if !matches!(kind.as_str(), "author" | "ticket") || ctx.ticket.is_empty() =>
        {
            return Err("commission requires author|ticket and a resolved origin ticket".into());
        }
        LeadAction::AuthorizeCredential { rule, .. } if credential_rule(rule).is_none() => {
            return Err("credential rule is not allow-listed".into());
        }
        _ => {}
    }
    Ok(())
}
