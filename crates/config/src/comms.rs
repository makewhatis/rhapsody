//! comms — the recognized communication backends and the identity each can express (STUDIO-1503).
//!
//! **Communication is bidirectional; notification is one-way.** This is the communication axis:
//! a backend polls a surface for an inbound message, the daemon folds that message onto the
//! candidate that carries it, and a run is re-engaged from it. [`crate::notify`] is the separate,
//! deliberately unentangled one-way axis; nothing here sends.
//!
//! This is the `harness:` / `memory.backend` / `notify:` precedent for the inbound surface: exactly
//! one place spells out the names the daemon RECOGNIZES, kept apart from the names this build
//! IMPLEMENTS. A recognized-but-unimplemented name (today `buzz` and `slack`) degrades with a
//! warning rather than failing a dispatch, exactly as `codex` does on the harness axis. The names
//! live here; the concrete transports live where the config is consumed
//! (`rhapsody_orchestrator::comms`), so this crate keeps no dependency on a transport.
//!
//! **The variation point between backends is how an agent's identity is represented** (the ticket's
//! whole point), and it is declared here, per name, rather than assumed by a caller. GitHub and
//! Linear both post through ONE shared account, so a name they report is a CLAIM and never a
//! verified identity; Buzz (5/5) holds a keypair per agent, so its attribution is proven at the
//! transport; Slack, if ever, is a bot token with per-agent display names at best. A caller must not
//! assume more than [`IdentityRepresentation::is_verified`] admits, and Rhapsody's own identity
//! model stays authoritative regardless — a backend's weaker attribution never becomes the source
//! of truth for who did what.

/// The `comms.backends` values this daemon RECOGNIZES. `none`, `github` and `linear` are
/// implemented; `buzz` and `slack` are named because they are squarely in the bidirectional
/// category the design describes, so a config that names one degrades loudly rather than looking
/// like an unknown typo. Adding a name here makes config accept it; it does NOT give the
/// composition root a transport for it — that stays a separate, per-build decision.
pub const COMMS_BACKEND_NAMES: &[&str] = &["none", "github", "linear", "buzz", "slack"];

/// The `comms.backends` names this build can actually run. A subset of [`COMMS_BACKEND_NAMES`],
/// pinned by a test so the two cannot silently drift.
pub const IMPLEMENTED_COMMS_BACKENDS: &[&str] = &["none", "github", "linear"];

/// Whether `name` is a recognized backend. `""` is not recognized on purpose: an empty entry is
/// dropped before resolution ever asks, exactly as an empty `notify.backend` means "inherit".
pub fn comms_backend_is_recognized(name: &str) -> bool {
    COMMS_BACKEND_NAMES.contains(&name)
}

/// Whether this build can run `name`.
pub fn comms_backend_is_implemented(name: &str) -> bool {
    IMPLEMENTED_COMMS_BACKENDS.contains(&name)
}

/// How a communication backend can represent WHO sent an inbound message — the STUDIO-1503
/// variation point between backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityRepresentation {
    /// One shared account posts for every actor. A name may appear in the message body, but nothing
    /// at the transport binds it: a name this backend reports is a CLAIM, never a verified
    /// identity. GitHub and Linear both work this way today.
    Textual,
    /// Each actor holds its own keypair and the transport proves the sender. Buzz (5/5).
    Cryptographic,
}

impl IdentityRepresentation {
    /// The stable token the boot log and any introspection render. Stable because an operator greps
    /// it.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityRepresentation::Textual => "textual",
            IdentityRepresentation::Cryptographic => "cryptographic",
        }
    }

    /// Whether a name this backend reports is PROVEN at the transport rather than merely claimed.
    ///
    /// This is the guard the ticket asks for: a caller must never treat a `Textual` name as the
    /// source of truth for who acted, because on a `Textual` backend any local process — or any
    /// account holder — can write any name into a body.
    pub fn is_verified(self) -> bool {
        matches!(self, IdentityRepresentation::Cryptographic)
    }
}

/// What a recognized backend can express about an inbound message's sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommsIdentity {
    /// How the backend represents identity.
    pub representation: IdentityRepresentation,
    /// Whether the backend can NAME the sender at all. `false` ⇒ the seam records no author and
    /// must never fabricate one from the body (GitHub: one shared account, no author field).
    pub surfaces_author: bool,
}

