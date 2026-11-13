//! Per-run scratch-directory policy (STUDIO-1993); no Go counterpart.
//!
//! A dispatched run gets its own scratch directory under one daemon-owned root, so the build output
//! and temp files an agent creates (`cargo` target dirs, probe checkouts, review worktrees) land
//! somewhere the daemon can find and reclaim. Nothing in the frozen Go reference has this — the Go
//! daemon leaves agents to choose their own `$TMPDIR` paths, which is exactly the leak this fixes.
//!
//! The root is normalized in [`crate::resolve`] (like `workspace.root`), so it carries its default
//! there. The TTLs and the disk floor are read by the orchestrator's janitor and dispatch guard.

use serde::{Deserialize, Serialize};

/// The per-run scratch-directory policy (`scratch:` front matter). Rhapsody-only.
///
/// Every field has a working default, so an install that never writes the block behaves exactly as
/// the daemon did before the key existed, apart from the scratch root now being daemon-owned.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Scratch {
    /// Where per-run scratch directories are created. Empty ⇒ [`crate::resolve`] applies
    /// `~/.rhapsody/scratch`. One directory per run lives directly beneath it, named by the run id.
    pub root: String,
    /// How long a scratch directory whose run is no longer live may survive, in hours. `0` disables
    /// the managed-root janitor entirely.
    pub ttl_hours: i64,
    /// How long a directory under the legacy `$TMPDIR/opencode` root may survive, in hours. `0`
    /// disables the legacy-root janitor entirely.
    pub legacy_ttl_hours: i64,
    /// Hold new dispatches while free space on the scratch volume is below this many GiB.
    pub min_free_gib: u64,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            root: String::new(),
            ttl_hours: 24,
            legacy_ttl_hours: 48,
            min_free_gib: 50,
        }
    }
}

impl Scratch {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.ttl_hours < 0 {
            return Err("scratch.ttl_hours: must be nonnegative".into());
        }
        if self.legacy_ttl_hours < 0 {
            return Err("scratch.legacy_ttl_hours: must be nonnegative".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let d = Scratch::default();
        assert_eq!(d.root, "");
        assert_eq!(d.ttl_hours, 24);
        assert_eq!(d.legacy_ttl_hours, 48);
        assert_eq!(d.min_free_gib, 50);
    }

    #[test]
    fn scratch_block_round_trips_through_decode_and_encode() {
        let text = "---\nscratch:\n  root: /var/scratch\n  ttl_hours: 6\n  legacy_ttl_hours: 12\n  min_free_gib: 20\n---\nprompt";
        let def = crate::workflow::parse(text).unwrap();
        let cfg = crate::decode::decode(&def).unwrap();
        assert_eq!(cfg.scratch.root, "/var/scratch");
        assert_eq!(cfg.scratch.ttl_hours, 6);
        assert_eq!(cfg.scratch.legacy_ttl_hours, 12);
        assert_eq!(cfg.scratch.min_free_gib, 20);
        let encoded = crate::encode::encode(&cfg).unwrap();
        let next = crate::decode::decode(&encoded).unwrap();
        assert_eq!(next.scratch, cfg.scratch);
    }

    #[test]
    fn a_negative_ttl_is_refused() {
        let text = "---\nscratch:\n  ttl_hours: -1\n---\nprompt";
        let def = crate::workflow::parse(text).unwrap();
        assert!(crate::decode::decode(&def).is_err());
    }

    #[test]
    fn an_unconfigured_block_encodes_back_to_nothing() {
        // The default block must not be written into a saved workflow, so an install that never
        // configured scratch stays byte-identical on a console Save.
        let def = crate::workflow::parse("---\ntracker:\n  kind: linear\n  project_slug: p\n---\nprompt").unwrap();
        let cfg = crate::decode::decode(&def).unwrap();
        let encoded = crate::encode::encode(&cfg).unwrap();
        assert!(
            !encoded.config.contains_key("scratch"),
            "a default scratch block must not be serialized"
        );
    }
}
