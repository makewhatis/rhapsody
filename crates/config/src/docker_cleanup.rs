//! Docker leftover-cleanup policy (STUDIO-2316); no Go counterpart.
//!
//! Agents start Postgres containers, compose projects and testcontainers inside their runs, and
//! nothing removed them when the run ended: an operator's Mac carried 30 leftover containers (some
//! ten days old), and leftover `<ticket>_default` compose networks once exhausted Docker's address
//! pools. The daemon now labels what a run starts (`RHAPSODY_RUN_ID`, `COMPOSE_PROJECT_NAME`),
//! removes a run's containers when its workspace is cleaned up, and reaps old leftovers daily. This
//! block configures that; the orchestrator's `dockercleanup` module does the work.

use serde::{Deserialize, Serialize};

/// The ticket-shaped container-name patterns seen in practice (`studio2297-postgres`,
/// `carini-pg-1986`, `s2312-pg`, `study-2077-fix-pg`, `pr_makewhatis_study_58_alice-postgres-1`).
/// An UNLABELLED container is only ever reaped when its name matches one of these AND it is older
/// than [`DockerCleanup::max_age_days`].
pub const DEFAULT_NAME_PATTERNS: &[&str] = &[
    "^studio",
    "^carini-pg",
    "^study",
    "^s[0-9]{4}",
    "^pr_[a-z0-9_]+",
];

/// The `docker_cleanup:` front-matter block. Rhapsody-only; every field has a working default.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DockerCleanup {
    /// Master switch for all three halves (run labelling hint, workspace-cleanup removal, the daily
    /// reaper). `false` makes the daemon touch Docker not at all.
    pub enabled: bool,
    /// How old an UNLABELLED container must be, in days, before the reaper may remove it. Also the
    /// age past which a labelled container whose run is not live is removed even when the store
    /// cannot say the run ended (a crash orphan, or a store-disabled daemon).
    pub max_age_days: i64,
    /// Regexes an unlabelled container's name must match to be reaped. An operator extends the
    /// allowlist here; an empty list means no unlabelled container is ever reaped.
    pub name_patterns: Vec<String>,
    /// Log every removal the reaper and the workspace cleanup WOULD make, and remove nothing.
    pub dry_run: bool,
}

impl Default for DockerCleanup {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age_days: 7,
            name_patterns: DEFAULT_NAME_PATTERNS
                .iter()
                .map(|p| (*p).to_string())
                .collect(),
            dry_run: false,
        }
    }
}

impl DockerCleanup {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.max_age_days < 1 {
            return Err("docker_cleanup.max_age_days: must be at least 1".into());
        }
        for p in &self.name_patterns {
            if let Err(e) = regex::Regex::new(p) {
                return Err(format!(
                    "docker_cleanup.name_patterns: invalid regex {p:?}: {e}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let d = DockerCleanup::default();
        assert!(d.enabled);
        assert_eq!(d.max_age_days, 7);
        assert!(!d.dry_run);
        assert_eq!(d.name_patterns, DEFAULT_NAME_PATTERNS);
    }

    #[test]
    fn block_round_trips_through_decode_and_encode() {
        let text = "---\ndocker_cleanup:\n  enabled: false\n  max_age_days: 3\n  name_patterns: ['^foo']\n  dry_run: true\n---\nprompt";
        let def = crate::workflow::parse(text).unwrap();
        let cfg = crate::decode::decode(&def).unwrap();
        assert!(!cfg.docker_cleanup.enabled);
        assert_eq!(cfg.docker_cleanup.max_age_days, 3);
        assert_eq!(cfg.docker_cleanup.name_patterns, vec!["^foo".to_string()]);
        assert!(cfg.docker_cleanup.dry_run);
        let encoded = crate::encode::encode(&cfg).unwrap();
        let next = crate::decode::decode(&encoded).unwrap();
        assert_eq!(next.docker_cleanup, cfg.docker_cleanup);
    }

    #[test]
    fn a_partial_block_keeps_the_other_defaults() {
        let text = "---\ndocker_cleanup:\n  dry_run: true\n---\nprompt";
        let def = crate::workflow::parse(text).unwrap();
        let cfg = crate::decode::decode(&def).unwrap();
        assert!(cfg.docker_cleanup.enabled);
        assert_eq!(cfg.docker_cleanup.max_age_days, 7);
        assert_eq!(cfg.docker_cleanup.name_patterns, DEFAULT_NAME_PATTERNS);
    }

    #[test]
    fn a_bad_age_or_regex_is_refused() {
        for text in [
            "---\ndocker_cleanup:\n  max_age_days: 0\n---\nprompt",
            "---\ndocker_cleanup:\n  name_patterns: ['(']\n---\nprompt",
        ] {
            let def = crate::workflow::parse(text).unwrap();
            assert!(crate::decode::decode(&def).is_err(), "{text}");
        }
    }

    #[test]
    fn an_unconfigured_block_encodes_back_to_nothing() {
        let def =
            crate::workflow::parse("---\ntracker:\n  kind: linear\n  project_slug: p\n---\nprompt")
                .unwrap();
        let cfg = crate::decode::decode(&def).unwrap();
        let encoded = crate::encode::encode(&cfg).unwrap();
        assert!(!encoded.config.contains_key("docker_cleanup"));
    }
}
