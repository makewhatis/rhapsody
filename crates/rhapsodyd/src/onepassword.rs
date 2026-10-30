//! The daemon-side 1Password service-account source (STUDIO-1584). Rhapsody-only; no Go counterpart.
//!
//! It resolves a bound credential id to its token over the SAME authenticated desktop channel the
//! provider credentials use ([`crate::credential_client::CredentialResolver`]) — the daemon never
//! touches the OS Keychain itself. A 1Password credential is addressed by the `op:v1:<id>` account
//! and pinned to a fixed adapter ([`ONEPASSWORD_SERVICE_ACCOUNT_V1`]), so the shared read path can
//! carry it without widening the provider protocol match. The token is handed only to the
//! orchestrator's source seam, which puts it in an agent subprocess env and nowhere else.

use std::sync::Arc;

use async_trait::async_trait;
use rhapsody_credential_ipc::domain::{
    Binding, CredentialRef, CredentialState, ONEPASSWORD_SERVICE_ACCOUNT_V1,
};
use rhapsody_orchestrator::onepassword::{OnePasswordRead, OnePasswordSource};

use crate::credential_client::CredentialResolver;

/// A [`OnePasswordSource`] backed by the daemon's authenticated credential channel.
pub struct ResolverOnePasswordSource {
    resolver: Arc<CredentialResolver>,
}

impl ResolverOnePasswordSource {
    pub fn new(resolver: Arc<CredentialResolver>) -> ResolverOnePasswordSource {
        ResolverOnePasswordSource { resolver }
    }
}

#[async_trait]
impl OnePasswordSource for ResolverOnePasswordSource {
    async fn read(&self, id: &str) -> OnePasswordRead {
        let Ok(credential_ref) = CredentialRef::for_onepassword(id) else {
            // A non-canonical id cannot be a stored credential; treat it as absent (validate already
            // refuses such a binding at config load, so this is defense in depth).
            return OnePasswordRead::Absent;
        };
        let binding = Binding {
            provider_id: id.to_string(),
            adapter: ONEPASSWORD_SERVICE_ACCOUNT_V1.to_string(),
            base_url: String::new(),
        };
        let observed = self
            .resolver
            .read_bound(credential_ref.account().to_string(), binding)
            .await;
        match observed.read.state {
            CredentialState::Present(lease) => {
                // `LeasePayload` owns a zeroizing `Drop`, so its value is TAKEN (moved out) rather
                // than copied out of a field.
                let mut payload = lease.into_lease_payload();
                OnePasswordRead::Present(std::mem::take(&mut payload.value))
            }
            CredentialState::Absent => OnePasswordRead::Absent,
            CredentialState::DeniedOrLocked => {
                OnePasswordRead::Unavailable("credential store locked or access denied".into())
            }
            CredentialState::Malformed => {
                OnePasswordRead::Unavailable("stored credential is malformed".into())
            }
            CredentialState::BindingMismatch => {
                OnePasswordRead::Unavailable("stored credential is bound to a different id".into())
            }
            CredentialState::OwnerUnavailable => {
                OnePasswordRead::Unavailable("credential owner unavailable".into())
            }
            CredentialState::OwnerUnauthorized => {
                OnePasswordRead::Unavailable("credential owner rejected the daemon".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_credential_ipc::domain::Revision;
    use rhapsody_credential_ipc::session::ServerSession;
    use rhapsody_credential_ipc::wire::read_frame;
    use rhapsody_credential_ipc::wire::{
        ClientFrame, HelloFrame, LeasePayload, ServerFrame, write_frame,
    };

    // The source reads `op:v1:<id>` over the real channel and unwraps a Present lease into the token.
    #[tokio::test]
    async fn resolves_a_present_token_over_a_real_channel() {
        use tokio::net::UnixListener;
        let dir =
            std::path::PathBuf::from("/tmp").join(format!("rd-op-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (mut r, mut w) = tokio::io::split(stream);
            let mut session =
                ServerSession::new(rhapsody_credential_ipc::session::Token::new("tok".into()));
            let hello: HelloFrame = read_frame(&mut r).await.expect("hello");
            session.accept_hello(&hello.token).expect("hello accepted");
            let ClientFrame::ReadBound { seq, account, .. } =
                read_frame(&mut r).await.expect("read");
            assert_eq!(account, "op:v1:studio-infra-readonly");
            session.accept_client_seq(seq).expect("seq");
            let resp = session.next_outgoing_seq();
            write_frame(
                &mut w,
                &ServerFrame::ReadBoundResult {
                    seq: resp,
                    revision: Revision(1),
                    state: rhapsody_credential_ipc::domain::CredentialStateTag::Present,
                    lease: Some(LeasePayload {
                        binding: Binding {
                            provider_id: "studio-infra-readonly".into(),
                            adapter: ONEPASSWORD_SERVICE_ACCOUNT_V1.into(),
                            base_url: String::new(),
                        },
                        value: "ops_sentinel_value".into(),
                    }),
                },
            )
            .await
            .expect("write result");
        });

        let resolver = Arc::new(CredentialResolver::new());
        resolver.adopt_bootstrap(Some(rhapsody_credential_ipc::wire::BootstrapMessage {
            token: "tok".into(),
            socket_path: path.to_string_lossy().into_owned(),
        }));
        let source = ResolverOnePasswordSource::new(resolver);
        match source.read("studio-infra-readonly").await {
            OnePasswordRead::Present(token) => assert_eq!(token, "ops_sentinel_value"),
            other => panic!("expected Present, got {other:?}"),
        }
        server.await.ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unreachable_owner_is_unavailable() {
        let resolver = Arc::new(CredentialResolver::new());
        resolver.adopt_bootstrap(None);
        let source = ResolverOnePasswordSource::new(resolver);
        assert!(matches!(
            source.read("studio-infra-readonly").await,
            OnePasswordRead::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn a_non_canonical_id_is_absent() {
        let resolver = Arc::new(CredentialResolver::new());
        resolver.adopt_bootstrap(None);
        let source = ResolverOnePasswordSource::new(resolver);
        assert!(matches!(
            source.read("Not An Id").await,
            OnePasswordRead::Absent
        ));
    }
}
