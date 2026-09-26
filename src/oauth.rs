// ---------------------------------------------------------------------------
// ChatGPT (Codex) OAuth client + credential store.
//
// Implements the browser PKCE flow that the Codex CLI uses (public client),
// stores the resulting tokens in `{name}.oauth.json` next to the backend's
// `.env` file, and refreshes them when they approach expiry.
// ---------------------------------------------------------------------------

use crate::config;
use crate::http;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// OAuth public client id used by the Codex CLI (no secret; PKCE).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
pub const DEFAULT_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PORT_FALLBACK: u16 = 1457;
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the ChatGPT backend API lives. Overridable so protocol changes
/// don't require a rebuild.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// The account's model catalog. Upstream requires a `client_version` query
/// parameter and answers 400 without it; the value itself is not validated,
/// so this only has to be plausible.
pub const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
pub const CODEX_CLIENT_VERSION: &str = "0.157.0";

const CALLBACK_OK_PAGE: &str = "<!doctype html><html><body style=\"font-family:system-ui\">\
<h2>Login successful</h2><p>You can close this tab and return to claude-switch.</p></body></html>";
const CALLBACK_ERR_PAGE: &str = "<!doctype html><html><body style=\"font-family:system-ui\">\
<h2>Login failed</h2><p>Authorization was not completed. You can close this tab.</p></body></html>";

// ---------------------------------------------------------------------------
// Credential storage
// ---------------------------------------------------------------------------

/// One entry of the account's ChatGPT model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Upstream's own preference ordering — lower is more preferred.
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthCredentials {
    pub kind: String,
    pub version: u32,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub small_model: Option<String>,
    /// Every model the account may call, in upstream preference order.
    /// Absent in credentials written before the catalog was captured.
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub last_refresh_epoch_secs: u64,
}

