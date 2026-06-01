use serde::Deserialize;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Per-backend reachability state.
#[derive(Debug, Clone)]
pub enum CheckStatus {
    Pending,
    InProgress,
    Reachable {
        models: Vec<String>,
    },
    Unreachable {
        error: String,
    },
    Skipped {
        reason: String,
    },
}

/// Message sent from a check thread back to the main event loop.
#[derive(Debug)]
pub struct CheckResult {
    pub backend_idx: usize,
    pub status: CheckStatus,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

/// Spawn a thread that checks one backend's API reachability and available models.
pub fn spawn_check(
    backend_idx: usize,
    base_url: String,
    api_key: String,
    tx: mpsc::Sender<CheckResult>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = check_single_backend(&base_url, &api_key);
        let _ = tx.send(CheckResult {
            backend_idx,
            status: result,
        });
    })
}

fn check_single_backend(base_url: &str, api_key: &str) -> CheckStatus {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();

    // Step 1: connectivity check — any HTTP response means reachable
    match agent.get(base_url).call() {
        Err(ureq::Error::Transport(e)) => {
            return CheckStatus::Unreachable {
                error: e.to_string(),
            };
        }
        Err(ureq::Error::Status(_, _)) | Ok(_) => {
            // Server responded — reachable. Proceed to model discovery.
        }
    }

    // Step 2: try to discover models
    let models = try_fetch_models(&agent, base_url, api_key);
    CheckStatus::Reachable { models }
}

/// Try multiple patterns to fetch the model list. Returns the first successful
/// result, or an empty vec if all patterns fail.
fn try_fetch_models(agent: &ureq::Agent, base_url: &str, api_key: &str) -> Vec<String> {
    let root_url = root_base_url(base_url);

    // Pattern 1: Anthropic-style — {base_url}/v1/models + x-api-key
    let url1 = format!("{}/v1/models", base_url.trim_end_matches('/'));
    if let Some(models) = fetch_with_header(agent, &url1, "x-api-key", api_key) {
        return models;
    }

    // Pattern 2: OpenAI-style — {root}/models + Authorization: Bearer
    let url2 = format!("{}/models", root_url);
    if let Some(models) = fetch_with_header(agent, &url2, "Authorization", &format!("Bearer {}", api_key)) {
        return models;
    }

    // Pattern 3: OpenAI-style v1 — {root}/v1/models + Authorization: Bearer
    let url3 = format!("{}/v1/models", root_url);
    if let Some(models) = fetch_with_header(agent, &url3, "Authorization", &format!("Bearer {}", api_key)) {
        return models;
    }

    vec![]
}

fn fetch_with_header(
    agent: &ureq::Agent,
    url: &str,
    header_name: &str,
    header_value: &str,
) -> Option<Vec<String>> {
    let resp = agent
        .get(url)
        .set(header_name, header_value)
        .set("Content-Type", "application/json")
        .call();

    match resp {
        Ok(response) if response.status() == 200 => {
            response.into_json::<ModelsResponse>().ok().map(|body| {
                body.data.into_iter().map(|m| m.id).collect()
            })
        }
        _ => None,
    }
}

/// Strip path suffixes like `/anthropic`, `/api` etc. to get the root API host.
/// `https://api.deepseek.com/anthropic` → `https://api.deepseek.com`
fn root_base_url(url: &str) -> &str {
    // Find the start of the path (after `https://host`)
    if let Some(rest) = url.strip_prefix("https://") {
        if let Some(slash) = rest.find('/') {
            return &url[.."https://".len() + slash];
        }
    }
    if let Some(rest) = url.strip_prefix("http://") {
        if let Some(slash) = rest.find('/') {
            return &url[.."http://".len() + slash];
        }
    }
    url
}
