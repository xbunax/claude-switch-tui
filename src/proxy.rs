// ---------------------------------------------------------------------------
// Anthropic↔ChatGPT translation proxy (daemon mode, `claude-switch --serve`).
//
// Claude Code speaks the Anthropic Messages API; the ChatGPT consumer backend
// speaks the OpenAI Responses API (chatgpt.com/backend-api/codex/responses).
// This module translates between the two: it listens on 127.0.0.1, converts
// incoming Anthropic requests, streams the translated SSE response back, and
// refreshes the OAuth token when needed.
//
// Everything is blocking (ureq + std::thread) to match the rest of the app.
// ---------------------------------------------------------------------------

use crate::config::{self, Backend};
use crate::daemon;
use crate::http::{self, SseWriter};
use crate::oauth::{self, OauthCredentials, RefreshError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle window for upstream socket reads — a timeout doubles as the cadence
/// for the `event: ping` keepalives we send to Claude Code.
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Proactively refresh when the token was last refreshed more than this long
/// ago (access tokens live for hours; this keeps the first request fast).
const PROACTIVE_REFRESH_AGE_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Model configuration
// ---------------------------------------------------------------------------

pub struct ModelConfig {
    pub main: String,
    pub small: String,
    /// Every model slug the account can call, in upstream preference order.
    /// Filled from the credentials' login-time catalog; empty for credentials
    /// written before the catalog was captured.
    pub catalog: Vec<oauth::ModelInfo>,
}

impl ModelConfig {
    pub fn from_backend(env: &HashMap<String, String>) -> Self {
        let main = unaliased(
            env.get("ANTHROPIC_MODEL")
                .map(String::as_str)
                .unwrap_or(config::DEFAULT_MAIN_MODEL),
        );
        let small = unaliased(
            env.get("ANTHROPIC_SMALL_FAST_MODEL")
                .map(String::as_str)
                .unwrap_or(config::DEFAULT_SMALL_MODEL),
        );
        Self {
            main,
            small,
            catalog: Vec::new(),
        }
    }

    /// Model ids to advertise to Claude Code: the full catalog when we have
    /// one, otherwise just the two the backend was configured with.
    fn advertised(&self) -> Vec<(&str, &str)> {
        if !self.catalog.is_empty() {
            return self
                .catalog
                .iter()
                .map(|m| (m.slug.as_str(), m.display_name.as_deref().unwrap_or(&m.slug)))
                .collect();
        }
        let mut out = vec![(self.main.as_str(), self.main.as_str())];
        if self.small != self.main {
            out.push((self.small.as_str(), self.small.as_str()));
        }
        out
    }
}

fn unaliased(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    if m.is_empty() {
        config::DEFAULT_MAIN_MODEL.into()
    } else {
        m.to_string()
    }
}

/// Map a Claude Code model id to an upstream ChatGPT model id. The `claude-`
/// prefix is used to pass Claude Code's model-picker filter; strip it here.
pub fn normalize_model(model: &str, cfg: &ModelConfig) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    if m == cfg.main || m == cfg.small || cfg.catalog.iter().any(|c| c.slug == m) {
        return m.to_string();
    }
    // Unknown claude-* aliases fall back to the configured main model.
    if model.starts_with("claude-") {
        return cfg.main.clone();
    }
    m.to_string()
}

// ---------------------------------------------------------------------------
// Daemon state
// ---------------------------------------------------------------------------

struct CredsState {
    creds: Option<OauthCredentials>,
    stale: bool,
}

pub struct ProxyState {
    config_dir: PathBuf,
    name: String,
    creds: Mutex<CredsState>,
    models: ModelConfig,
    port: u16,
}

impl ProxyState {
    /// Return usable credentials, refreshing proactively when stale-ish.
    fn get_creds(&self) -> Result<OauthCredentials, String> {
        let mut guard = self.creds.lock().unwrap();
        if guard.stale {
            return Err("ChatGPT login expired — re-login this backend via claude-switch".into());
        }
        let creds = guard.creds.clone().ok_or_else(|| {
            String::from("no ChatGPT credentials — log in via claude-switch")
        })?;
        let now = oauth::now_epoch_secs();
        if now.saturating_sub(creds.last_refresh_epoch_secs) > PROACTIVE_REFRESH_AGE_SECS {
            let mut fresh = creds.clone();
            match oauth::refresh(&mut fresh) {
                Ok(()) => {
                    let _ = oauth::save_credentials(&self.config_dir, &self.name, &fresh);
                    guard.creds = Some(fresh.clone());
                    return Ok(fresh);
                }
                Err(RefreshError::Permanent(msg)) => {
                    guard.stale = true;
                    return Err(format!("ChatGPT login expired: {}", msg));
                }
                Err(RefreshError::Transient(_)) => {
                    // Keep the old token and let the request try it anyway.
                }
            }
        }
        Ok(creds)
    }

    /// Refresh once (single-flight via the credentials mutex). Used when an
    /// upstream request comes back 401.
    fn refresh_once(&self) -> Result<OauthCredentials, String> {
        let mut guard = self.creds.lock().unwrap();
        let creds = guard
            .creds
            .clone()
            .ok_or_else(|| "no ChatGPT credentials".to_string())?;
        if guard.stale {
            return Err("ChatGPT login expired".into());
        }
        let mut fresh = creds.clone();
        match oauth::refresh(&mut fresh) {
            Ok(()) => {
                let _ = oauth::save_credentials(&self.config_dir, &self.name, &fresh);
                guard.creds = Some(fresh.clone());
                Ok(fresh)
            }
            Err(RefreshError::Permanent(msg)) => {
                guard.stale = true;
                Err(format!("ChatGPT login expired: {}", msg))
            }
            Err(RefreshError::Transient(msg)) => Err(format!("token refresh failed: {}", msg)),
        }
    }

