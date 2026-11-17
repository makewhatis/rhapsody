//! WORKFLOW.md's CI route-back and landing-line policies; no Go counterpart.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ReviewCi {
    /// Maximum PRs updated/tested concurrently in a strict repository's landing line.
    pub landing_width: usize,
    pub ci_failure_routing: bool,
    pub ci_failure_escalation_hours: u32,
}

impl Default for ReviewCi {
    fn default() -> Self {
        Self {
            landing_width: 3,
            ci_failure_routing: true,
            ci_failure_escalation_hours: 2,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectReview {
    /// Absent inherits the global WORKFLOW.md review policy.
    pub landing_width: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::ReviewCi;

    #[test]
    fn landing_width_defaults_overrides_and_round_trips() {
        let d = crate::workflow::parse("---\nreview:\n  landing_width: 4\nprojects:\n  - slugs: [app]\n    review:\n      landing_width: 1\n  - slugs: [other]\n---\nprompt").unwrap();
        let c = crate::decode::decode(&d).unwrap();
        assert_eq!(ReviewCi::default().landing_width, 3);
        assert_eq!(c.review.landing_width, 4);
        assert_eq!(c.projects[0].review.landing_width, Some(1));
        assert_eq!(c.projects[1].review.landing_width, None);
        let next = crate::decode::decode(&crate::encode::encode(&c).unwrap()).unwrap();
        assert_eq!(next.review, c.review);
        assert_eq!(next.projects, c.projects);
    }

    #[test]
    fn invalid_landing_width_is_refused_globally_and_per_project() {
        for value in ["0", "-1", "1.5", "three"] {
            for yaml in [
                format!("review:\n  landing_width: {value}"),
                format!("projects:\n  - slugs: [app]\n    review:\n      landing_width: {value}"),
            ] {
                let d = crate::workflow::parse(&format!("---\n{yaml}\n---\nprompt")).unwrap();
                assert!(crate::decode::decode(&d).is_err(), "{yaml}");
            }
        }
    }

    #[test]
    fn single_project_width_survives_encode_without_collapsing_the_override() {
        let d = crate::workflow::parse(
            "---\nprojects:\n  - slugs: [app]\n    review:\n      landing_width: 1\n---\nprompt",
        )
        .unwrap();
        let c = crate::decode::decode(&d).unwrap();
        let next = crate::decode::decode(&crate::encode::encode(&c).unwrap()).unwrap();
        assert_eq!(next.projects, c.projects);
        assert_eq!(next.review.landing_width, 3);
    }

    #[test]
    fn ci_review_defaults_and_round_trip() {
        let d = crate::workflow::parse("---\n{}\n---\nprompt").unwrap();
        let c = crate::decode::decode(&d).unwrap();
        assert!(c.review.ci_failure_routing);
        assert_eq!(c.review.ci_failure_escalation_hours, 2);
        let d = crate::workflow::parse("---\nreview:\n  ci_failure_routing: false\n  ci_failure_escalation_hours: 5\n---\nprompt").unwrap();
        let c = crate::decode::decode(&d).unwrap();
        assert!(!c.review.ci_failure_routing);
        assert_eq!(c.review.ci_failure_escalation_hours, 5);
        let next = crate::decode::decode(&crate::encode::encode(&c).unwrap()).unwrap();
        assert_eq!(next.review, c.review);
    }

    #[test]
    fn zero_ci_escalation_is_refused() {
        let d =
            crate::workflow::parse("---\nreview:\n  ci_failure_escalation_hours: 0\n---\nprompt")
                .unwrap();
        assert!(crate::decode::decode(&d).is_err());
    }
}
