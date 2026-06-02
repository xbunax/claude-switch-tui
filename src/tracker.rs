use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SwitchRecord {
    timestamp: u64,
    backend: String,
    session_id: Option<String>,
    cwd: String,
}

#[derive(Debug)]
pub struct BackendModelStats {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub total: u64,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct BackendStats {
    pub backend: String,
    pub models: Vec<BackendModelStats>,
    pub total_input: u64,
    pub total_output: u64,
    pub total_cache_read: u64,
    pub total_cache_creation: u64,
    pub grand_total: u64,
}

// ---------------------------------------------------------------------------
// Record a backend switch
// ---------------------------------------------------------------------------

pub fn record_switch(config_dir: &Path, backend_name: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let session_id = find_active_session(&cwd);

    let record = SwitchRecord {
        timestamp: now,
        backend: backend_name.to_string(),
        session_id,
        cwd,
    };

    let history_path = config_dir.join(".switch-history.jsonl");
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
    {
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&record).unwrap_or_default()
        );
    }
}

// ---------------------------------------------------------------------------
// Scan all sessions and aggregate token usage by backend
// ---------------------------------------------------------------------------

pub fn scan_usage(config_dir: &Path) -> Vec<BackendStats> {
    let switches = read_switch_history(config_dir);
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let projects_dir = home.join(".claude").join("projects");

    // Build session_id -> backend map from switch history
    let mut session_backend: HashMap<String, String> = HashMap::new();
    for s in &switches {
        if let Some(ref sid) = s.session_id {
            session_backend.insert(sid.clone(), s.backend.clone());
        }
    }

    // model -> (input, output, cache_read, cache_creation)
    type ModelTokens = HashMap<String, (u64, u64, u64, u64)>;
    let mut backend_tokens: HashMap<String, ModelTokens> = HashMap::new();
    let mut unknown: ModelTokens = HashMap::new();

    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }

            if let Ok(sessions) = fs::read_dir(&project_dir) {
                for session in sessions.flatten() {
                    let path = session.path();
                    if path.extension().is_some_and(|e| e == "jsonl") {
                        let session_id = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();

                        let backend = session_backend.get(&session_id);

                        let tokens = parse_session_jsonl(&path);
                        for (model, (input, output, cr, cc)) in tokens {
                            if let Some(b) = backend {
                                backend_tokens
                                    .entry(b.clone())
                                    .or_default()
                                    .entry(model)
                                    .and_modify(|e| {
                                        e.0 += input;
                                        e.1 += output;
                                        e.2 += cr;
                                        e.3 += cc;
                                    })
                                    .or_insert((input, output, cr, cc));
                            } else {
                                unknown
                                    .entry(model)
                                    .and_modify(|e| {
                                        e.0 += input;
                                        e.1 += output;
                                        e.2 += cr;
                                        e.3 += cc;
                                    })
                                    .or_insert((input, output, cr, cc));
                            }
                        }
                    }
                }
            }
        }
    }

    let mut results: Vec<BackendStats> = backend_tokens
        .into_iter()
        .map(|(backend, models)| build_stats(backend, models))
        .collect();

    results.sort_by(|a, b| b.grand_total.cmp(&a.grand_total));

    if !unknown.is_empty() {
        results.push(build_stats("(unattributed)".into(), unknown));
    }

    results
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn find_active_session(cwd: &str) -> Option<String> {
    let sessions_dir = dirs::home_dir()?.join(".claude").join("sessions");
    let mut candidates: Vec<(u64, String)> = Vec::new();

    let entries = fs::read_dir(&sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let content = fs::read_to_string(&path).ok()?;
            let v: serde_json::Value = serde_json::from_str(&content).ok()?;
            if v.get("cwd").and_then(|c| c.as_str()) == Some(cwd) {
                let started = v.get("startedAt").and_then(|s| s.as_u64()).unwrap_or(0);
                let sid = v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !sid.is_empty() {
                    candidates.push((started, sid));
                }
            }
        }
    }

    candidates.sort_by_key(|(t, _)| *t);
    candidates.pop().map(|(_, sid)| sid)
}

fn read_switch_history(config_dir: &Path) -> Vec<SwitchRecord> {
    let path = config_dir.join(".switch-history.jsonl");
    let mut records = Vec::new();

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return records,
    };

    for line in content.lines() {
        if let Ok(record) = serde_json::from_str::<SwitchRecord>(line) {
            records.push(record);
        }
    }

    records
}

fn parse_session_jsonl(path: &Path) -> HashMap<String, (u64, u64, u64, u64)> {
    let mut model_tokens: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return model_tokens,
    };

    for line in content.lines() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }

        let usage = match entry.get("message").and_then(|m| m.get("usage")) {
            Some(u) => u,
            None => continue,
        };

        let model = entry
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        let input = usage.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let output = usage.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let cache_create = usage
            .get("cache_creation_input_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);

        model_tokens
            .entry(model)
            .and_modify(|e| {
                e.0 += input;
                e.1 += output;
                e.2 += cache_read;
                e.3 += cache_create;
            })
            .or_insert((input, output, cache_read, cache_create));
    }

    model_tokens
}

fn build_stats(backend: String, models: HashMap<String, (u64, u64, u64, u64)>) -> BackendStats {
    let mut model_stats: Vec<BackendModelStats> = models
        .into_iter()
        .map(|(model, (input, output, cr, cc))| BackendModelStats {
            model,
            input_tokens: input,
            output_tokens: output,
            cache_read: cr,
            cache_creation: cc,
            total: input + output,
        })
        .filter(|m| m.total > 0)
        .collect();
    model_stats.sort_by(|a, b| b.total.cmp(&a.total));

    let total_input = model_stats.iter().map(|m| m.input_tokens).sum();
    let total_output = model_stats.iter().map(|m| m.output_tokens).sum();
    let total_cr = model_stats.iter().map(|m| m.cache_read).sum();
    let total_cc = model_stats.iter().map(|m| m.cache_creation).sum();

    BackendStats {
        backend,
        total_input,
        total_output,
        total_cache_read: total_cr,
        total_cache_creation: total_cc,
        grand_total: total_input + total_output,
        models: model_stats,
    }
}

pub(crate) fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