    fn oauth_status(&self) -> &'static str {
        if self.creds.lock().unwrap().stale {
            "stale"
        } else {
            "ok"
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon entry point
// ---------------------------------------------------------------------------

/// Run the translation proxy until killed. `backend` comes from the
/// `--serve --backend <path>` CLI path; its `.oauth.json` sibling holds the
/// credentials.
pub fn run_daemon(backend: &Backend) -> anyhow::Result<()> {
    let env_path = PathBuf::from(&backend.description);
    let config_dir = env_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(config::app_dir);
    let mut creds = oauth::load_credentials(&config_dir, &backend.name);
    let mut models = ModelConfig::from_backend(&backend.env);
    if let Some(c) = creds.as_mut() {
        // Credentials written before the catalog was captured carry no model
        // list. Backfill it once so Claude Code's picker is complete without
        // forcing a re-login; the result is persisted, so this runs at most
        // once per credential file.
        if c.models.is_empty() {
            let fetched = oauth::fetch_models(&c.access_token, c.account_id.as_deref());
            if fetched.is_empty() {
                eprintln!(
                    "warning: could not fetch the model catalog; \
                     advertising only the configured model ids"
                );
            } else {
                c.models = fetched;
                if let Err(e) = oauth::save_credentials(&config_dir, &backend.name, c) {
                    eprintln!("warning: could not persist model catalog: {}", e);
                }
            }
        }
        // Advertise the whole catalog so Claude Code's picker shows every
        // model the account can actually call.
        models.catalog = c.models.clone();
    }

    let (listener, port, changed) = bind_listener(backend)?;
    if changed {
        // Persist the fallback port so the activator's claude.env agrees.
        if let Err(e) = config::rewrite_env_value(
            &env_path,
            "ANTHROPIC_BASE_URL",
            &format!("http://127.0.0.1:{}", port),
        ) {
            eprintln!("warning: could not persist fallback port: {}", e);
        }
    }
    let _ = daemon::write_pid_file(&config_dir, &backend.name, std::process::id(), port);
    eprintln!("claude-switch proxy: serving '{}' on 127.0.0.1:{}", backend.name, port);

    let state = Arc::new(ProxyState {
        config_dir,
        name: backend.name.clone(),
        creds: Mutex::new(CredsState {
            creds,
            stale: false,
        }),
        models,
        port,
    });

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(&state, stream) {
                        eprintln!("connection error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
    Ok(())
}

fn bind_listener(backend: &Backend) -> anyhow::Result<(TcpListener, u16, bool)> {
    let preferred = backend
        .env
        .get("CS_PROXY_PORT")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(config::DEFAULT_PROXY_PORT);
    for port in preferred..preferred.saturating_add(20) {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => return Ok((l, port, port != preferred)),
            Err(_) => continue,
        }
    }
    anyhow::bail!("no free port in range {}-{}", preferred, preferred + 20)
}

// ---------------------------------------------------------------------------
// Connection dispatch
// ---------------------------------------------------------------------------

fn handle_conn(state: &ProxyState, mut stream: TcpStream) -> anyhow::Result<()> {
    let req = http::read_request(&mut stream, Duration::from_secs(30))?;
    // Set CS_PROXY_DEBUG=1 in the proxy's environment to trace which endpoints
    // Claude Code actually calls (useful when a feature seems not to be used).
    if std::env::var("CS_PROXY_DEBUG").is_ok() {
        eprintln!("{} {}", req.method, req.path);
    }
    match (req.method.as_str(), req.path_no_query()) {
        ("GET", "/") | ("GET", "/health") => {
            http::write_json(
                &mut stream,
                200,
                &json!({"status": "ok", "oauth": state.oauth_status(), "port": state.port}),
            )?;
        }
        ("GET", "/api/hello") => {
            http::write_json(&mut stream, 200, &json!({}))?;
        }
        ("HEAD", "/api/hello") => {
            http::write_response_empty(&mut stream, 200)?;
        }
        ("GET", "/v1/models") | ("GET", "/models") => {
            http::write_json(&mut stream, 200, &models_json(state))?;
        }
        ("POST", "/v1/messages") => handle_messages(state, stream, req)?,
        ("POST", "/v1/messages/count_tokens") => handle_count_tokens(state, stream, req)?,
        _ => {
            http::write_error(&mut stream, 404, "invalid_request_error", "not found")?;
        }
    }
    Ok(())
}

fn models_json(state: &ProxyState) -> Value {
    let mk = |raw: &str, display: &str| {
        json!({
            "id": format!("claude-{}", raw),
            "type": "model",
            "display_name": display,
            "created_at": "2025-01-01T00:00:00Z",
        })
    };
    let data: Vec<Value> = state
        .models
        .advertised()
        .iter()
        .map(|(slug, display)| mk(slug, display))
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": data.first().and_then(|d| d.get("id")).cloned().unwrap_or(Value::Null),
        "last_id": data.last().and_then(|d| d.get("id")).cloned().unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------
// /v1/messages
// ---------------------------------------------------------------------------

fn handle_messages(state: &ProxyState, mut stream: TcpStream, req: http::HttpRequest) -> anyhow::Result<()> {
    let cc_req: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            http::write_error(&mut stream, 400, "invalid_request_error", &format!("invalid JSON: {}", e))?;
            return Ok(());
        }
    };
    let want_stream = cc_req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let input_tokens = estimate_input_tokens(&cc_req);
    let creds = match state.get_creds() {
        Ok(c) => c,
        Err(msg) => {
            http::write_error(&mut stream, 401, "authentication_error", &msg)?;
            return Ok(());
        }
    };
    let upstream = translate_request(&cc_req, &state.models);
    let upstream_model = upstream["model"].as_str().unwrap_or("").to_string();

    match call_upstream(state, &creds, &upstream) {
        Err(e) => {
            http::write_error(&mut stream, e.status, e.error_type, &e.message)?;
        }
        Ok(mut reader) => {
            let mut translator = Translator::new(&upstream_model, input_tokens);
            if want_stream {
                http::write_sse_headers(&mut stream)?;
                let mut out = SseWriter::new(&mut stream);
                pump_stream(&mut reader, Some(&mut out), &mut translator)?;
            } else {
                // Aggregate the upstream stream into one Anthropic JSON message.
                pump_stream::<Vec<u8>>(&mut reader, None, &mut translator)?;
                http::write_json(&mut stream, 200, &translator.final_message())?;
            }
        }
    }
    Ok(())
}

fn handle_count_tokens(state: &ProxyState, mut stream: TcpStream, req: http::HttpRequest) -> anyhow::Result<()> {
    let _ = state; // credentials not needed for a local estimate
    let cc_req: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            http::write_error(&mut stream, 400, "invalid_request_error", &format!("invalid JSON: {}", e))?;
            return Ok(());
        }
    };
    http::write_json(&mut stream, 200, &json!({"input_tokens": estimate_input_tokens(&cc_req)}))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Upstream call
// ---------------------------------------------------------------------------

struct UpstreamError {
    status: u16,
    error_type: &'static str,
    message: String,
}

impl UpstreamError {
    fn auth(message: String) -> Self {
        Self {
            status: 401,
            error_type: "authentication_error",
            message,
        }
    }