/// Load the credential file for a backend, if present and parseable.
pub fn load_credentials(config_dir: &Path, name: &str) -> Option<OauthCredentials> {
    let path = config::oauth_json_path(config_dir, name);
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Atomically save credentials with 0600 permissions.
pub fn save_credentials(config_dir: &Path, name: &str, creds: &OauthCredentials) -> anyhow::Result<()> {
    fs::create_dir_all(config_dir)?;
    let path = config::oauth_json_path(config_dir, name);
    let tmp = config_dir.join(format!("{}.oauth.json.tmp", name));
    let json = serde_json::to_string_pretty(creds)?;
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(json.as_bytes())?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PKCE + authorize URL
// ---------------------------------------------------------------------------

pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkcePair {
    let mut bytes = vec![0u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(&bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair { verifier, challenge }
}

fn url_escape(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// The issuer to authenticate against (overridable for tests/future changes).
pub fn issuer() -> String {
    std::env::var("CS_OAUTH_ISSUER").unwrap_or_else(|_| DEFAULT_ISSUER.to_string())
}

/// The codex/responses endpoint (overridable for tests/future changes).
pub fn responses_url() -> String {
    std::env::var("CS_CODEX_RESPONSES_URL").unwrap_or_else(|_| CODEX_RESPONSES_URL.to_string())
}

/// The model-catalog endpoint, with the mandatory `client_version` parameter.
/// Overridable like the other endpoints so a protocol change doesn't force a
/// rebuild. Note the similarly-named `/backend-api/models` is a *different*
/// catalog (the ChatGPT app's model picker) whose entries are not callable
/// through codex/responses — it is deliberately not used as a fallback.
pub fn models_url() -> String {
    if let Ok(url) = std::env::var("CS_CODEX_MODELS_URL") {
        return url;
    }
    let version = std::env::var("CS_CODEX_CLIENT_VERSION")
        .unwrap_or_else(|_| CODEX_CLIENT_VERSION.to_string());
    format!("{}?client_version={}", CODEX_MODELS_URL, version)
}

pub fn build_authorize_url(issuer: &str, redirect_uri: &str, state: &str, challenge: &str) -> String {
    format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=claude-switch",
        issuer.trim_end_matches('/'),
        url_escape(CLIENT_ID),
        url_escape(redirect_uri),
        url_escape(DEFAULT_SCOPES),
        url_escape(challenge),
        url_escape(state),
    )
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn open_browser(url: &str) -> Result<(), String> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/c", "start", "", url]).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "could not open browser (exit {}) — open this URL manually:\n{}",
            s, url
        )),
        Err(e) => Err(format!(
            "could not open browser: {} — open this URL manually:\n{}",
            e, url
        )),
    }
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

fn oauth_error_text(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error_description")
                .or_else(|| v.get("error"))
                .and_then(|e| e.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

fn exchange_code(issuer: &str, code: &str, verifier: &str, redirect_uri: &str) -> Result<TokenResponse, String> {
    let url = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "authorization_code",
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect_uri,
    });
    let resp = ureq::post(&url).set("Content-Type", "application/json").send_json(body);
    match resp {
        Ok(r) => r
            .into_json::<TokenResponse>()
            .map_err(|e| format!("token response parse error: {}", e)),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(format!("token endpoint returned {}: {}", code, oauth_error_text(&body)))
        }
        Err(ureq::Error::Transport(e)) => Err(format!("token endpoint unreachable: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RefreshError {
    /// The refresh token is dead — the user must log in again.
    Permanent(String),
    /// Network or temporary server trouble — keep the old credentials.
    Transient(String),
}

/// Refresh `creds` in place against the issuer's token endpoint.
pub fn refresh(creds: &mut OauthCredentials) -> Result<(), RefreshError> {
    let url = format!("{}/oauth/token", issuer().trim_end_matches('/'));
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": creds.refresh_token,
    });
    match ureq::post(&url).set("Content-Type", "application/json").send_json(body) {
        Ok(r) => {
            let tok: TokenResponse = r
                .into_json()
                .map_err(|e| RefreshError::Transient(format!("token response parse error: {}", e)))?;
            creds.access_token = tok.access_token;
            if let Some(rt) = tok.refresh_token {
                creds.refresh_token = rt; // rotation: persist the new token
            }
            if let Some(idt) = tok.id_token {
                creds.id_token = Some(idt);
            }
            creds.last_refresh_epoch_secs = now_epoch_secs();
            Ok(())
        }
        Err(ureq::Error::Status(400, r)) => {
            let body = r.into_string().unwrap_or_default();
            if body.contains("invalid_grant") {
                Err(RefreshError::Permanent("refresh token rejected (invalid_grant)".into()))
            } else {
                Err(RefreshError::Transient(format!("400: {}", oauth_error_text(&body))))
            }
        }
        Err(ureq::Error::Status(401, _)) => Err(RefreshError::Permanent("refresh rejected (401)".into())),
        Err(ureq::Error::Status(code, r)) => Err(RefreshError::Transient(format!(
            "{}: {}",
            code,
            oauth_error_text(&r.into_string().unwrap_or_default())
        ))),
        Err(ureq::Error::Transport(e)) => Err(RefreshError::Transient(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Login flow
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum OauthOutcome {
    Success {
        account_id: Option<String>,
        plan_type: Option<String>,
        email: Option<String>,
        roles: ModelRoles,
    },
    Error { message: String },
}

/// Start the browser login on a background thread (for the TUI).
pub fn start_login(
    config_dir: PathBuf,
    name: String,
) -> (mpsc::Receiver<OauthOutcome>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let outcome = login_blocking(&config_dir, &name);
        let _ = tx.send(outcome);
    });
    (rx, handle)
}

/// Run the browser login to completion (blocks; used by the TUI thread and
/// the `--login-oauth` CLI flag).
pub fn login_blocking(config_dir: &Path, name: &str) -> OauthOutcome {
    match login_inner(config_dir, name) {
        Ok(creds) => OauthOutcome::Success {
            account_id: creds.account_id.clone(),
            plan_type: creds.plan_type.clone(),
            email: creds.email.clone(),
            roles: model_roles(&creds.models),
        },
        Err(message) => OauthOutcome::Error { message },
    }
}

fn login_inner(config_dir: &Path, name: &str) -> Result<OauthCredentials, String> {
    let (listener, port) = bind_callback_listener()?;
    let state = random_hex(32);
    let pkce = generate_pkce();
    let issuer = issuer();
    let redirect_uri = format!("http://127.0.0.1:{}/auth/callback", port);
    let auth_url = build_authorize_url(&issuer, &redirect_uri, &state, &pkce.challenge);
    open_browser(&auth_url)?;

    let code = wait_for_callback(&listener, &state)?;
    let token = exchange_code(&issuer, &code, &pkce.verifier, &redirect_uri)?;

    // Account + plan come from the id_token's OpenAI auth claim.
    let claims = token.id_token.as_deref().and_then(decode_id_token_payload);
    let auth_claim = claims
        .as_ref()
        .and_then(|c| c.get("https://api.openai.com/auth"));
    let account_id = auth_claim
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let plan_type = auth_claim
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let email = claims
        .as_ref()
        .and_then(|c| c.get("email"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Best-effort default models from the ChatGPT catalog.
    let models = fetch_models(&token.access_token, account_id.as_deref());
    let roles = model_roles(&models);

    let creds = OauthCredentials {
        kind: "chatgpt-codex-oauth".into(),
        version: 1,
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .ok_or_else(|| "token response missing refresh_token".to_string())?,
        id_token: token.id_token,
        account_id,
        plan_type,
        email,
        default_model: Some(roles.main),
        small_model: Some(roles.small),
        models,
        last_refresh_epoch_secs: now_epoch_secs(),
    };
    save_credentials(config_dir, name, &creds).map_err(|e| format!("could not save credentials: {}", e))?;
    Ok(creds)
}

fn bind_callback_listener() -> Result<(TcpListener, u16), String> {
    for port in [CALLBACK_PORT, CALLBACK_PORT_FALLBACK] {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok((l, port));
        }
    }
    let l = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("could not start the login callback server: {}", e))?;
    let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
    Ok((l, port))
}

/// Accept exactly one callback request, verify `state`, return the `code`.
fn wait_for_callback(listener: &TcpListener, state: &str) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("callback server error: {}", e))?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let req = http::read_request(&mut stream, Duration::from_secs(10))
                    .map_err(|e| format!("callback read error: {}", e))?;
                let params = req.query();
                let code = params
                    .iter()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.clone());
                let got_state = params
                    .iter()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.clone());
                match (code, got_state) {
                    (None, _) => {
                        let _ = http::write_response(&mut stream, 200, "text/html", CALLBACK_ERR_PAGE.as_bytes());
                        return Err("authorization failed in the browser (no code returned)".into());
                    }
                    (Some(_), got) if got.as_deref() != Some(state) => {
                        let _ = http::write_response(&mut stream, 200, "text/html", CALLBACK_ERR_PAGE.as_bytes());
                        return Err("OAuth state mismatch — aborting login".into());
                    }
                    (Some(code), _) => {
                        let _ = http::write_response(&mut stream, 200, "text/html", CALLBACK_OK_PAGE.as_bytes());
                        return Ok(code);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("login timed out after 120s — try again".into());
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("callback server error: {}", e)),
        }
    }
}

