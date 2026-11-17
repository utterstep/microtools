//! Signing in to HEY, and keeping the tokens it issued fresh and on disk.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use hey_sdk::oauth::{self, ExchangeRequest, OAuthClient, RefreshRequest, ServerMetadata, Token};
use hey_sdk::{SensitiveString, TokenProvider};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::state::write_private;

/// An access token this close to expiry is renewed before it is used.
const RENEW_AHEAD: TimeDelta = TimeDelta::minutes(2);
/// HEY rate-limits failed grants, so a refresh that failed is not tried again at once.
const RENEW_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// What a sign-in leaves behind: the tokens, and the installation they were issued to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Identifies this installation to HEY; every token request has to carry it.
    pub install_id: String,
    pub token: Token,
}

impl Credentials {
    pub fn load(path: &Path) -> Result<Credentials> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "not signed in: no credentials at {} (run `hey-forward login`)",
                    path.display()
                )
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        serde_json::from_slice(&bytes)
            .with_context(|| format!("credentials file {} is unreadable", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Hands the HEY client its access token, renewing it shortly before it expires or when
/// HEY refuses it, and writing every renewal back to disk — HEY rotates refresh tokens,
/// so one that is not saved is lost with the process.
pub struct StoredTokens {
    path: PathBuf,
    oauth: OAuthClient,
    token_endpoint: String,
    client_id: String,
    held: Mutex<Credentials>,
    renewing: tokio::sync::Mutex<()>,
    last_failure: Mutex<Option<Instant>>,
}

impl StoredTokens {
    pub fn open(path: PathBuf, hey: &hey_sdk::Config) -> Result<StoredTokens> {
        let held = Credentials::load(&path)?;
        Ok(StoredTokens {
            path,
            oauth: OAuthClient::default(),
            token_endpoint: ServerMetadata::for_hey(&hey.base_url).token_endpoint,
            client_id: hey.oauth_client_id.clone(),
            held: Mutex::new(held),
            renewing: tokio::sync::Mutex::new(()),
            last_failure: Mutex::new(None),
        })
    }

    fn held(&self) -> MutexGuard<'_, Credentials> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn last_failure(&self) -> MutexGuard<'_, Option<Instant>> {
        self.last_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the access token `stale`, and answers whether a newer one is now held.
    async fn renew(&self, stale: &SensitiveString) -> bool {
        let _one_at_a_time = self.renewing.lock().await;
        if self.held().token.access_token != *stale {
            return true;
        }
        // Another process — a fresh `login`, a `check` that renewed — may have put newer
        // credentials on disk, and with them spent the refresh token held here.
        if let Ok(on_disk) = Credentials::load(&self.path)
            && on_disk.token.access_token != *stale
        {
            info!("picked up newer credentials from disk");
            *self.held() = on_disk;
            return true;
        }
        if self
            .last_failure()
            .is_some_and(|at| at.elapsed() < RENEW_COOLDOWN)
        {
            return false;
        }

        let Credentials { install_id, token } = self.held().clone();
        let Some(refresh_token) = token.refresh_token else {
            error!("HEY issued no refresh token: run `hey-forward login` again");
            *self.last_failure() = Some(Instant::now());
            return false;
        };
        let request = RefreshRequest {
            token_endpoint: self.token_endpoint.clone(),
            refresh_token: refresh_token.clone(),
            client_id: self.client_id.clone(),
            client_secret: None,
            install_id: install_id.clone(),
        };
        match self.oauth.refresh(&request).await {
            Ok(mut token) => {
                // An answer without a refresh token means "keep the one you have".
                token.refresh_token.get_or_insert(refresh_token);
                let renewed = Credentials { install_id, token };
                if let Err(error) = renewed.save(&self.path) {
                    error!(
                        "renewed the access token but could not save it: {error:#}; \
                         a restart may need `hey-forward login` again"
                    );
                }
                *self.held() = renewed;
                *self.last_failure() = None;
                debug!("access token renewed");
                true
            }
            Err(error) => {
                warn!(
                    "could not renew the access token: {error}; if this persists, run `hey-forward login` again"
                );
                *self.last_failure() = Some(Instant::now());
                false
            }
        }
    }
}

#[async_trait]
impl TokenProvider for StoredTokens {
    async fn access_token(&self) -> Result<String, hey_sdk::Error> {
        let (current, expiring) = {
            let held = self.held();
            let expiring = held
                .token
                .expires_at
                .is_some_and(|at| at - Utc::now() < RENEW_AHEAD);
            (held.token.access_token.clone(), expiring)
        };
        if expiring {
            // Should this fail, the token held goes out anyway: HEY has the last word on
            // whether it is still good.
            self.renew(&current).await;
        }
        Ok(self.held().token.access_token.expose().to_string())
    }

    async fn refresh(&self) -> bool {
        let stale = self.held().token.access_token.clone();
        self.renew(&stale).await
    }
}

/// Runs HEY's OAuth sign-in (authorization code with PKCE) and saves the credentials.
///
/// HEY sends the browser back to a listener on this machine's loopback interface. When the
/// browser is on another machine — the usual case for a homelab — that redirect cannot
/// arrive, so the address the browser was sent to can be pasted in instead.
pub async fn login(hey: &hey_sdk::Config, path: &Path) -> Result<()> {
    let oauth = OAuthClient::default();
    let metadata = ServerMetadata::for_hey(&hey.base_url);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("cannot listen for the sign-in redirect")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );

    let pkce = oauth::generate_pkce();
    let state = oauth::generate_state();
    // The install id outlives a sign-in: it names this installation, not this session.
    let install_id = match Credentials::load(path) {
        Ok(previous) => previous.install_id,
        Err(_) => oauth::generate_state(),
    };
    let url = oauth::authorization_url(
        &metadata,
        &hey.oauth_client_id,
        &redirect_uri,
        None,
        &state,
        &pkce,
        &install_id,
    )?;

    println!("Open this address in a browser and approve access:\n\n  {url}\n");
    println!(
        "If that browser runs on another machine, it will end up on a page that fails to\n\
         load ({redirect_uri}?code=…). Copy that page's address\n\
         from the address bar and paste it here, then press Enter.\n"
    );

    let redirect = wait_for_redirect(&listener).await?;
    ensure!(
        redirect.state == state,
        "the redirect does not belong to this sign-in (state mismatch); start over"
    );

    let token = oauth
        .exchange(&ExchangeRequest {
            token_endpoint: metadata.token_endpoint,
            code: redirect.code.into(),
            redirect_uri,
            client_id: hey.oauth_client_id.clone(),
            client_secret: None,
            code_verifier: pkce.verifier,
            install_id: install_id.clone(),
        })
        .await
        .context("HEY refused the sign-in")?;
    if token.refresh_token.is_none() {
        warn!(
            "HEY issued no refresh token: this sign-in lasts only until the access token expires"
        );
    }
    Credentials { install_id, token }.save(path)?;
    println!("Signed in. Credentials saved to {}", path.display());
    Ok(())
}

/// What HEY's redirect carries back.
#[derive(Debug, PartialEq, Eq)]
struct Redirect {
    code: String,
    state: String,
}

/// Waits for the redirect to reach the listener, or for its address to be pasted on stdin,
/// whichever comes first.
async fn wait_for_redirect(listener: &TcpListener) -> Result<Redirect> {
    let mut pasted = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted.context("the sign-in listener failed")?;
                match answer_redirect(socket).await {
                    Ok(Some(redirect)) => return redirect,
                    Ok(None) => {}
                    Err(error) => debug!("ignored a request to the sign-in listener: {error:#}"),
                }
            }
            line = pasted.next_line(), if stdin_open => match line {
                Ok(Some(line)) if line.trim().is_empty() => {}
                Ok(Some(line)) => match parse_redirect(line.trim()) {
                    Ok(redirect) => return Ok(redirect),
                    Err(error) => println!("Could not use that: {error:#}. Paste the whole address:"),
                },
                // Nothing to paste from; the listener is still worth waiting on.
                Ok(None) | Err(_) => stdin_open = false,
            },
        }
    }
}