    fn network(e: ureq::Transport) -> Self {
        Self {
            status: 502,
            error_type: "api_error",
            message: format!("upstream unreachable: {}", e),
        }
    }

    fn classify(code: u16, body: &str) -> Self {
        let message = extract_upstream_message(body);
        let (status, error_type) = match code {
            400 => (400, "invalid_request_error"),
            401 => (401, "authentication_error"),
            403 => (403, "permission_error"),
            429 => (429, "rate_limit_error"),
            529 => (529, "overloaded_error"),
            _ => (502, "api_error"),
        };
        Self {
            status,
            error_type,
            message,
        }
    }
}

fn extract_upstream_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("error_description"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(UPSTREAM_IDLE_TIMEOUT)
        .build()
}

#[allow(clippy::result_large_err)] // ureq::Error is big; boxing it would clutter every match
fn post_upstream(
    agent: &ureq::Agent,
    creds: &OauthCredentials,
    body: &Value,
) -> Result<ureq::Response, ureq::Error> {
    let mut req = agent
        .post(&oauth::responses_url())
        .set("Authorization", &format!("Bearer {}", creds.access_token))
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if let Some(account) = &creds.account_id {
        req = req.set("ChatGPT-Account-Id", account);
    }
    req.send_json(body)
}

/// Call the upstream with one refresh-and-retry on 401.
fn call_upstream(
    state: &ProxyState,
    creds: &OauthCredentials,
    body: &Value,
) -> Result<Box<dyn Read>, UpstreamError> {
    let agent = build_agent();
    match post_upstream(&agent, creds, body) {
        Ok(r) => Ok(r.into_reader()),
        Err(ureq::Error::Status(401, r)) => {
            let _ = r.into_string();
            match state.refresh_once() {
                Ok(fresh) => match post_upstream(&agent, &fresh, body) {
                    Ok(r) => Ok(r.into_reader()),
                    Err(ureq::Error::Status(code, r)) => {
                        Err(UpstreamError::classify(code, &r.into_string().unwrap_or_default()))
                    }
                    Err(ureq::Error::Transport(e)) => Err(UpstreamError::network(e)),
                },
                Err(msg) => Err(UpstreamError::auth(format!(
                    "{} — re-login this backend via claude-switch",
                    msg
                ))),
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            Err(UpstreamError::classify(code, &r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Transport(e)) => Err(UpstreamError::network(e)),
    }
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic Messages → codex/responses
// ---------------------------------------------------------------------------

/// Translate an Anthropic `/v1/messages` request into the codex/responses
/// body. Anthropic-only fields (cache_control, thinking, strict, …) are
/// dropped — unpaired beta fields would make Claude Code 400 on its side.
pub fn translate_request(req: &Value, cfg: &ModelConfig) -> Value {
    let model = normalize_model(
        req.get("model").and_then(|m| m.as_str()).unwrap_or(""),
        cfg,
    );
    let parallel = !req
        .get("tool_choice")
        .and_then(|t| t.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut out = serde_json::Map::new();
    out.insert("model".into(), json!(model));
    out.insert("store".into(), json!(false));
    out.insert("stream".into(), json!(true));
    out.insert("parallel_tool_calls".into(), json!(parallel));
    out.insert("text".into(), json!({"verbosity": "low"}));

    let sys = system_text(req.get("system"));
    if !sys.is_empty() {
        out.insert("instructions".into(), json!(sys));
    }

    let mut input: Vec<Value> = Vec::new();
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            translate_message(msg, &mut input);
        }
    }
    out.insert("input".into(), Value::Array(input));

    if let Some(tools) = translate_tools(req.get("tools")) {
        out.insert("tools".into(), tools);
    }
    if let Some(tc) = translate_tool_choice(req.get("tool_choice")) {
        out.insert("tool_choice".into(), tc);
    }

    Value::Object(out)
}

fn system_text(system: Option<&Value>) -> String {
    match system {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn translate_message(msg: &Value, input: &mut Vec<Value>) {
    match msg.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
        "system" => {
            // Mid-conversation system reminders become developer messages.
            if let Some(text) = single_text(msg) {
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": text}],
                    }));
                }
            }
        }
        "user" => translate_user_message(msg, input),
        _ => translate_assistant_message(msg, input),
    }
}

fn single_text(msg: &Value) -> Option<String> {
    match msg.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text.join("\n"))
            }
        }
        _ => None,
    }
}

