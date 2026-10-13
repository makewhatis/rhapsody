//! Opt-in landing configuration and repository contract (no Go counterpart).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawLandingConfig", into = "RawLandingConfig")]
pub struct LandingConfig {
    pub enabled: bool,
    pub auto_revert: bool,
    pub deploy_timeout: Duration,
    pub watch: Duration,
    pub failure_threshold: u32,
    pub revert_review: bool,
    pub state: String,
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RawLandingConfig {
    enabled: Option<bool>,
    auto_revert: Option<bool>,
    deploy_timeout: Option<String>,
    watch: Option<String>,
    failure_threshold: Option<u32>,
    revert_review: Option<bool>,
    state: String,
    env_file: String,
}

impl TryFrom<RawLandingConfig> for LandingConfig {
    type Error = ContractError;
    fn try_from(r: RawLandingConfig) -> Result<Self, Self::Error> {
        let failure_threshold = r.failure_threshold.unwrap_or(2);
        if failure_threshold == 0 {
            return Err(error("landing.failure_threshold", "must be positive"));
        }
        Ok(Self {
            enabled: r.enabled.unwrap_or(false),
            auto_revert: r.auto_revert.unwrap_or(true),
            deploy_timeout: duration(
                r.deploy_timeout.as_deref().unwrap_or("30m"),
                "landing.deploy_timeout",
            )?,
            watch: duration(r.watch.as_deref().unwrap_or("10m"), "landing.watch")?,
            failure_threshold,
            revert_review: r.revert_review.unwrap_or(false),
            state: r.state,
            env_file: (!r.env_file.is_empty()).then(|| PathBuf::from(r.env_file)),
        })
    }
}

impl From<LandingConfig> for RawLandingConfig {
    fn from(l: LandingConfig) -> Self {
        let render = |d: Duration| {
            crate::go_duration_string(
                chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX),
            )
        };
        Self {
            enabled: Some(l.enabled),
            auto_revert: Some(l.auto_revert),
            deploy_timeout: Some(render(l.deploy_timeout)),
            watch: Some(render(l.watch)),
            failure_threshold: Some(l.failure_threshold),
            revert_review: Some(l.revert_review),
            state: l.state,
            env_file: l
                .env_file
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingContract {
    pub live: CheckSpec,
    /// Retained verbatim; the scheduler owns live cadence defaulting.
    pub live_every: Option<Duration>,
    pub live_timeout: Option<Duration>,
    pub checks: Vec<NamedCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedCheck {
    pub name: String,
    pub spec: CheckSpec,
    pub every: Option<Duration>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckSpec {
    Http(HttpCheck),
    Command(CommandCheck),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpCheck {
    pub url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body_contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCheck {
    pub argv: Vec<String>,
    pub expect_stdout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct ContractError {
    pub path: String,
    pub message: String,
}

fn error(path: &str, message: &str) -> ContractError {
    ContractError {
        path: path.into(),
        message: message.into(),
    }
}

fn duration(value: &str, path: &str) -> Result<Duration, ContractError> {
    let d = crate::decode::parse_go_duration(value).map_err(|e| error(path, &e))?;
    if d <= chrono::Duration::zero() {
        return Err(error(path, "must be positive"));
    }
    d.to_std().map_err(|e| error(path, &e.to_string()))
}

fn field_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.into()
    } else {
        format!("{path}.{key}")
    }
}

fn mapping<'a>(value: &'a Value, path: &str, keys: &[&str]) -> Result<&'a Mapping, ContractError> {
    let map = value
        .as_mapping()
        .ok_or_else(|| error(path, "expected a mapping"))?;
    for key in map.keys() {
        let key = key
            .as_str()
            .ok_or_else(|| error(path, "expected a string key"))?;
        if !keys.contains(&key) {
            return Err(error(&field_path(path, key), "unknown key"));
        }
    }
    Ok(map)
}

fn required<'a>(map: &'a Mapping, key: &str, path: &str) -> Result<&'a Value, ContractError> {
    map.get(key)
        .ok_or_else(|| error(&field_path(path, key), "required"))
}

fn string(value: &Value, path: &str) -> Result<String, ContractError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| error(path, "expected a string"))
}

