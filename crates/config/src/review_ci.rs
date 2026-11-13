//! WORKFLOW.md's CI route-back policy (STUDIO-1994); no Go counterpart.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ReviewCi {
    pub ci_failure_routing: bool,
    pub ci_failure_escalation_hours: u32,
}

impl Default for ReviewCi {
    fn default() -> Self {
        Self {
            ci_failure_routing: true,
            ci_failure_escalation_hours: 2,
        }
    }
}

#[cfg(test)]
mod tests {
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