fn translate_user_message(msg: &Value, input: &mut Vec<Value>) {
    let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
        if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
            if !s.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": s}],
                }));
            }
        }
        return;
    };

    let mut parts: Vec<Value> = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(json!({"type": "input_text", "text": text}));
                }
            }
            Some("image") => parts.push(translate_image(block)),
            Some("tool_result") => {
                // Tool results are split out of the surrounding text and must
                // follow the user message that contains them.
                if !parts.is_empty() {
                    input.push(json!({"type": "message", "role": "user", "content": parts}));
                    parts = Vec::new();
                }
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "output": tool_result_output(block),
                }));
            }
            _ => {} // thinking and unknown blocks are dropped
        }
    }
    if !parts.is_empty() {
        input.push(json!({"type": "message", "role": "user", "content": parts}));
    }
}

fn translate_image(block: &Value) -> Value {
    let source = block.get("source").unwrap_or(&Value::Null);
    let media = source
        .get("media_type")
        .and_then(|m| m.as_str())
        .unwrap_or("image/png");
    if let Some(data) = source.get("data").and_then(|d| d.as_str()) {
        json!({"type": "input_image", "image_url": format!("data:{};base64,{}", media, data)})
    } else if let Some(url) = source.get("url").and_then(|u| u.as_str()) {
        json!({"type": "input_text", "text": format!("[image omitted: {}]", url)})
    } else {
        json!({"type": "input_text", "text": "[unsupported content block omitted: image]"})
    }
}

fn tool_result_output(block: &Value) -> Value {
    let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut text = String::new();
    let mut image_parts: Vec<Value> = Vec::new();
    if let Some(parts) = block.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            match p.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("image") => image_parts.push(translate_image(p)),
                _ => {}
            }
        }
    } else if let Some(s) = block.get("content").and_then(|c| c.as_str()) {
        text = s.to_string();
    }
    if is_error && !text.is_empty() {
        text = format!("[tool execution error] {}", text);
    }
    if image_parts.is_empty() {
        json!(text)
    } else {
        if !text.is_empty() {
            image_parts.insert(0, json!({"type": "input_text", "text": text}));
        }
        Value::Array(image_parts)
    }
}

fn translate_assistant_message(msg: &Value, input: &mut Vec<Value>) {
    let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
        if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
            if !s.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": s}],
                }));
            }
        }
        return;
    };

    let mut parts: Vec<Value> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(json!({"type": "output_text", "text": t}));
                }
            }
            Some("tool_use") => {
                if !parts.is_empty() {
                    input.push(json!({"type": "message", "role": "assistant", "content": parts}));
                    parts = Vec::new();
                }
                calls.push(json!({
                    "type": "function_call",
                    "call_id": block.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    "name": block.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "arguments": block.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into()),
                }));
            }
            _ => {} // thinking blocks are dropped
        }
    }
    if !parts.is_empty() {
        input.push(json!({"type": "message", "role": "assistant", "content": parts}));
    }
    for call in calls {
        input.push(call);
    }
}

fn translate_tools(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let mut out: Vec<Value> = Vec::new();
    for t in arr {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name == "web_search_20250305" {
            out.push(json!({
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text", "image"],
            }));
            continue;
        }
        let mut ft = serde_json::Map::new();
        ft.insert("type".into(), json!("function"));
        ft.insert("name".into(), json!(name));
        if let Some(desc) = t.get("description").and_then(|d| d.as_str()) {
            ft.insert("description".into(), json!(desc));
        }
        if let Some(schema) = t.get("input_schema").or_else(|| t.get("parameters")) {
            ft.insert("parameters".into(), schema.clone());
        }
        ft.insert("strict".into(), json!(false));
        out.push(Value::Object(ft));
    }
    Some(Value::Array(out))
}

fn translate_tool_choice(tc: Option<&Value>) -> Option<Value> {
    let tc = tc?;
    if let Some(s) = tc.as_str() {
        return Some(json!(match s {
            "auto" => "auto",
            "none" => "none",
            _ => "required",
        }));
    }
    if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
        if name == "web_search_20250305" {
            return None; // hosted web search — leave choice on auto
        }
        return Some(json!({"type": "function", "name": name}));
    }
    None
}

/// Rough input-token estimate (chars/4). The ChatGPT consumer endpoint does
/// not reliably report usage, so local estimation is the established practice.
pub fn estimate_input_tokens(req: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(system) = req.get("system") {
        chars += count_string_chars(system);
    }
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            chars += count_string_chars(m);
        }
    }
    if let Some(tools) = req.get("tools") {
        chars += count_string_chars(tools);
    }
    (chars / 4).max(1) as u64
}

