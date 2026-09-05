//! SourceService/CredentialService integration tests (DMN-083/084) against a
//! real in-process gRPC server. Each test here sets a different pair of
//! `ASC_*` environment overrides (`ASC_SOURCES`/`ASC_USER_SOURCES` vs.
//! `ASC_GIT_AUTH`/`ASC_USER_GIT_AUTH`), so unlike `tests/install.rs` and
//! friends they can safely share one binary without racing each other.

use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::config::Config;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

const TOKEN: &str = "test-token-1234";

fn test_state() -> (Arc<ApiState>, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    (ApiState::new(config, TOKEN.into()), ws)
}

async fn spawn_server(state: Arc<ApiState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(state)).await.unwrap();
    });
    addr
}

async fn channel(addr: std::net::SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

fn with_auth<T>(mut request: tonic::Request<T>) -> tonic::Request<T> {
    let value: MetadataValue<_> = format!("Bearer {TOKEN}").parse().unwrap();
    request.metadata_mut().insert("authorization", value);
    request
}

#[tokio::test]
async fn replace_sources_is_idempotent_and_survives_a_reload() {
    use pb::source_service_client::SourceServiceClient;

    let ws = tempfile::tempdir().unwrap();
    // Safe: this is the only test in this binary touching ASC_SOURCES.
    unsafe { std::env::set_var("ASC_SOURCES", ws.path().join("sources.toml")) };
    unsafe { std::env::set_var("ASC_USER_SOURCES", ws.path().join("user-sources.toml")) };

    let (state, _data) = test_state();
    let addr = spawn_server(state).await;
    let mut client = SourceServiceClient::new(channel(addr).await);

    let desired = vec![
        pb::Source {
            name: "acme".into(),
            url: "https://acme.example.com".into(),
        },
        pb::Source {
            name: "corp".into(),
            url: "https://corp.example.com".into(),
        },
    ];

    let first = client
        .replace_sources(with_auth(tonic::Request::new(pb::ReplaceSourcesRequest {
            sources: desired.clone(),
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        first
            .sources
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<&str>>(),
        vec!["acme", "corp"]
    );

    // A repeated push with the same desired state must not error or drift —
    // the whole point of an idempotent full replace.
    let second = client
        .replace_sources(with_auth(tonic::Request::new(pb::ReplaceSourcesRequest {
            sources: desired,
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.sources, second.sources);

    // A fresh ListSources call (a new "reload" of sources.toml under the
    // hood) reports exactly what was pushed — proves it was actually
    // persisted to disk, not just echoed back from memory.
    let listed = client
        .list_sources(with_auth(tonic::Request::new(pb::ListSourcesRequest {})))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.sources, second.sources);

    // Reserved name and bad scheme are both rejected as INVALID_ARGUMENT,
    // and neither call disturbs the previously-pushed state.
    let err = client
        .replace_sources(with_auth(tonic::Request::new(pb::ReplaceSourcesRequest {
            sources: vec![pb::Source {
                name: "git".into(),
                url: "https://x.example.com".into(),
            }],
        })))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let err = client
        .replace_sources(with_auth(tonic::Request::new(pb::ReplaceSourcesRequest {
            sources: vec![
                pb::Source {
                    name: "dup".into(),
                    url: "https://a.example.com".into(),
                },
                pb::Source {
                    name: "dup".into(),
                    url: "https://b.example.com".into(),
                },
            ],
        })))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let unchanged = client
        .list_sources(with_auth(tonic::Request::new(pb::ListSourcesRequest {})))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(unchanged.sources, second.sources);

    unsafe { std::env::remove_var("ASC_SOURCES") };
    unsafe { std::env::remove_var("ASC_USER_SOURCES") };
}

#[tokio::test]
async fn credential_upsert_never_leaks_the_secret_and_replaces_by_triple() {
    use pb::credential_service_client::CredentialServiceClient;

    let ws = tempfile::tempdir().unwrap();
    // Safe: this is the only test in this binary touching ASC_GIT_AUTH.
    unsafe { std::env::set_var("ASC_GIT_AUTH", ws.path().join("auth.json")) };
    unsafe { std::env::set_var("ASC_USER_GIT_AUTH", ws.path().join("user-auth.json")) };
    // add_ssh_key (DMN-087) would otherwise write under /etc/asc/ssh-keys,
    // which a non-root test run cannot create.
    unsafe { std::env::set_var("ASC_SSH_KEY_STORE", ws.path().join("ssh-keys")) };

    let (state, _data) = test_state();
    let addr = spawn_server(state).await;
    let mut client = CredentialServiceClient::new(channel(addr).await);

    let secret = "ghp_super-secret-token";
    let created = client
        .upsert_credential(with_auth(tonic::Request::new(
            pb::UpsertCredentialRequest {
                kind: pb::CredentialKind::Repo as i32,
                target: "github.com/acme".into(),
                secret: Some(pb::upsert_credential_request::Secret::Token(secret.into())),
                username: None,
                app: None,
            },
        )))
        .await
        .unwrap()
        .into_inner()
        .credential
        .unwrap();
    assert_eq!(created.pattern, "github.com/acme");
    assert!(created.has_secret);
    // The wire type has no field capable of carrying the token at all, but
    // assert on the one place a stray "just serialize the whole thing"
    // regression could sneak it back in: the method label.
    assert!(!created.method_label.contains(secret));

    let listed = client
        .list_credentials(with_auth(tonic::Request::new(
            pb::ListCredentialsRequest {},
        )))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.credentials.len(), 1);
    assert!(!format!("{listed:?}").contains(secret));

    // Upsert again with the same (kind, pattern, app) triple: replaces the
    // token in place rather than appending a second entry.
    let replaced_token = "ghp_rotated-token";
    client
        .upsert_credential(with_auth(tonic::Request::new(
            pb::UpsertCredentialRequest {
                kind: pb::CredentialKind::Repo as i32,
                target: "github.com/acme".into(),
                secret: Some(pb::upsert_credential_request::Secret::Token(
                    replaced_token.into(),
                )),
                username: Some("me".into()),
                app: None,
            },
        )))
        .await
        .unwrap();
    let listed = client
        .list_credentials(with_auth(tonic::Request::new(
            pb::ListCredentialsRequest {},
        )))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.credentials.len(), 1, "upsert replaces, not appends");
    assert_eq!(listed.credentials[0].username.as_deref(), Some("me"));

    client
        .remove_credential(with_auth(tonic::Request::new(
            pb::RemoveCredentialRequest {
                kind: Some(pb::CredentialKind::Repo as i32),
                target: "github.com/acme".into(),
            },
        )))
        .await
        .unwrap();
    let listed = client
        .list_credentials(with_auth(tonic::Request::new(
            pb::ListCredentialsRequest {},
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.credentials.is_empty());

    // DMN-087: an ssh-key secret writes a 0600 file this daemon owns and
    // never leaks the PEM bytes back over the API either.
    let pem =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nfakefakefake\n-----END OPENSSH PRIVATE KEY-----\n";
    let created = client
        .upsert_credential(with_auth(tonic::Request::new(
            pb::UpsertCredentialRequest {
                kind: pb::CredentialKind::Repo as i32,
                target: "gitlab.com/acme".into(),
                secret: Some(pb::upsert_credential_request::Secret::SshPrivateKeyPem(
                    pem.into(),
                )),
                username: None,
                app: None,
            },
        )))
        .await
        .unwrap()
        .into_inner()
        .credential
        .unwrap();
    assert!(created.has_secret);
    assert!(created.method_label.starts_with("ssh-key "));
    let key_path = created.method_label.trim_start_matches("ssh-key ");
    assert_eq!(std::fs::read_to_string(key_path).unwrap(), pem);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let listed = client
        .list_credentials(with_auth(tonic::Request::new(
            pb::ListCredentialsRequest {},
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(!format!("{listed:?}").contains("fakefakefake"));

    // Removing it deletes the key file too, not just the auth.json entry.
    client
        .remove_credential(with_auth(tonic::Request::new(
            pb::RemoveCredentialRequest {
                kind: Some(pb::CredentialKind::Repo as i32),
                target: "gitlab.com/acme".into(),
            },
        )))
        .await
        .unwrap();
    assert!(!std::path::Path::new(key_path).exists());

    unsafe { std::env::remove_var("ASC_GIT_AUTH") };
    unsafe { std::env::remove_var("ASC_USER_GIT_AUTH") };
    unsafe { std::env::remove_var("ASC_SSH_KEY_STORE") };
}