fn optional_string(map: &Mapping, key: &str, path: &str) -> Result<Option<String>, ContractError> {
    map.get(key)
        .map(|v| string(v, &field_path(path, key)))
        .transpose()
}

fn optional_duration(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<Duration>, ContractError> {
    optional_string(map, key, path)?
        .map(|s| duration(&s, &field_path(path, key)))
        .transpose()
}

/// Parse the merged revision's `.rhapsody/landing.yaml`, refusing unknown keys at every level.
pub fn parse_contract(yaml: &str) -> Result<LandingContract, ContractError> {
    let value: Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| error("contract", &e.to_string()))?;
    let map = mapping(&value, "", &["version", "live", "checks"])?;
    if required(map, "version", "")?.as_u64() != Some(1) {
        return Err(error("version", "only version 1 is supported"));
    }
    let live = parse_check(required(map, "live", "")?, "live", false)?;
    let mut checks = Vec::new();
    if let Some(value) = map.get("checks") {
        let list = value
            .as_sequence()
            .ok_or_else(|| error("checks", "expected a sequence"))?;
        for (i, value) in list.iter().enumerate() {
            let check = parse_check(value, &format!("checks[{i}]"), true)?;
            if checks.iter().any(|c: &NamedCheck| c.name == check.name) {
                return Err(error(&format!("checks[{i}].name"), "duplicate name"));
            }
            checks.push(check);
        }
    }
    Ok(LandingContract {
        live: live.spec,
        live_every: live.every,
        live_timeout: live.timeout,
        checks,
    })
}

fn parse_check(value: &Value, path: &str, named: bool) -> Result<NamedCheck, ContractError> {
    let keys: &[&str] = if named {
        &[
            "name",
            "http",
            "command",
            "expect_stdout",
            "every",
            "timeout",
        ]
    } else {
        &["http", "command", "expect_stdout", "every", "timeout"]
    };
    let map = mapping(value, path, keys)?;
    let name = if named {
        string(required(map, "name", path)?, &field_path(path, "name"))?
    } else {
        String::new()
    };
    if named && name.trim().is_empty() {
        return Err(error(&field_path(path, "name"), "must not be empty"));
    }
    let every = optional_duration(map, "every", path)?;
    let timeout = optional_duration(map, "timeout", path)?;
    let spec = match (map.get("http"), map.get("command")) {
        (Some(http), None) => {
            if map.contains_key("expect_stdout") {
                return Err(error(
                    &field_path(path, "expect_stdout"),
                    "requires command",
                ));
            }
            CheckSpec::Http(parse_http(http, &field_path(path, "http"))?)
        }
        (None, Some(command)) => {
            let cp = field_path(path, "command");
            let list = command
                .as_sequence()
                .ok_or_else(|| error(&cp, "expected an argv sequence"))?;
            let argv = list
                .iter()
                .enumerate()
                .map(|(i, v)| string(v, &format!("{cp}[{i}]")))
                .collect::<Result<Vec<_>, _>>()?;
            if argv.first().is_none_or(|s| s.is_empty()) {
                return Err(error(&cp, "requires a nonempty executable"));
            }
            CheckSpec::Command(CommandCheck {
                argv,
                expect_stdout: optional_string(map, "expect_stdout", path)?,
            })
        }
        _ => return Err(error(path, "requires exactly one of http or command")),
    };
    Ok(NamedCheck {
        name,
        spec,
        every,
        timeout,
    })
}