fn count_string_chars(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(items) => items.iter().map(count_string_chars).sum(),
        Value::Object(map) => map.values().map(count_string_chars).sum(),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Response translation: codex/responses SSE → Anthropic SSE
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    ToolUse,
}

struct Block {
    kind: BlockKind,
    id: String,
    name: String,
    content: String,
    stopped: bool,
}

/// Accumulates a translated response. In streaming mode each transition also
/// emits the corresponding Anthropic SSE event; in aggregate mode
/// (`out == None`) only the state is built for `final_message`.
pub struct Translator {
    msg_id: String,
    model: String,
    input_tokens: u64,
    blocks: Vec<Block>,
    started: bool,
    stopped: bool,
    stop_reason: Option<String>,
    saw_tool_use: bool,
    upstream_output_tokens: Option<u64>,
    output_chars: u64,
}

impl Translator {
    pub fn new(model: &str, input_tokens: u64) -> Self {
        Self {
            msg_id: format!("msg_{}", crate::config::random_hex_nonce()),
            model: model.to_string(),
            input_tokens,
            blocks: Vec::new(),
            started: false,
            stopped: false,
            stop_reason: None,
            saw_tool_use: false,
            upstream_output_tokens: None,
            output_chars: 0,
        }
    }

    fn has_open_block(&self) -> bool {
        self.blocks.iter().any(|b| !b.stopped)
    }

    fn emit_start<W: std::io::Write>(&mut self, w: &mut SseWriter<W>) -> std::io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        w.event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                },
            })
            .to_string(),
        )
    }

    fn open_block<W: std::io::Write>(
        &mut self,
        kind: BlockKind,
        id: String,
        name: String,
        out: Option<&mut SseWriter<W>>,
    ) -> std::io::Result<()> {
        self.blocks.push(Block {
            kind,
            id,
            name,
            content: String::new(),
            stopped: false,
        });
        if let Some(w) = out {
            self.emit_start(w)?;
            let idx = self.blocks.len() - 1;
            let block = &self.blocks[idx];
            let content_block = match block.kind {
                BlockKind::Text => json!({"type": "text", "text": ""}),
                BlockKind::ToolUse => json!({
                    "type": "tool_use",
                    "id": block.id,
                    "name": block.name,
                    "input": {},
                }),
            };
            w.event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": idx, "content_block": content_block}).to_string(),
            )?;
        }
        Ok(())
    }

    fn close_open_block<W: std::io::Write>(
        &mut self,
        out: Option<&mut SseWriter<W>>,
    ) -> std::io::Result<()> {
        let Some(idx) = self.blocks.iter().position(|b| !b.stopped) else {
            return Ok(());
        };
        self.blocks[idx].stopped = true;
        if let Some(w) = out {
            w.event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}).to_string(),
            )?;
        }
        Ok(())
    }

    fn output_tokens(&self) -> u64 {
        self.upstream_output_tokens.unwrap_or_else(|| {
            ((self.output_chars / 4).max(1)) + if self.saw_tool_use { 8 } else { 0 }
        })
    }

    // `out` is move-only (Option<&mut _>), so reborrows via as_deref_mut are
    // intentional here; the lint's suggested move would consume the option.
    #[allow(clippy::needless_option_as_deref)]
    fn finish<W: std::io::Write>(
        &mut self,
        stop_reason: &str,
        upstream_usage: Option<(u64, u64)>,
        mut out: Option<&mut SseWriter<W>>,
    ) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        if let Some((_, output)) = upstream_usage {
            if output > 0 {
                self.upstream_output_tokens = Some(output);
            }
        }
        while self.has_open_block() {
            self.close_open_block(out.as_deref_mut())?;
        }
        let stop = if self.saw_tool_use { "tool_use" } else { stop_reason };
        self.stop_reason = Some(stop.to_string());
        if let Some(w) = out.as_deref_mut() {
            self.emit_start(w)?;
            w.event(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop, "stop_sequence": null},
                    "usage": {"output_tokens": self.output_tokens()},
                })
                .to_string(),
            )?;
            w.event("message_stop", &json!({"type": "message_stop"}).to_string())?;
        }
        self.stopped = true;
        Ok(())
    }

    /// Process one upstream SSE event.
    // `out` is move-only (Option<&mut _>), so reborrows via as_deref_mut are
    // intentional; the lint's suggested move would consume the option.
    #[allow(clippy::needless_option_as_deref)]
    pub fn process<W: std::io::Write>(
        &mut self,
        ev: &str,
        data: &Value,
        mut out: Option<&mut SseWriter<W>>,
    ) -> std::io::Result<()> {
        match ev {
            "response.created" => {
                if let Some(id) = data.get("response").and_then(|r| r.get("id")).and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        self.msg_id = id.to_string();
                    }
                }
            }
            "response.output_item.added" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("call_unknown")
                        .to_string();
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    self.saw_tool_use = true;
                    self.open_block(BlockKind::ToolUse, id, name, out.as_deref_mut())?;
                }
            }
            "response.content_part.added" => {
                // Standard text path: a message item starts producing text.
                if !self.has_open_block() {
                    self.open_block(BlockKind::Text, String::new(), String::new(), out.as_deref_mut())?;
                }
            }
            "response.output_text.delta" | "response.function_call_arguments.delta" => {
                let delta = data.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(());
                }
                let is_json = ev == "response.function_call_arguments.delta";
                if !self.has_open_block() {
                    // Legacy variants stream tool arguments without an
                    // output_item.added first.
                    if is_json {
                        self.saw_tool_use = true;
                        self.open_block(
                            BlockKind::ToolUse,
                            format!("call_{}", crate::config::random_hex_nonce()),
                            String::new(),
                            out.as_deref_mut(),
                        )?;
                    } else {
                        self.open_block(BlockKind::Text, String::new(), String::new(), out.as_deref_mut())?;
                    }
                }
                let idx = self.blocks.len() - 1;
                // Heuristic: deltas landing on a tool block are JSON fragments.
                let as_json = is_json || self.blocks[idx].kind == BlockKind::ToolUse;
                self.blocks[idx].content.push_str(delta);
                self.output_chars += delta.chars().count() as u64;
                if let Some(w) = out.as_deref_mut() {
                    let (delta_type, field) = if as_json {
                        ("input_json_delta", "partial_json")
                    } else {
                        ("text_delta", "text")
                    };
                    w.event(
                        "content_block_delta",
                        &json!({"type": "content_block_delta", "index": idx, "delta": {"type": delta_type, (field): delta}})
                            .to_string(),
                    )?;
                }
            }
            "response.output_item.done" => {
                self.close_open_block(out.as_deref_mut())?;
            }
            "response.completed" => {
                let status = data.get("response").and_then(|r| r.get("status")).and_then(|v| v.as_str());
                if status.is_some() && status != Some("completed") {
                    // A completed event carrying a failed response.
                    let (etype, msg) = error_from_event(data);
                    self.finish("end_turn", None, out.as_deref_mut())?;
                    if let Some(w) = out.as_deref_mut() {
                        w.event("error", &json!({"type": "error", "error": {"type": etype, "message": msg}}).to_string())?;
                    }
                } else {
                    let usage = extract_usage(data);
                    self.finish("end_turn", usage, out.as_deref_mut())?;
                }
            }
            "response.incomplete" => {
                let usage = extract_usage(data);
                let reason = data
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str());
                let sr = if reason == Some("max_output_tokens") {
                    "max_tokens"
                } else {
                    "end_turn"
                };
                self.finish(sr, usage, out.as_deref_mut())?;
            }
            "response.failed" | "response.error" | "error" => {
                let (etype, msg) = error_from_event(data);
                self.finish("end_turn", None, out.as_deref_mut())?;
                if let Some(w) = out.as_deref_mut() {
                    w.event("error", &json!({"type": "error", "error": {"type": etype, "message": msg}}).to_string())?;
                }
            }
            _ => {} // keepalive, rate_limits, unknown events: ignore
        }
        Ok(())
    }

    /// The aggregated message for `stream: false` clients.
    pub fn final_message(&self) -> Value {
        let content: Vec<Value> = self
            .blocks
            .iter()
            .map(|b| match b.kind {
                BlockKind::Text => json!({"type": "text", "text": b.content}),
                BlockKind::ToolUse => json!({
                    "type": "tool_use",
                    "id": b.id,
                    "name": b.name,
                    "input": serde_json::from_str(&b.content).unwrap_or(Value::Null),
                }),
            })
            .collect();
        json!({
            "id": self.msg_id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": content,
            "stop_reason": self.stop_reason.clone().unwrap_or_else(|| "end_turn".into()),
            "stop_sequence": null,
            "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens()},
        })
    }
}

