//! Credential diagnostics. Only configured owners are ever probed; responses are never printed.

use rhapsody_workspace::github::{Credentials, Keychain};
use std::sync::Arc;

pub async fn check(
    config: &rhapsody_config::Config,
    keychain: Arc<dyn Keychain>,
) -> (String, bool) {
    check_at(config, keychain, "https://api.github.com").await
}

async fn check_at(
    config: &rhapsody_config::Config,
    keychain: Arc<dyn Keychain>,
    endpoint: &str,
) -> (String, bool) {
    let Some(github) = config.github.clone() else {
        return ("No GitHub credentials configured.\n".into(), true);
    };
    let policy = Arc::new(Credentials::new(keychain, Arc::new(|_| {})));
    policy.update(Some(github.clone()));
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return ("GitHub check: HTTP client unavailable.\n".into(), false),
    };
    let mut report = String::new();
    let mut ok = true;
    let repos: Vec<_> = std::iter::once(&config.repo)
        .chain(config.projects.iter().map(|p| &p.repo))
        .filter_map(|repo| rhapsody_config::github::repo(repo))
        .collect();
    for owner in github.credentials.keys() {
        let auth = {
            let policy = policy.clone();
            let owner = owner.clone();
            tokio::task::spawn_blocking(move || policy.for_owner(&owner)).await
        };
        let auth = match auth {
            Ok(Ok(Some(auth))) => auth,
            _ => {
                report.push_str(&format!(
                    "{owner}: keychain absent/locked/unreadable; repo not checked\n"
                ));
                ok = false;
                continue;
            }
        };
        let Some((_, repo)) = repos.iter().find(|(o, _)| o == owner) else {
            report.push_str(&format!(
                "{owner}: keychain present; no configured repo to check\n"
            ));
            ok = false;
            continue;
        };
        let answer = client
            .get(format!("{endpoint}/repos/{owner}/{repo}"))
            .header(reqwest::header::USER_AGENT, "rhapsody")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(auth.token())
            .send()
            .await;
        match answer {
            Ok(response) if response.status().is_success() => report.push_str(&format!(
                "{owner}: keychain present; {owner}/{repo} accessible\n"
            )),
            Ok(response) => {
                report.push_str(&format!(
                    "{owner}: keychain present; {owner}/{repo} refused (HTTP {})\n",
                    response.status().as_u16()
                ));
                ok = false;
            }
            Err(_) => {
                report.push_str(&format!(
                    "{owner}: keychain present; {owner}/{repo} request failed\n"
                ));
                ok = false;
            }
        }
    }
    (report, ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct FakeKeychain;
    impl Keychain for FakeKeychain {
        fn read(&self, service: &str) -> Result<String, String> {
            match service {
                "studio49dev-ref" => Ok("fake-secret-never-print".into()),
                "makewhatis-ref" => Err("missing".into()),
                _ => panic!("unconfigured credential requested"),
            }
        }
    }
    #[tokio::test]
    async fn check_uses_only_configured_owners_and_never_prints_a_secret() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with("GET /repos/studio49dev/example "));
            assert!(request.contains("Bearer fake-secret-never-print"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 23\r\nConnection: close\r\n\r\nfake-secret-never-print").await.unwrap();
        });
        let def = rhapsody_config::workflow::Definition {
            config: serde_yaml_ng::from_str("github:\n  strict: true\n  credentials:\n    studio49dev: {keychain: studio49dev-ref}\n    makewhatis: {keychain: makewhatis-ref}\nprojects:\n  - slugs: [app]\n    repo: git@github.com:studio49dev/example.git\n  - slugs: [other]\n    repo: https://github.com/fake-unconfigured/example\n").unwrap(),
            prompt_template: String::new(),
        };
        let (report, ok) = check_at(
            &rhapsody_config::decode(&def).unwrap(),
            Arc::new(FakeKeychain),
            &endpoint,
        )
        .await;
        server.await.unwrap();
        assert!(!ok);
        assert!(report.contains("studio49dev/example accessible"));
        assert!(report.contains("makewhatis: keychain absent"));
        assert!(!report.contains("fake-secret"));
        assert!(!report.contains("fake-unconfigured"));
    }
}
