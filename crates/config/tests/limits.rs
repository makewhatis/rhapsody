use rhapsody_config::{decode, encode, workflow::Definition};

fn config(yaml: &str) -> rhapsody_config::Config {
    decode(&Definition {
        config: serde_yaml_ng::from_str(yaml).unwrap(),
        prompt_template: String::new(),
    })
    .unwrap()
}

#[test]
fn limits_defaults_and_roundtrip() {
    let default = config("{}");
    assert_eq!(default.limits.credits, "never");
    assert_eq!(default.limits.wait_max_minutes, 30);
    assert_eq!(default.limits.handoff_grace_minutes, 10);
    assert_eq!(default.limits.thresholds.warn, 80.0);
    assert_eq!(default.limits.thresholds.stop_new, 90.0);
    assert_eq!(default.limits.thresholds.handoff, 95.0);
    let cfg = config(
        "limits:\n  credits: daily_cap\n  credits_daily_usd: 2\n  wait_max_minutes: 15\n  handoff_grace_minutes: 5\n  accounts:\n    claude-subscription:\n      thresholds: {stop_new: 85}\n",
    );
    assert_eq!(cfg.limits.for_account("claude-subscription").stop_new, 85.0);
    assert_eq!(cfg.limits.for_account("claude-subscription").warn, 80.0);
    assert_eq!(decode(&encode(&cfg).unwrap()).unwrap().limits, cfg.limits);
    assert!(!encode(&default).unwrap().config.contains_key("limits"));
}

#[test]
fn limits_invalid_policy_and_thresholds_refused() {
    for yaml in [
        "limits: {credits: sometimes}",
        "limits: {credits_daily_usd: -1}",
        "limits: {wait_max_minutes: -1}",
        "limits: {handoff_grace_minutes: -1}",
        "limits: {thresholds: {warn: 101}}",
        "limits: {thresholds: {stop_new: 75}}",
        "limits: {accounts: {x: {thresholds: {handoff: 70}}}}",
    ] {
        assert!(
            decode(&Definition {
                config: serde_yaml_ng::from_str(yaml).unwrap(),
                prompt_template: String::new()
            })
            .is_err(),
            "{yaml}"
        );
    }
}