fn extract_usage(data: &Value) -> Option<(u64, u64)> {
    let usage = data.get("response").and_then(|r| r.get("usage"))?;
    match (
        usage.get("input_tokens").and_then(|v| v.as_u64()),
        usage.get("output_tokens").and_then(|v| v.as_u64()),
    ) {
        (Some(i), Some(o)) => Some((i, o)),
        _ => None,
    }
}

fn error_from_event(data: &Value) -> (&'static str, String) {
    let msg = data
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| data.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| {
            data.get("response")
                .and_then(|r| r.get("status"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("upstream error")
        .to_string();
    let error_type = if msg.contains("rate") || msg.contains("limit") {
        "rate_limit_error"
    } else if msg.contains("auth") || msg.contains("credential") || msg.contains("expired") {
        "authentication_error"
    } else if msg.contains("overload") || msg.contains("capacity") {
        "overloaded_error"
    } else {
        "api_error"
    };
    (error_type, msg)
}

/// Pump the upstream SSE stream into the translator. Idle read timeouts on
/// the upstream socket double as the ping cadence for the client.
// `out` is move-only (Option<&mut _>), so reborrows via as_deref_mut are
// intentional; the lint's suggested move would consume the option.
#[allow(clippy::needless_option_as_deref)]
fn pump_stream<W: std::io::Write>(
    reader: &mut dyn Read,
    mut out: Option<&mut SseWriter<W>>,
    translator: &mut Translator,
) -> anyhow::Result<()> {
    let mut buf = BufReader::new(reader);
    let mut event: Option<String> = None;
    let mut data = String::new();
    loop {
        let mut line = String::new();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    if let Some(ev) = event.take() {
                        let parsed = if data.trim().is_empty() {
                            Value::Null
                        } else {
                            serde_json::from_str(&data).unwrap_or(Value::String(data.clone()))
                        };
                        translator.process(&ev, &parsed, out.as_deref_mut())?;
                    }
                    data.clear();
                } else if let Some(rest) = trimmed.strip_prefix("event:") {
                    event = Some(rest.trim().to_string());
                } else if let Some(rest) = trimmed.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                // Upstream silence: keep Claude Code's watchdog happy.
                if let Some(w) = out.as_deref_mut() {
                    w.ping()?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    // The upstream ended without a terminal event — close out gracefully.
    translator.finish("end_turn", None, out.as_deref_mut())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ModelConfig {
        ModelConfig {
            main: "gpt-6-astra".into(),
            small: "gpt-5.5".into(),
            catalog: Vec::new(),
        }
    }

    #[test]
    fn test_normalize_model() {
        let c = cfg();
        assert_eq!(normalize_model("claude-gpt-6-astra", &c), "gpt-6-astra");
        assert_eq!(normalize_model("gpt-6-astra", &c), "gpt-6-astra");
        assert_eq!(normalize_model("claude-sonnet-4-5", &c), "gpt-6-astra");
        assert_eq!(normalize_model("gpt-4.1", &c), "gpt-4.1");
        assert_eq!(normalize_model("claude-", &c), "gpt-6-astra");
    }

    #[test]
    fn test_normalize_model_accepts_catalog_entries() {
        let mut c = cfg();
        c.catalog = vec![
            oauth::ModelInfo {
                slug: "gpt-6-astra".into(),
                display_name: Some("GPT-6-Astra".into()),
                context_window: Some(272_000),
                priority: Some(1),
            },
            oauth::ModelInfo {
                slug: "gpt-5.6-sol".into(),
                display_name: None,
                context_window: None,
                priority: Some(4),
            },
        ];
        // A catalog model that is neither main nor small must pass through
        // aliased, not collapse onto the main model.
        assert_eq!(normalize_model("claude-gpt-5.6-sol", &c), "gpt-5.6-sol");
        assert_eq!(normalize_model("gpt-5.6-sol", &c), "gpt-5.6-sol");
        // Still falls back for genuinely unknown claude-* aliases.
        assert_eq!(normalize_model("claude-opus-4-1", &c), "gpt-6-astra");
    }

    #[test]
    fn test_advertised_models_prefers_catalog() {
        let c = cfg();
        assert_eq!(c.advertised(), vec![("gpt-6-astra", "gpt-6-astra"), ("gpt-5.5", "gpt-5.5")]);

        let mut c = cfg();
        c.catalog = vec![
            oauth::ModelInfo {
                slug: "gpt-6-astra".into(),
                display_name: Some("GPT-6-Astra".into()),
                context_window: None,
                priority: Some(1),
            },
            oauth::ModelInfo {
                slug: "gpt-5.6-sol".into(),
                display_name: None,
                context_window: None,
                priority: Some(4),
            },
        ];
        // Catalog order wins, and a missing display name falls back to the slug.
        assert_eq!(
            c.advertised(),
            vec![("gpt-6-astra", "GPT-6-Astra"), ("gpt-5.6-sol", "gpt-5.6-sol")]
        );
    }

    #[test]
    fn test_translate_request_basic() {
        let req = json!({
            "model": "claude-gpt-6-astra",
            "max_tokens": 4096,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]}
            ],
            "stream": true,
        });
        let out = translate_request(&req, &cfg());
        assert_eq!(out["model"], "gpt-6-astra");
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert_eq!(out["instructions"], "You are helpful.");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert!(out.get("max_output_tokens").is_none());
        assert!(out.get("max_tokens").is_none());
    }

    #[test]
    fn test_translate_request_tools_and_results() {
        let req = json!({
            "model": "gpt-5.2-codex",
            "tools": [
                {"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}},
                {"name": "web_search_20250305", "description": "Search the web"}
            ],
            "tool_choice": {"type": "tool", "name": "Read"},
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking…"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "a.txt"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "hello world", "is_error": false},
                ]},
            ],
        });
        let out = translate_request(&req, &cfg());
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["parameters"]["type"], "object");
        assert_eq!(out["tools"][0]["strict"], false);
        assert_eq!(out["tools"][1]["type"], "web_search");
        assert_eq!(out["tool_choice"], json!({"type": "function", "name": "Read"}));

        let input = out["input"].as_array().unwrap();
        // assistant text part, then the function call
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "toolu_1");
        assert_eq!(input[1]["arguments"], "{\"file_path\":\"a.txt\"}");
        // tool result
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "hello world");
    }

    #[test]
    fn test_translate_request_strips_anthropic_fields() {
        let req = json!({
            "model": "gpt-5.2-codex",
            "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                    {"type": "thinking", "thinking": "secret thoughts"},
                ]}
            ],
        });
        let out = translate_request(&req, &cfg());
        assert_eq!(out["instructions"], "sys");
        assert_eq!(out["input"][0]["content"][0]["text"], "hi");
        assert!(out["input"][0]["content"][0].get("cache_control").is_none());
        // thinking block dropped → only the text part remains
        assert_eq!(out["input"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_translate_tool_choice_modes() {
        assert_eq!(translate_tool_choice(Some(&json!("auto"))).unwrap(), "auto");
        assert_eq!(translate_tool_choice(Some(&json!("none"))).unwrap(), "none");
        assert_eq!(translate_tool_choice(Some(&json!("any"))).unwrap(), "required");
        assert_eq!(translate_tool_choice(Some(&json!("required"))).unwrap(), "required");
        assert!(translate_tool_choice(Some(&json!({"type": "tool", "name": "web_search_20250305"}))).is_none());
    }

    #[test]
    fn test_estimate_input_tokens() {
        let req = json!({
            "system": "be brief",
            "messages": [{"role": "user", "content": "hello hello hello hello"}],
        });
        // (8 + 24) / 4 = 8
        assert_eq!(estimate_input_tokens(&req), 8);
        // empty request → minimum 1
        assert_eq!(estimate_input_tokens(&json!({})), 1);
    }

    fn collect_stream(t: &mut Translator, events: &[(&str, Value)]) -> Vec<(String, Value)> {
        // Run the translator with a Vec-backed SseWriter and re-parse the
        // framed events for assertions.
        let mut out_vec: Vec<u8> = Vec::new();
        {
            let mut w = SseWriter::new(&mut out_vec);
            for (ev, data) in events {
                t.process(ev, data, Some(&mut w)).unwrap();
            }
        }
        parse_sse(&out_vec)
    }

    fn parse_sse(bytes: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8_lossy(bytes);
        let mut out = Vec::new();
        let mut event = String::new();
        let mut data = String::new();
        for line in text.lines() {
            if line.is_empty() {
                if !event.is_empty() {
                    out.push((event.clone(), serde_json::from_str(&data).unwrap_or(Value::Null)));
                    event.clear();
                    data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = rest.to_string();
            }
        }
        out
    }

    #[test]
    fn test_translator_stream_text() {
        let mut t = Translator::new("gpt-5.2-codex", 10);
        let events = [
            ("response.created", json!({"response": {"id": "resp_1"}})),
            ("response.output_item.added", json!({"item": {"type": "message"}})),
            ("response.content_part.added", json!({})),
            ("response.output_text.delta", json!({"delta": "Hel"})),
            ("response.output_text.delta", json!({"delta": "lo"})),
            ("response.output_item.done", json!({})),
            ("response.completed", json!({"response": {"status": "completed", "usage": {"input_tokens": 10, "output_tokens": 5}}})),
        ];
        let out = collect_stream(&mut t, &events);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let start = &out[0].1;
        assert_eq!(start["message"]["id"], "resp_1");
        assert_eq!(start["message"]["usage"]["input_tokens"], 10);
        let delta = &out[2].1;
        assert_eq!(delta["delta"]["type"], "text_delta");
        assert_eq!(delta["delta"]["text"], "Hel");
        let final_delta = &out[5].1;
        assert_eq!(final_delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(final_delta["usage"]["output_tokens"], 5); // upstream usage wins
    }

    #[test]
    fn test_translator_stream_tool_use() {
        let mut t = Translator::new("gpt-5.2-codex", 10);
        let events = [
            ("response.created", json!({})),
            (
                "response.output_item.added",
                json!({"item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}}),
            ),
            ("response.function_call_arguments.delta", json!({"delta": "{\"cmd\":"})),
            ("response.function_call_arguments.delta", json!({"delta": "\"ls\"}"})),
            ("response.output_item.done", json!({})),
            ("response.completed", json!({"response": {"status": "completed"}})),
        ];
        let out = collect_stream(&mut t, &events);
        let start_block = &out[1].1;
        assert_eq!(start_block["content_block"]["type"], "tool_use");
        assert_eq!(start_block["content_block"]["id"], "call_1");
        assert_eq!(start_block["content_block"]["name"], "Bash");
        let delta = &out[2].1;
        assert_eq!(delta["delta"]["type"], "input_json_delta");
        assert_eq!(delta["delta"]["partial_json"], "{\"cmd\":");
        // tool call → stop_reason tool_use
        let final_delta = out
            .iter()
            .find(|(n, _)| n == "message_delta")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(final_delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_translator_usage_fallback_estimate() {
        let mut t = Translator::new("gpt-5.2-codex", 4);
        let events = [
            ("response.content_part.added", json!({})),
            ("response.output_text.delta", json!({"delta": "abcdefgh"})), // 8 chars → 2 tokens
            ("response.completed", json!({"response": {"status": "completed"}})),
        ];
        let out = collect_stream(&mut t, &events);
        let final_delta = out
            .iter()
            .find(|(n, _)| n == "message_delta")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(final_delta["usage"]["output_tokens"], 2);
    }

    #[test]
    fn test_translator_aggregate_json() {
        let mut t = Translator::new("gpt-5.2-codex", 4);
        let events = [
            ("response.output_item.added", json!({"item": {"type": "message"}})),
            ("response.content_part.added", json!({})),
            ("response.output_text.delta", json!({"delta": "abc"})),
            ("response.completed", json!({"response": {"status": "completed"}})),
        ];
        for (ev, data) in &events {
            t.process::<Vec<u8>>(ev, data, None).unwrap();
        }
        let msg = t.final_message();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["content"][0]["text"], "abc");
    }

    #[test]
    fn test_translator_max_tokens_stop_reason() {
        let mut t = Translator::new("gpt-5.2-codex", 4);
        let events = [
            ("response.content_part.added", json!({})),
            ("response.output_text.delta", json!({"delta": "x"})),
            (
                "response.incomplete",
                json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
            ),
        ];
        let out = collect_stream(&mut t, &events);
        let final_delta = out
            .iter()
            .find(|(n, _)| n == "message_delta")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(final_delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn test_error_classify() {
        let e = UpstreamError::classify(429, "{\"error\":{\"message\":\"rate limited\"}}");
        assert_eq!(e.status, 429);
        assert_eq!(e.error_type, "rate_limit_error");
        assert_eq!(e.message, "rate limited");

        let e = UpstreamError::classify(500, "boom");
        assert_eq!(e.status, 502);
        assert_eq!(e.error_type, "api_error");

        let e = UpstreamError::classify(400, "{\"error\":{\"message\":\"bad\"}}");
        assert_eq!(e.status, 400);
        assert_eq!(e.error_type, "invalid_request_error");
    }
}