fn parse_http(value: &Value, path: &str) -> Result<HttpCheck, ContractError> {
    let map = mapping(value, path, &["url", "status", "headers", "body_contains"])?;
    let url = string(required(map, "url", path)?, &field_path(path, "url"))?;
    let parsed =
        reqwest::Url::parse(&url).map_err(|e| error(&field_path(path, "url"), &e.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(error(
            &field_path(path, "url"),
            "requires an http or https URL",
        ));
    }
    let status = required(map, "status", path)?
        .as_u64()
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| {
            error(
                &field_path(path, "status"),
                "expected an HTTP status (100..599)",
            )
        })? as u16;
    let mut headers = BTreeMap::new();
    if let Some(value) = map.get("headers") {
        let hp = field_path(path, "headers");
        let hm = value
            .as_mapping()
            .ok_or_else(|| error(&hp, "expected a mapping"))?;
        for (key, value) in hm {
            let key = string(key, &hp)?;
            reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| error(&field_path(&hp, &key), &e.to_string()))?;
            headers.insert(key.clone(), string(value, &field_path(&hp, &key))?);
        }
    }
    Ok(HttpCheck {
        url,
        status,
        headers,
        body_contains: optional_string(map, "body_contains", path)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode, workflow::Definition};
    use std::time::Duration;

    fn config(yaml: &str) -> Result<crate::Config, crate::ConfigError> {
        decode(&Definition {
            config: serde_yaml_ng::from_str(yaml).unwrap(),
            prompt_template: String::new(),
        })
    }

    #[test]
    fn landing_absent_means_disabled_and_defaults_apply_when_present() {
        assert!(
            config("projects: [{slugs: [app]}]").unwrap().projects[0]
                .landing
                .is_none()
        );
        let c = config("projects: [{slugs: [app], landing: {}}]").unwrap();
        let l = c.projects[0].landing.as_ref().unwrap();
        assert!(!l.enabled);
        assert!(l.auto_revert);
        assert_eq!(l.deploy_timeout, Duration::from_secs(1800));
        assert_eq!(l.watch, Duration::from_secs(600));
        assert_eq!(l.failure_threshold, 2);
        assert!(!l.revert_review);
        assert_eq!(l.state, "");
        assert_eq!(l.env_file, None);
    }

    #[test]
    fn landing_unknown_key_is_an_error_naming_it() {
        let error = config("projects: [{landing: {enabeld: true}}]")
            .unwrap_err()
            .to_string();
        assert!(error.contains("enabeld"), "{error}");
    }

    #[test]
    fn landing_overrides_round_trip_without_collapsing_or_materializing_absence() {
        let c = config("projects: [{slugs: [app], landing: {enabled: true, auto_revert: false, deploy_timeout: 2m, watch: 1.5s, failure_threshold: 3, revert_review: true, state: Landing, env_file: /private/check.env}}]").unwrap();
        let l = c.projects[0].landing.as_ref().unwrap();
        assert_eq!(l.deploy_timeout, Duration::from_secs(120));
        assert_eq!(l.watch, Duration::from_millis(1500));
        assert!(l.enabled && l.revert_review && !l.auto_revert);
        assert_eq!(l.failure_threshold, 3);
        assert_eq!(l.state, "Landing");
        assert_eq!(
            l.env_file.as_deref(),
            Some(std::path::Path::new("/private/check.env"))
        );
        assert_eq!(decode(&encode(&c).unwrap()).unwrap().projects, c.projects);
        let absent = config("projects: [{slugs: [app], name: App}]").unwrap();
        assert!(
            !serde_yaml_ng::to_string(&encode(&absent).unwrap().config)
                .unwrap()
                .contains("landing")
        );
        for block in ["{}", "{enabled: false, env_file: ''}"] {
            let c = config(&format!("projects: [{{slugs: [app], landing: {block}}}]")).unwrap();
            assert_eq!(decode(&encode(&c).unwrap()).unwrap().projects, c.projects);
        }
    }

    #[test]
    fn landing_rejects_invalid_durations_and_thresholds() {
        for (key, value) in [
            ("watch", "wat"),
            ("deploy_timeout", "-1s"),
            ("watch", "0s"),
            ("failure_threshold", "0"),
        ] {
            let error = config(&format!("projects: [{{landing: {{{key}: {value}}}}}]"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn contract_parses_http_and_command_checks_with_every_and_timeout() {
        let c = parse_contract("version: 1\nlive: {command: [scripts/live]}\nchecks:\n  - name: health\n    http: {url: 'https://example.test/health', status: 200, headers: {x-ready: present}, body_contains: ok}\n    every: 30s\n    timeout: 10s\n  - name: migration\n    command: [scripts/schema, --read-only]\n    expect_stdout: '8|f'\n").unwrap();
        assert_eq!(c.checks.len(), 2);
        assert_eq!(c.checks[0].name, "health");
        assert_eq!(c.checks[0].every, Some(Duration::from_secs(30)));
        assert_eq!(c.checks[0].timeout, Some(Duration::from_secs(10)));
        let CheckSpec::Http(h) = &c.checks[0].spec else {
            panic!("http")
        };
        assert_eq!(h.url, "https://example.test/health");
        assert_eq!(h.status, 200);
        assert_eq!(h.headers["x-ready"], "present");
        assert_eq!(h.body_contains.as_deref(), Some("ok"));
        let CheckSpec::Command(cmd) = &c.checks[1].spec else {
            panic!("command")
        };
        assert_eq!(cmd.argv, ["scripts/schema", "--read-only"]);
        assert_eq!(cmd.expect_stdout.as_deref(), Some("8|f"));
        assert_eq!(c.checks[1].every, None);
        assert_eq!(c.checks[1].timeout, None);
    }

    #[test]
    fn contract_requires_live() {
        assert!(
            parse_contract("version: 1\nchecks: []")
                .unwrap_err()
                .to_string()
                .contains("live")
        );
    }

    #[test]
    fn contract_rejects_unknown_key_with_path() {
        for (yaml, path) in [
            (
                "version: 1\nlive: {command: ['/bin/true']}\nchecks: [{name: good, command: ['/bin/true']}, {name: bad, htp: {}}]",
                "checks[1].htp",
            ),
            (
                "version: 1\nlive: {http: {url: 'https://example.test', stats: 200}}",
                "live.http.stats",
            ),
            (
                "version: 1\nlive: {command: [true], evry: 30s}",
                "live.evry",
            ),
            ("version: 1\nlive: {command: [true]}\ncheckz: []", "checkz"),
        ] {
            let error = parse_contract(yaml).unwrap_err().to_string();
            assert!(error.contains(path), "{error}");
        }
    }

    #[test]
    fn contract_rejects_version_other_than_1() {
        for version in ["0", "2", "'1'", "null"] {
            assert!(
                parse_contract(&format!("version: {version}\nlive: {{command: [true]}}"))
                    .unwrap_err()
                    .to_string()
                    .contains("version")
            );
        }
    }

    #[test]
    fn contract_live_timing_survives_parsing_and_omitted_timings_remain_none() {
        let c = parse_contract(
            "version: 1\nlive: {command: [scripts/live], every: 30s, timeout: 1.5s}",
        )
        .unwrap();
        assert_eq!(c.live_every, Some(Duration::from_secs(30)));
        assert_eq!(c.live_timeout, Some(Duration::from_millis(1500)));
        let c = parse_contract(
            "version: 1\nlive: {http: {url: 'https://example.test/version', status: 200}}",
        )
        .unwrap();
        assert_eq!(c.live_every, None);
        assert_eq!(c.live_timeout, None);
        assert!(c.checks.is_empty());
    }

    #[test]
    fn contract_rejects_malformed_durations_with_path() {
        for path in ["every", "timeout"] {
            for value in ["wat", "0s", "-2s", "null"] {
                let error = parse_contract(&format!(
                    "version: 1\nlive: {{command: [scripts/live], {path}: {value}}}"
                ))
                .unwrap_err()
                .to_string();
                assert!(error.contains(&format!("live.{path}")), "{error}");
            }
            let error = parse_contract(&format!("version: 1\nlive: {{command: [scripts/live]}}\nchecks: [{{name: test, command: ['/bin/true'], {path}: wat}}]")).unwrap_err().to_string();
            assert!(error.contains(&format!("checks[0].{path}")), "{error}");
        }
    }

    #[test]
    fn contract_rejects_ambiguous_or_empty_checks() {
        for probe in [
            "{}",
            "{command: []}",
            "{command: ['']}",
            "{command: [true], http: {url: 'https://example.test', status: 200}}",
            "{http: {url: 'ftp://example.test', status: 200}}",
            "{http: {url: 'https://example.test', status: 999}}",
            "{http: {url: 'https://example.test', status: 200}, expect_stdout: x}",
        ] {
            assert!(
                parse_contract(&format!("version: 1\nlive: {probe}")).is_err(),
                "{probe}"
            );
        }
        assert!(
            parse_contract("version: 1\nlive: {command: [true]}\nchecks: [{command: [true]}]")
                .is_err()
        );
    }
}