/// Reads one HTTP request off the listener and answers it. `Ok(None)` is a request that
/// was not the redirect — a browser asking for a favicon, say.
async fn answer_redirect(mut socket: TcpStream) -> Result<Option<Result<Redirect>>> {
    let mut request_line = String::new();
    {
        // The request line, then the headers up to the blank line; 16 KiB is more than a
        // redirect needs.
        let mut reader = BufReader::new((&mut socket).take(16 * 1024));
        reader.read_line(&mut request_line).await?;
        let mut header = String::new();
        loop {
            header.clear();
            let read = reader.read_line(&mut header).await?;
            if read == 0 || header == "\r\n" || header == "\n" {
                break;
            }
        }
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let (status, body, outcome) = if target.starts_with("/callback") {
        match parse_redirect(target) {
            Ok(redirect) => (
                "200 OK",
                "Signed in. You can close this tab.".to_string(),
                Some(Ok(redirect)),
            ),
            Err(error) => ("400 Bad Request", format!("{error:#}"), Some(Err(error))),
        }
    } else {
        ("404 Not Found", "Nothing here.".to_string(), None)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await.ok();
    Ok(outcome)
}

/// Picks the code and state out of the address HEY redirected to: a whole URL as copied
/// from an address bar, or the path and query a browser requests.
fn parse_redirect(input: &str) -> Result<Redirect> {
    let url = if input.starts_with('/') {
        Url::parse(&format!("http://127.0.0.1{input}"))
    } else {
        Url::parse(input)
    }
    .context("that is not the address HEY redirected to")?;

    let parameter = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if let Some(error) = parameter("error") {
        let description = parameter("error_description").unwrap_or_default();
        bail!("HEY declined the sign-in: {error} {description}");
    }
    match (parameter("code"), parameter("state")) {
        (Some(code), Some(state)) => Ok(Redirect { code, state }),
        _ => bail!("that address carries no sign-in code"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redirect_reads_from_a_pasted_address_or_a_request_target() {
        let expected = Redirect {
            code: "abc".to_string(),
            state: "xyz".to_string(),
        };
        assert_eq!(
            parse_redirect("http://127.0.0.1:4242/callback?code=abc&state=xyz").unwrap(),
            expected
        );
        assert_eq!(
            parse_redirect("/callback?state=xyz&code=abc").unwrap(),
            expected
        );
    }

    #[test]
    fn a_refusal_or_an_unrelated_address_is_reported() {
        let refusal = parse_redirect("/callback?error=access_denied&state=xyz").unwrap_err();
        assert!(refusal.to_string().contains("access_denied"));
        assert!(parse_redirect("https://app.hey.com/imbox").is_err());
        assert!(parse_redirect("not an address").is_err());
    }
}