/// Decode the payload segment of a JWT without verifying its signature.
pub fn decode_id_token_payload(id_token: &str) -> Option<serde_json::Value> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort fetch of the current ChatGPT codex model ids. Non-fatal:
/// callers fall back to `config::DEFAULT_*_MODEL` when this fails.
/// Fetch the account's model catalog, sorted by upstream preference.
///
/// Returns an empty vec on any failure — the caller then falls back to the
/// backend's configured model ids. Set `CS_CATALOG_DEBUG=1` to log why.
///
/// Model *names* are not stable (they have already moved from `gpt-5.x-codex`
/// to `gpt-6-*`), so nothing here may key off a name pattern.
pub fn fetch_models(access_token: &str, account_id: Option<&str>) -> Vec<ModelInfo> {
    let debug = std::env::var("CS_CATALOG_DEBUG").is_ok();
    let url = models_url();
    // Bounded overall: the proxy backfills the catalog before it binds, so a
    // hung request here would delay the daemon becoming healthy.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout(Duration::from_secs(8))
        .build();

    let mut req = agent
        .get(&url)
        .set("Authorization", &format!("Bearer {}", access_token));
    // Without the account header the endpoint answers 403.
    if let Some(acc) = account_id {
        req = req.set("ChatGPT-Account-Id", acc);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            if debug {
                let body = r.into_string().unwrap_or_default();
                eprintln!(
                    "catalog: GET {} -> {} body={}",
                    url,
                    code,
                    body.chars().take(300).collect::<String>()
                );
            }
            return Vec::new();
        }
        Err(e) => {
            if debug {
                eprintln!("catalog: GET {} failed: {}", url, e);
            }
            return Vec::new();
        }
    };
    let json: serde_json::Value = match resp.into_json() {
        Ok(j) => j,
        Err(e) => {
            if debug {
                eprintln!("catalog: GET {} not JSON: {}", url, e);
            }
            return Vec::new();
        }
    };
    // Tolerate both {"models":[...]} and {"data":[...]} shapes.
    let Some(arr) = json
        .get("models")
        .or_else(|| json.get("data"))
        .and_then(|v| v.as_array())
    else {
        if debug {
            eprintln!(
                "catalog: GET {} -> unexpected shape, keys {:?}",
                url,
                json.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
        }
        return Vec::new();
    };
    let mut models: Vec<ModelInfo> = arr.iter().filter_map(parse_model_entry).collect();
    if debug {
        eprintln!(
            "catalog: {} entries, {} usable",
            arr.len(),
            models.len()
        );
    }
    models.sort_by(|a, b| {
        a.priority
            .unwrap_or(i64::MAX)
            .cmp(&b.priority.unwrap_or(i64::MAX))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    models
}

/// Parse one catalog entry, dropping models the account cannot call over the
/// API (the catalog also lists internal/hidden ones).
fn parse_model_entry(v: &serde_json::Value) -> Option<ModelInfo> {
    let slug = v
        .get("slug")
        .or_else(|| v.get("id"))
        .and_then(|s| s.as_str())?
        .to_string();
    if slug.is_empty() {
        return None;
    }
    if v.get("visibility").and_then(|x| x.as_str()) == Some("hide") {
        return None;
    }
    if v.get("supported_in_api").and_then(|x| x.as_bool()) == Some(false) {
        return None;
    }
    Some(ModelInfo {
        slug,
        display_name: v
            .get("display_name")
            .and_then(|x| x.as_str())
            .map(String::from),
        context_window: v.get("context_window").and_then(|x| x.as_u64()),
        priority: v.get("priority").and_then(|x| x.as_i64()),
    })
}

/// The model ids Claude Code exposes as its Opus / Sonnet / Haiku slots.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRoles {
    /// Strongest catalog entry — the Opus slot and the default model.
    pub main: String,
    /// Second-strongest — the Sonnet slot and the subagent default.
    pub mid: String,
    /// Cheapest entry — the Haiku slot and the small/fast model.
    pub small: String,
}

/// Assign catalog entries to Claude Code's model slots.
///
/// Claude Code's `/model` picker is *not* driven by `GET /v1/models` — it shows
/// a fixed set of presets plus whatever these role variables name. So the way
/// to make GPT models selectable is to point the roles at catalog entries.
///
/// The catalog arrives sorted by upstream preference: the first entry is the
/// flagship and the last is the cheapest thing the account can call. Catalogs
/// shorter than three entries reuse what they have rather than leaving a slot
/// empty; an empty catalog falls back to the built-in defaults.
pub fn model_roles(models: &[ModelInfo]) -> ModelRoles {
    let main = models
        .first()
        .map(|m| m.slug.clone())
        .unwrap_or_else(|| config::DEFAULT_MAIN_MODEL.to_string());
    let small = models
        .last()
        .map(|m| m.slug.clone())
        .unwrap_or_else(|| config::DEFAULT_SMALL_MODEL.to_string());
    let mid = models
        .get(1)
        .map(|m| m.slug.clone())
        .unwrap_or_else(|| main.clone());
    ModelRoles { main, mid, small }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pkce() {
        let pkce = generate_pkce();
        assert!(pkce.verifier.len() >= 43); // 64 random bytes → 86 chars base64url
        assert!(!pkce.challenge.is_empty());
        assert_ne!(pkce.verifier, pkce.challenge);
    }

    #[test]
    fn test_build_authorize_url() {
        let url = build_authorize_url(
            "https://auth.openai.com",
            "http://127.0.0.1:1455/auth/callback",
            "state123",
            "challenge456",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state123"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn test_decode_id_token_payload() {
        // header.payload.signature with a fake, unpadded payload
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"a@b.c","https://api.openai.com/auth":{"chatgpt_account_id":"user-1","chatgpt_plan_type":"plus"}}"#);
        let jwt = format!("{}.{}.sig", header, payload);
        let claims = decode_id_token_payload(&jwt).unwrap();
        assert_eq!(claims["email"], "a@b.c");
        assert_eq!(claims["https://api.openai.com/auth"]["chatgpt_account_id"], "user-1");
    }

    #[test]
    fn test_save_load_credentials_roundtrip() {
        let dir = std::env::temp_dir().join("claude-switch-oauth-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let creds = OauthCredentials {
            kind: "chatgpt-codex-oauth".into(),
            version: 1,
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: None,
            account_id: Some("user-1".into()),
            plan_type: Some("plus".into()),
            email: None,
            default_model: None,
            small_model: None,
            models: Vec::new(),
            last_refresh_epoch_secs: 1,
        };
        save_credentials(&dir, "gpt", &creds).unwrap();
        let loaded = load_credentials(&dir, "gpt").unwrap();
        assert_eq!(loaded.access_token, "at");
        assert_eq!(loaded.account_id.as_deref(), Some("user-1"));
        // Permissions are 0600 on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config::oauth_json_path(&dir, "gpt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_credentials_missing() {
        let dir = std::env::temp_dir().join("claude-switch-oauth-missing");
        assert!(load_credentials(&dir, "nope").is_none());
    }

    /// Credentials written before the catalog was captured must still load.
    #[test]
    fn test_credentials_without_catalog_field_load() {
        let json = r#"{"kind":"chatgpt-codex-oauth","version":1,
                       "access_token":"at","refresh_token":"rt"}"#;
        let c: OauthCredentials = serde_json::from_str(json).unwrap();
        assert!(c.models.is_empty());
    }

    #[test]
    fn test_parse_model_entry_filters_hidden_and_non_api() {
        let visible: serde_json::Value = serde_json::json!({
            "slug": "gpt-6-astra",
            "display_name": "GPT-6-Astra",
            "context_window": 272000,
            "priority": 1,
            "visibility": "list",
            "supported_in_api": true,
        });
        let m = parse_model_entry(&visible).unwrap();
        assert_eq!(m.slug, "gpt-6-astra");
        assert_eq!(m.display_name.as_deref(), Some("GPT-6-Astra"));
        assert_eq!(m.context_window, Some(272_000));
        assert_eq!(m.priority, Some(1));

        let hidden = serde_json::json!({"slug": "gpt-reserve", "visibility": "hide"});
        assert!(parse_model_entry(&hidden).is_none());

        let cli_only = serde_json::json!({"slug": "x", "supported_in_api": false});
        assert!(parse_model_entry(&cli_only).is_none());

        assert!(parse_model_entry(&serde_json::json!({"foo": 1})).is_none());
        assert!(parse_model_entry(&serde_json::json!({"slug": ""})).is_none());
    }

    fn model(slug: &str, priority: i64) -> ModelInfo {
        ModelInfo {
            slug: slug.into(),
            display_name: None,
            context_window: None,
            priority: Some(priority),
        }
    }

    /// Regression: the catalog used to be filtered by `slug.contains("codex")`,
    /// which silently emptied it once upstream renamed every model to `gpt-6-*`.
    #[test]
    fn test_model_roles_does_not_depend_on_name_pattern() {
        let models = vec![
            model("gpt-6-astra", 1),
            model("gpt-6-sol", 2),
            model("gpt-6-luna", 3),
            model("gpt-5.6-sol", 4),
            model("gpt-5.5", 12),
        ];
        let roles = model_roles(&models);
        assert_eq!(roles.main, "gpt-6-astra");
        assert_eq!(roles.mid, "gpt-6-sol");
        assert_eq!(roles.small, "gpt-5.5");
    }

    /// The catalog endpoint rejects requests without `client_version`.
    #[test]
    fn test_models_url_carries_client_version() {
        let url = models_url();
        assert!(url.starts_with("https://chatgpt.com/backend-api/codex/models?"));
        assert!(url.contains("client_version="));
        assert!(!url.ends_with("client_version="));
    }

    #[test]
    fn test_model_roles_empty_catalog_falls_back_to_defaults() {
        let roles = model_roles(&[]);
        assert_eq!(roles.main, config::DEFAULT_MAIN_MODEL);
        assert_eq!(roles.small, config::DEFAULT_SMALL_MODEL);
        assert_eq!(roles.mid, config::DEFAULT_MAIN_MODEL);
    }

    #[test]
    fn test_model_roles_single_entry_fills_every_slot() {
        let roles = model_roles(&[model("only", 1)]);
        assert_eq!(roles.main, "only");
        assert_eq!(roles.mid, "only");
        assert_eq!(roles.small, "only");
    }

    #[test]
    fn test_model_roles_two_entries_mid_is_the_second() {
        let roles = model_roles(&[model("strong", 1), model("cheap", 9)]);
        assert_eq!(roles.main, "strong");
        assert_eq!(roles.mid, "cheap");
        assert_eq!(roles.small, "cheap");
    }
}
