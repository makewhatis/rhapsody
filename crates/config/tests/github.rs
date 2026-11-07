use rhapsody_config::{decode, encode, workflow::Definition};

fn definition(yaml: &str) -> Definition {
    Definition {
        config: serde_yaml_ng::from_str(yaml).unwrap(),
        prompt_template: String::new(),
    }
}

#[test]
fn github_references_survive_config_saves() {
    let cfg = decode(&definition("github:\n  strict: true\n  credentials:\n    studio49dev: {keychain: rhapsody-github-studio49dev}\n")).unwrap();
    let saved = encode(&cfg).unwrap();
    let text = serde_yaml_ng::to_string(&saved.config).unwrap();
    assert!(text.contains("rhapsody-github-studio49dev"));
    assert_eq!(decode(&saved).unwrap(), cfg);
}

#[test]
fn github_literals_are_refused_without_echoing_secrets() {
    for binding in [
        "github_pat_FAKE_SECRET",
        "{keychain: github_pat_FAKE_SECRET}",
        "{token: ghp_FAKE_SECRET}",
        "{keychain: legitimate, token: ghp_FAKE_SECRET}",
    ] {
        let err = decode(&definition(&format!(
            "github:\n  credentials:\n    studio49dev: {binding}\n"
        )))
        .unwrap_err()
        .to_string();
        assert!(err.contains("keychain"), "{err}");
        assert!(!err.contains("FAKE_SECRET"), "{err}");
        let view = rhapsody_config::effective_json::render(&definition(&format!(
            "github:\n  credentials:\n    studio49dev: {binding}\n"
        )));
        assert!(!view.to_string().contains("FAKE_SECRET"));
    }
}

#[test]
fn absent_github_stays_absent() {
    let saved =
        encode(&decode(&definition("repo: https://github.com/makewhatis/rhapsody")).unwrap())
            .unwrap();
    assert!(!saved.config.contains_key("github"));
}
