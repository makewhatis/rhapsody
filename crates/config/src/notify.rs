//! notify — the recognized one-way notification backends (STUDIO-1504).
//!
//! **Notification is one-way; communication is bidirectional.** This module is the notification
//! axis only: a fire-and-forget `send`, no identity, no reply path, no receipt. The bidirectional
//! axis (an agent identity and a reply surface) is a separate, deliberately unentangled design.
//!
//! This is the `harness:` / `memory.backend` precedent for the `notify:` block: exactly one place
//! spells out the names the daemon RECOGNIZES, kept apart from the names it IMPLEMENTS. Config
//! validation accepts every recognized name; the composition root resolves a name to a concrete
//! backend and, for a recognized-but-unimplemented one, degrades with a warning rather than
//! failing the event that triggered the notification. `codex` is `HARNESS_NAMES`'s precedent for
//! this exact split; [`IMPLEMENTED_NOTIFY_BACKENDS`] is this module's.
//!
//! Deliberately **names only** — the resolution and the concrete channels live where the config
//! is consumed (`rhapsody_orchestrator::notify`), so this crate keeps no dependency on a transport.

/// The `notify.backend` values this daemon RECOGNIZES. `none`, `macos`, `ntfy` and `webhook` are
/// implemented; `email` and `pager` are named because they are squarely in the one-way category
/// the design describes (`ntfy`, macOS, email, a pager) and a config that names one should degrade
/// loudly rather than look like an unknown typo. Adding a name here makes config accept it; it does
/// NOT give the composition root a channel for it — that stays a separate, per-build decision.
pub const NOTIFY_BACKEND_NAMES: &[&str] = &["none", "macos", "ntfy", "webhook", "email", "pager"];

/// The `notify.backend` names this build can actually deliver through. A subset of
/// [`NOTIFY_BACKEND_NAMES`], and pinned by a test so the two cannot silently drift the way the
/// harness registry once did.
pub const IMPLEMENTED_NOTIFY_BACKENDS: &[&str] = &["none", "macos", "ntfy", "webhook"];

/// Whether `name` is a recognized backend. `""` is not recognized on purpose: an empty name means
/// "inherit the legacy `notify:` keys", which the resolver settles before it ever asks this.
pub fn notify_backend_is_recognized(name: &str) -> bool {
    NOTIFY_BACKEND_NAMES.contains(&name)
}

/// Whether this build can deliver through `name`.
pub fn notify_backend_is_implemented(name: &str) -> bool {
    IMPLEMENTED_NOTIFY_BACKENDS.contains(&name)
}

/// Where a resolved backend name came from — the introspection half of the ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOrigin {
    /// An explicit `notify.backend` value.
    Explicit,
    /// Derived from a legacy channel key, named here (e.g. `notify.ntfy`).
    Derived(&'static str),
    /// Nothing is configured: the default `none`.
    Default,
}

impl NotifyOrigin {
    /// The stable token the boot log and any introspection render. Stable because an operator
    /// greps it.
    pub fn as_str(self) -> &'static str {
        match self {
            NotifyOrigin::Explicit => "notify.backend",
            NotifyOrigin::Derived(key) => key,
            NotifyOrigin::Default => "default",
        }
    }
}

/// A resolved notification backend: the name and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNotify {
    /// The backend name, e.g. `ntfy` or `none`.
    pub backend: String,
    pub origin: NotifyOrigin,
}

impl ResolvedNotify {
    /// Whether this build implements the resolved backend.
    pub fn is_implemented(&self) -> bool {
        notify_backend_is_implemented(&self.backend)
    }

    /// Whether the resolved backend is a name the daemon recognizes at all.
    pub fn is_recognized(&self) -> bool {
        notify_backend_is_recognized(&self.backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_implemented_backend_is_recognized() {
        for name in IMPLEMENTED_NOTIFY_BACKENDS {
            assert!(
                NOTIFY_BACKEND_NAMES.contains(name),
                "{name:?} is implemented, so it must remain a recognized name"
            );
        }
    }

    #[test]
    fn recognized_but_unimplemented_names_exist() {
        // The graceful-degradation path needs at least one; `email` is the design's own example.
        assert!(notify_backend_is_recognized("email"));
        assert!(!notify_backend_is_implemented("email"));
        assert!(notify_backend_is_recognized("pager"));
        assert!(!notify_backend_is_implemented("pager"));
    }

    #[test]
    fn unknown_name_is_not_recognized() {
        assert!(!notify_backend_is_recognized("telegram"));
        assert!(!notify_backend_is_recognized(""));
    }

    #[test]
    fn origin_tokens_are_stable() {
        assert_eq!(NotifyOrigin::Explicit.as_str(), "notify.backend");
        assert_eq!(NotifyOrigin::Derived("notify.ntfy").as_str(), "notify.ntfy");
        assert_eq!(NotifyOrigin::Default.as_str(), "default");
    }
}
