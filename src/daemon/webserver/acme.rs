//! Let's Encrypt for the web server's sites (DMN-124): ACME (RFC 8555)
//! with the HTTP-01 challenge answered from the nginx webroot.
//!
//! Before an order is placed, every name is self-checked: a probe file is
//! dropped into the webroot and fetched through the name over plain HTTP.
//! A name that does not lead back to this node fails right there, as
//! `pending_dns`, without spending Let's Encrypt's failed-validation quota.
//! A self-check that cannot connect at all (hairpin NAT: many hosts cannot
//! reach their own public address) is not proof of anything, so the order
//! goes ahead and Let's Encrypt has the final word.

use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{info, warn};

use super::model::Settings;
use super::render::Paths;
use super::write_atomic;

pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// A freshly issued certificate.
pub struct Issued {
    pub chain_pem: String,
    pub key_pem: String,
}

/// Why issuing failed — DNS problems are the operator's to fix and shown
/// as such, everything else is an ACME error.
#[derive(Debug)]
pub enum IssueError {
    Dns(String),
    Acme(String),
}

fn challenge_dir(paths: &Paths) -> PathBuf {
    paths.webroot.join(".well-known").join("acme-challenge")
}

fn random_token() -> String {
    use rustls::crypto::ring::default_provider;
    let mut bytes = [0u8; 16];
    let _ = default_provider().secure_random.fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Outcome of the self-check of one name.
#[derive(Debug, PartialEq, Eq)]
pub enum SelfCheck {
    Reachable,
    /// Could not connect — inconclusive, the order proceeds.
    Inconclusive(String),
    /// The name demonstrably does not reach this node.
    Wrong(String),
}

pub fn self_check(paths: &Paths, name: &str) -> SelfCheck {
    let addresses: Vec<String> = match (name, 80).to_socket_addrs() {
        Ok(addrs) => addrs.map(|a| a.ip().to_string()).collect(),
        Err(_) => {
            return SelfCheck::Wrong(format!(
                "{name} does not resolve: create an A/AAAA record pointing at this node"
            ));
        }
    };
    let token = format!("asc-probe-{}", random_token());
    let path = challenge_dir(paths).join(&token);
    if let Err(err) = write_atomic(&path, token.as_bytes(), 0o644) {
        return SelfCheck::Inconclusive(format!("cannot write the probe: {err:#}"));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("http://{name}/.well-known/acme-challenge/{token}");
    let result = agent.get(&url).call();
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(4096)
                .read_to_string()
                .unwrap_or_default();
            if status == 200 && body.trim() == token {
                SelfCheck::Reachable
            } else {
                SelfCheck::Wrong(format!(
                    "http://{name}/ does not reach this node's web server (HTTP {status}); \
                     {name} resolves to {}",
                    addresses.join(", ")
                ))
            }
        }
        Err(err) => SelfCheck::Inconclusive(format!("{err}")),
    }
}

fn account_path(paths: &Paths, directory: &str) -> PathBuf {
    let mut hash: u32 = 0x811c_9dc5;
    for b in directory.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    paths
        .state
        .join("acme")
        .join(format!("account-{hash:08x}.json"))
}

async fn account(settings: &Settings, paths: &Paths, directory: &str) -> Result<Account> {
    let path = account_path(paths, directory);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(credentials) = serde_json::from_slice::<AccountCredentials>(&bytes)
    {
        return Account::builder()?
            .from_credentials(credentials)
            .await
            .context("cannot restore the ACME account");
    }
    let contact =
        (!settings.acme_email.is_empty()).then(|| format!("mailto:{}", settings.acme_email));
    let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
    let (account, credentials) = Account::builder()?
        .create(
            &NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory.to_string(),
            None,
        )
        .await
        .context("cannot create the ACME account")?;
    let bytes = serde_json::to_vec_pretty(&credentials).context("cannot serialize the account")?;
    write_atomic(&path, &bytes, 0o600)?;
    info!(directory, "ACME account created");
    Ok(account)
}

async fn order(settings: &Settings, paths: &Paths, names: &[String]) -> Result<Issued> {
    let directory = if settings.acme_directory.is_empty() {
        LETS_ENCRYPT
    } else {
        settings.acme_directory.as_str()
    };
    let account = account(settings, paths, directory).await?;
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .context("cannot create the order")?;

    let dir = challenge_dir(paths);
    let mut written: Vec<PathBuf> = Vec::new();
    let result = async {
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz.context("cannot fetch an authorization")?;
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                other => return Err(anyhow!("authorization is {other:?}")),
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| anyhow!("the ACME server offered no HTTP-01 challenge"))?;
            let file = dir.join(&challenge.token);
            write_atomic(
                &file,
                challenge.key_authorization().as_str().as_bytes(),
                0o644,
            )?;
            written.push(file);
            challenge
                .set_ready()
                .await
                .context("cannot mark the challenge ready")?;
        }
        let retry = RetryPolicy::new().timeout(Duration::from_secs(120));
        let status = order
            .poll_ready(&retry)
            .await
            .context("the order did not settle")?;
        if status != OrderStatus::Ready {
            return Err(anyhow!(
                "validation failed: {}",
                failure_details(&mut order).await
            ));
        }
        let key_pem = order
            .finalize()
            .await
            .context("cannot finalize the order")?;
        let chain_pem = order
            .poll_certificate(&retry)
            .await
            .context("the certificate was not issued")?;
        Ok(Issued { chain_pem, key_pem })
    }
    .await;
    for file in written {
        let _ = std::fs::remove_file(file);
    }
    result
}

/// The ACME server's own explanation of a failed validation.
async fn failure_details(order: &mut instant_acme::Order) -> String {
    let mut details = Vec::new();
    let mut authorizations = order.authorizations();
    while let Some(Ok(authz)) = authorizations.next().await {
        for challenge in &authz.challenges {
            if let Some(problem) = &challenge.error {
                details.push(format!("{}: {problem}", authz.identifier()));
            }
        }
    }
    if details.is_empty() {
        "the ACME server rejected the order".into()
    } else {
        details.join("; ")
    }
}

/// Self-checks the names, then runs the order. Blocking — called from the
/// background pass or a worker thread.
pub fn issue(settings: &Settings, paths: &Paths, names: &[String]) -> Result<Issued, IssueError> {
    for name in names {
        match self_check(paths, name) {
            SelfCheck::Reachable => {}
            SelfCheck::Wrong(message) => return Err(IssueError::Dns(message)),
            SelfCheck::Inconclusive(why) => {
                warn!(name, %why, "self-check inconclusive, ordering anyway");
            }
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| IssueError::Acme(format!("cannot start the ACME runtime: {e}")))?
        .block_on(order(settings, paths, names))
        .map_err(|e| IssueError::Acme(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn unresolvable_names_are_dns_problems() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("root"),
            state: dir.path().join("state"),
            webroot: dir.path().join("www"),
        };
        match self_check(&paths, "nonexistent.invalid") {
            SelfCheck::Wrong(message) => assert!(message.contains("does not resolve")),
            other => panic!("{other:?}"),
        }
    }
}