/// The identity a recognized backend can express, or `None` for `none` and any unrecognized name.
pub fn comms_backend_identity(name: &str) -> Option<CommsIdentity> {
    match name {
        // One shared account posts every comment; there is no author field to read.
        "github" => Some(CommsIdentity {
            representation: IdentityRepresentation::Textual,
            surfaces_author: false,
        }),
        // A Linear comment carries its author's display name — but through the same shared token, so
        // the name is still a claim, not a proof.
        "linear" => Some(CommsIdentity {
            representation: IdentityRepresentation::Textual,
            surfaces_author: true,
        }),
        // 5/5: a keypair per agent; the transport proves the sender.
        "buzz" => Some(CommsIdentity {
            representation: IdentityRepresentation::Cryptographic,
            surfaces_author: true,
        }),
        // A bot token with per-agent display names at best.
        "slack" => Some(CommsIdentity {
            representation: IdentityRepresentation::Textual,
            surfaces_author: true,
        }),
        // `none` sends nothing and has no sender to represent; anything else is unrecognized.
        _ => None,
    }
}

/// Where a resolved backend name came from — the introspection half of the ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommsOrigin {
    /// An explicit `comms.backends` entry.
    Explicit,
    /// Inherited from the rest of the config, named here (e.g. `tracker.github_summons`).
    Derived(&'static str),
    /// Nothing is configured: the default `none`.
    Default,
}

impl CommsOrigin {
    /// The stable token the boot log and any introspection render.
    pub fn as_str(self) -> &'static str {
        match self {
            CommsOrigin::Explicit => "comms.backends",
            CommsOrigin::Derived(key) => key,
            CommsOrigin::Default => "default",
        }
    }
}

/// A resolved communication backend: the name and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComms {
    /// The backend name, e.g. `github` or `none`.
    pub backend: String,
    pub origin: CommsOrigin,
}

impl ResolvedComms {
    /// Whether this build implements the resolved backend.
    pub fn is_implemented(&self) -> bool {
        comms_backend_is_implemented(&self.backend)
    }

    /// Whether the resolved backend is a name the daemon recognizes at all.
    pub fn is_recognized(&self) -> bool {
        comms_backend_is_recognized(&self.backend)
    }

    /// The identity this backend can express, or `None` for `none`/unrecognized.
    pub fn identity(&self) -> Option<CommsIdentity> {
        comms_backend_identity(&self.backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_implemented_backend_is_recognized() {
        for name in IMPLEMENTED_COMMS_BACKENDS {
            assert!(
                COMMS_BACKEND_NAMES.contains(name),
                "{name:?} is implemented, so it must remain a recognized name"
            );
        }
    }

    #[test]
    fn recognized_but_unimplemented_names_exist() {
        // The graceful-degradation path needs at least one; the ticket's own 5/5 backend is one.
        assert!(comms_backend_is_recognized("buzz"));
        assert!(!comms_backend_is_implemented("buzz"));
        assert!(comms_backend_is_recognized("slack"));
        assert!(!comms_backend_is_implemented("slack"));
    }

    #[test]
    fn unknown_name_is_not_recognized() {
        assert!(!comms_backend_is_recognized("telegram"));
        assert!(!comms_backend_is_recognized(""));
        assert!(comms_backend_identity("telegram").is_none());
    }

    #[test]
    fn github_and_linear_are_textual_and_github_cannot_name_the_sender() {
        let github = comms_backend_identity("github").expect("github is recognized");
        assert_eq!(github.representation, IdentityRepresentation::Textual);
        assert!(!github.representation.is_verified());
        assert!(
            !github.surfaces_author,
            "one shared account posts every GitHub comment, so no author can be read"
        );

        let linear = comms_backend_identity("linear").expect("linear is recognized");
        assert_eq!(linear.representation, IdentityRepresentation::Textual);
        assert!(!linear.representation.is_verified());
        assert!(
            linear.surfaces_author,
            "a Linear comment carries its author"
        );
    }

    #[test]
    fn buzz_is_cryptographic_and_slack_is_display_name_at_best() {
        let buzz = comms_backend_identity("buzz").expect("buzz is recognized");
        assert_eq!(buzz.representation, IdentityRepresentation::Cryptographic);
        assert!(buzz.representation.is_verified());

        let slack = comms_backend_identity("slack").expect("slack is recognized");
        assert_eq!(slack.representation, IdentityRepresentation::Textual);
        assert!(!slack.representation.is_verified());
    }

    #[test]
    fn origin_tokens_are_stable() {
        assert_eq!(CommsOrigin::Explicit.as_str(), "comms.backends");
        assert_eq!(
            CommsOrigin::Derived("tracker.github_summons").as_str(),
            "tracker.github_summons"
        );
        assert_eq!(CommsOrigin::Default.as_str(), "default");
        assert_eq!(IdentityRepresentation::Textual.as_str(), "textual");
        assert_eq!(
            IdentityRepresentation::Cryptographic.as_str(),
            "cryptographic"
        );
    }

    #[test]
    fn none_has_no_identity_to_express() {
        assert!(comms_backend_is_implemented("none"));
        assert!(comms_backend_identity("none").is_none());
    }
}
