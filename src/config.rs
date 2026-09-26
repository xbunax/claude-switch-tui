use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// XDG-compliant application directory
// ---------------------------------------------------------------------------

/// Application directory: `$XDG_CONFIG_HOME/claude-switch/`,
/// falling back to `~/.config/claude-switch/`.
pub fn app_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("claude-switch");
        }
    }
    dirs::home_dir()
        .expect("Cannot determine HOME directory")
        .join(".config")
        .join("claude-switch")
}

/// Default env output file: `<app_dir>/claude.env`
pub fn default_env_path() -> PathBuf {
    app_dir().join("claude.env")
}

/// Expand `~` at the start of a path to the user's home directory.
pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(path)
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Backend {
    pub name: String,
    pub description: String,
    pub env: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Discover backends — scans *.env files AND falls back to backends.json
// ---------------------------------------------------------------------------

/// Discover all available backends by scanning `<config_dir>/` for `*.env` files.
pub fn discover_backends(config_dir: &Path) -> anyhow::Result<Vec<Backend>> {
    let mut backends: Vec<Backend> = Vec::new();

    if config_dir.exists() {
        if let Ok(entries) = fs::read_dir(config_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Skip the output file (claude.env)
                if path.file_name().is_some_and(|n| n == "claude.env") {
                    continue;
                }
                if path.extension().is_some_and(|ext| ext == "env") {
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    match parse_dotenv_file(&path) {
                        Ok(env) => {
                            backends.push(Backend {
                                name,
                                description: path.display().to_string(),
                                env,
                            });
                        }
                        Err(e) => {
                            eprintln!("Warning: skipping {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }
    }

    if backends.is_empty() {
        let exe_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "claude-switch".into());
        anyhow::bail!(
            "No .env files found in {}.\n\
             Place *.env files in that directory, or run: {} --init",
            config_dir.display(),
            exe_name
        );
    }

    backends.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(backends)
}

// ---------------------------------------------------------------------------
// Parse a .env file into a HashMap
// ---------------------------------------------------------------------------

fn parse_dotenv_file(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)?;
    let mut env = HashMap::new();

    for (lineno, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        // Skip blank lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Strip `export ` prefix if present (shell env file format)
        let line = trimmed.strip_prefix("export ").unwrap_or(trimmed);

        // Split on first `=`
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("{}:{}: invalid format", path.display(), lineno + 1))?;

        let key = key.trim().to_string();
        let mut value = value.trim().to_string();

        // Strip surrounding quotes if present
        if value.len() >= 2 {
            let first = value.chars().next().unwrap();
            let last = value.chars().last().unwrap();
            if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
                value = value[1..value.len() - 1].to_string();
            }
        }

        if !key.is_empty() {
            env.insert(key, value);
        }
    }

    if env.is_empty() {
        anyhow::bail!("{}: no valid environment variables found", path.display());
    }

    Ok(env)
}

// ---------------------------------------------------------------------------
// Init — creates example .env files
// ---------------------------------------------------------------------------

/// Create example configuration in the config directory.
pub fn init_config(config_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(config_dir)?;

    let example_env = config_dir.join("anthropic.example.env");
    let example_env2 = config_dir.join("custom-gateway.example.env");

    if example_env.exists() {
        anyhow::bail!(
            "Config directory already has files: {}\nClear them manually or use them directly",
            config_dir.display()
        );
    }

    fs::write(
        &example_env,
        "# Anthropic Official API\n\
         ANTHROPIC_BASE_URL=https://api.anthropic.com\n\
         ANTHROPIC_API_KEY=sk-ant-xxx\n",
    )?;

    fs::write(
        &example_env2,
        "# Custom Gateway\n\
         ANTHROPIC_BASE_URL=https://example.com/anthropic\n\
         ANTHROPIC_API_KEY=your-token\n",
    )?;

    println!("Example configuration created:");
    println!("  {}  (remove .example to enable)", example_env.display());
    println!("  {}  (remove .example to enable)", example_env2.display());
    println!("\nUsage:");
    println!("  1. Copy .example.env → my-backend.env and fill in real credentials");
    println!("  2. Run claude-switch to select a backend");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shell-quote a value for use in `export KEY=VALUE` lines.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Write the selected backend's environment variables to a shell file.
///
/// Before writing the new exports, reads the existing env file (if any) and
/// emits `unset` statements for its keys so stale variables don't leak across
/// backend switches.
pub fn write_env_file(path: &Path, backend: &Backend) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Collect keys from the current env file so we can unset them first
    let stale_keys = if path.exists() {
        match fs::read_to_string(path) {
            Ok(content) => parse_exported_keys(&content),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut lines = vec![
        "# Generated by claude-switch".to_string(),
        format!("# Backend: {}", backend.name),
        String::new(),
    ];

    // Unset previously-exported variables to avoid cross-backend pollution
    if !stale_keys.is_empty() {
        let sorted_stale: std::collections::BTreeSet<&str> =
            stale_keys.iter().map(|s| s.as_str()).collect();
        lines.push(format!("unset {}", sorted_stale.into_iter().collect::<Vec<_>>().join(" ")));
        lines.push(String::new());
    }

    let mut keys: Vec<&String> = backend.env.keys().collect();
    keys.sort();

    for key in keys {
        let value = &backend.env[key];
        lines.push(format!("export {}={}", key, shell_quote(value)));
    }
    lines.push(String::new());

    fs::write(path, lines.join("\n"))?;
    Ok(())
}

/// Extract variable names from `export KEY=...` lines in a shell env file.
fn parse_exported_keys(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            trimmed
                .strip_prefix("export ")
                .and_then(|rest| rest.split_once('=').map(|(k, _)| k.to_string()))
        })
        .collect()
}

/// Write a new backend `.env` file directly from form fields.
pub fn save_backend_env(
    config_dir: &Path,
    name: &str,
    base_url: &str,
    api_key: &str,
    description: &str,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(config_dir)?;

    let filename = format!("{}.env", name);
    let path = config_dir.join(&filename);

    let content = format!(
        "# {}\nANTHROPIC_BASE_URL={}\nANTHROPIC_API_KEY={}\n",
        description, base_url, api_key
    );

    fs::write(&path, content)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// ChatGPT (Codex OAuth) backends
// ---------------------------------------------------------------------------

/// Marker value for the `CS_BACKEND_KIND` key identifying an OAuth backend.
pub const CS_BACKEND_KIND_OAUTH: &str = "chatgpt-oauth";

/// Default port of the built-in Anthropic↔ChatGPT translation proxy.
pub const DEFAULT_PROXY_PORT: u16 = 18765;

/// Fallback model ids used when the ChatGPT catalog lookup at login fails.
/// Model names change often — these live here so they're easy to bump.
/// Fallbacks used only when the login-time catalog fetch fails; the real ids
/// are captured from the account's catalog at login and written into the
/// backend's `.env`. Upstream renames models regularly.
pub const DEFAULT_MAIN_MODEL: &str = "gpt-6-astra";
pub const DEFAULT_SMALL_MODEL: &str = "gpt-5.5";

/// True when the backend is a ChatGPT OAuth backend (uses the local proxy).
pub fn is_oauth_backend(backend: &Backend) -> bool {
    backend
        .env
        .get("CS_BACKEND_KIND")
        .map(|v| v == CS_BACKEND_KIND_OAUTH)
        .unwrap_or(false)
}

/// Path of the sibling credential file for an OAuth backend.
pub fn oauth_json_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join(format!("{}.oauth.json", name))
}

/// Clone a backend with all internal `CS_*` keys removed — these must never
/// reach `claude.env` or the shell.
pub fn strip_cs_keys(backend: &Backend) -> Backend {
    let env = backend
        .env
        .iter()
        .filter(|(k, _)| !k.starts_with("CS_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Backend {
        name: backend.name.clone(),
        description: backend.description.clone(),
        env,
    }
}

/// Load a single backend `.env` file by path (used by `--serve`).
pub fn load_backend_file(path: &Path) -> anyhow::Result<Backend> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let env = parse_dotenv_file(path)?;
    Ok(Backend {
        name,
        description: path.display().to_string(),
        env,
    })
}

/// Generate a 64-char random hex nonce for `ANTHROPIC_AUTH_TOKEN`. Its value
/// is ignored by the proxy — presence is what matters (it outranks any
/// stored `/login` credentials in Claude Code).
pub fn random_hex_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Write a ChatGPT OAuth backend `.env` file. The credential tokens live in
/// the sibling `{name}.oauth.json`, never in the env file.
pub fn save_oauth_backend_env(
    config_dir: &Path,
    name: &str,
    description: &str,
    main_model: &str,
    mid_model: &str,
    small_model: &str,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(config_dir)?;
    let path = config_dir.join(format!("{}.env", name));
    let content = format!(
        "# {}\n\
         CS_BACKEND_KIND={}\n\
         ANTHROPIC_BASE_URL=http://127.0.0.1:{}\n\
         ANTHROPIC_AUTH_TOKEN={}\n\
         ANTHROPIC_MODEL={}\n\
         {}\
         CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1\n\
         CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1\n\
         CLAUDE_CODE_ATTRIBUTION_HEADER=0\n\
         CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000\n",
        description,
        CS_BACKEND_KIND_OAUTH,
        DEFAULT_PROXY_PORT,
        random_hex_nonce(),
        main_model,
        extra_key_lines(main_model, mid_model, small_model),
    );
    fs::write(&path, content)?;
    Ok(path)
}

/// Keys an OAuth backend needs on top of the base env, with the value to use
/// when the key is absent. Existing keys are never rewritten, so hand-tuned
/// values survive.
///
/// The role keys retarget Claude Code's built-in Opus/Sonnet/Haiku rows at
/// GPT models — without them those rows would send `claude-opus-5` &co. to the
/// proxy, which can only fall back to the main model. They do *not* add rows.
/// The rows themselves come from gateway model discovery, which is off by
/// default and is what `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` turns on:
/// Claude Code then reads our `GET /v1/models` and lists every entry whose id
/// contains `claude` or `anthropic` (the proxy already aliases them as
/// `claude-…` for exactly this reason).
fn oauth_extra_keys(main: &str, mid: &str, small: &str) -> Vec<(&'static str, String)> {
    vec![
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", main.to_string()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", mid.to_string()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", small.to_string()),
        ("ANTHROPIC_SMALL_FAST_MODEL", small.to_string()),
        ("CLAUDE_CODE_SUBAGENT_MODEL", mid.to_string()),
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1".to_string()),
    ]
}

fn extra_key_lines(main: &str, mid: &str, small: &str) -> String {
    oauth_extra_keys(main, mid, small)
        .iter()
        .map(|(k, v)| format!("{}={}\n", k, v))
        .collect()
}

/// Does the file already define `key`? Handles `export ` prefixes and spacing.
fn has_key(content: &str, key: &str) -> bool {
    content.lines().any(|line| {
        let t = line.trim();
        let t = t.strip_prefix("export ").unwrap_or(t).trim_start();
        t.starts_with(key) && t[key.len()..].trim_start().starts_with('=')
    })
}

/// Backfill keys an OAuth backend's `.env` is missing, leaving everything else
/// — including values the user edited — untouched. Returns whether it wrote.
///
/// Backends created before these keys existed only carry `ANTHROPIC_MODEL`, so
/// their picker offers a single model until this runs.
pub fn ensure_model_roles(path: &Path, main: &str, mid: &str, small: &str) -> anyhow::Result<bool> {
    let content = fs::read_to_string(path)?;
    let missing = extra_key_lines(main, mid, small)
        .lines()
        .filter(|l| {
            let key = l.split('=').next().unwrap_or("");
            !has_key(&content, key)
        })
        .map(String::from)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(false);
    }
    let block: String = missing.iter().map(|l| format!("{}\n", l)).collect();

    // Insert right after ANTHROPIC_MODEL to keep the file readable.
    let mut out = String::new();
    let mut inserted = false;
    for line in content.lines() {
        out.push_str(line);
        out.push('\n');
        if !inserted && line.trim_start().starts_with("ANTHROPIC_MODEL") {
            out.push_str(&block);
            inserted = true;
        }
    }
    if !inserted {
        // No ANTHROPIC_MODEL line (hand-written file): append instead.
        out.push_str(&block);
    }
    fs::write(path, out)?;
    Ok(true)
}

/// Update one key in a `.env` file in place, preserving comments, `export`
/// prefixes on other lines, and ordering.
pub fn rewrite_env_value(path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let content = fs::read_to_string(path)?;
    let mut out = String::new();
    let mut replaced = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let bare = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((k, _)) = bare.split_once('=') {
            if k.trim() == key {
                out.push_str(&format!("{}={}\n", key, value));
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        out.push_str(&format!("{}={}\n", key, value));
    }
    fs::write(path, out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_expand_path_tilde() {
        let result = expand_path("~/test/path");
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("test/path"));
    }

    #[test]
    fn test_expand_path_no_tilde() {
        let result = expand_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_parse_dotenv_file() {
        let tmp = std::env::temp_dir().join("claude-switch-test.env");
        fs::write(
            &tmp,
            "# comment\nKEY_A=value_a\nKEY_B=value b\nKEY_C='quoted'\n",
        )
        .unwrap();

        let env = parse_dotenv_file(&tmp).unwrap();
        assert_eq!(env.get("KEY_A").unwrap(), "value_a");
        assert_eq!(env.get("KEY_B").unwrap(), "value b");
        assert_eq!(env.get("KEY_C").unwrap(), "quoted");

        fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_parse_dotenv_skips_comments_and_blanks() {
        let tmp = std::env::temp_dir().join("claude-switch-test2.env");
        fs::write(&tmp, "\n\n# header\nKEY=val\n\n# footer\n").unwrap();
        let env = parse_dotenv_file(&tmp).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("KEY").unwrap(), "val");
        fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_discover_backends_from_env_files() {
        let dir = std::env::temp_dir().join("claude-switch-test-dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("backend-a.env"),
            "KEY_A=val_a\nKEY_B=val_b\n",
        )
        .unwrap();
        fs::write(
            dir.join("backend-b.env"),
            "KEY_C=val_c\n",
        )
        .unwrap();

        let backends = discover_backends(&dir).unwrap();
        assert_eq!(backends.len(), 2);

        // Sorted by name
        assert_eq!(backends[0].name, "backend-a");
        assert_eq!(backends[1].name, "backend-b");
        assert_eq!(backends[0].env.get("KEY_A").unwrap(), "val_a");
        assert_eq!(backends[1].env.get("KEY_C").unwrap(), "val_c");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_backends_empty_dir_fails() {
        let dir = std::env::temp_dir().join("claude-switch-test-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let result = discover_backends(&dir);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_exported_keys() {
        let content = "# comment\nexport KEY_A='val_a'\nexport KEY_B='val_b'\n";
        let keys = parse_exported_keys(content);
        assert_eq!(keys, vec!["KEY_A", "KEY_B"]);
    }

    #[test]
    fn test_parse_exported_keys_skips_comments_and_blanks() {
        let content = "\n# header\nexport FOO=bar\n\n# footer\n";
        let keys = parse_exported_keys(content);
        assert_eq!(keys, vec!["FOO"]);
    }

    #[test]
    fn test_write_env_file() {
        let tmp = std::env::temp_dir().join("claude-switch-test-env.sh");
        let backend = Backend {
            name: "Test".into(),
            description: "desc".into(),
            env: HashMap::from([
                ("KEY_A".into(), "value_a".into()),
                ("KEY_B".into(), "value b".into()),
            ]),
        };

        write_env_file(&tmp, &backend).unwrap();
        let content = fs::read_to_string(&tmp).unwrap();

        assert!(content.contains("export KEY_A='value_a'"));
        assert!(content.contains("export KEY_B='value b'"));
        assert!(content.contains("# Generated by claude-switch"));
        assert!(content.contains("# Backend: Test"));
        // First write: no stale keys, so no unset line expected
        assert!(!content.contains("unset"));

        fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_write_env_file_unset_stale_keys() {
        let tmp = std::env::temp_dir().join("claude-switch-test-unset.sh");

        // Simulate an existing env file from a previous backend
        fs::write(
            &tmp,
            "# Generated by claude-switch\n# Backend: old\n\nexport STALE_KEY='old_val'\nexport SHARED_KEY='also_old'\n",
        )
        .unwrap();

        let backend = Backend {
            name: "New".into(),
            description: "desc".into(),
            env: HashMap::from([
                ("SHARED_KEY".into(), "fresh".into()),
                ("NEW_KEY".into(), "new_val".into()),
            ]),
        };

        write_env_file(&tmp, &backend).unwrap();
        let content = fs::read_to_string(&tmp).unwrap();

        // Should unset the old keys first
        assert!(content.contains("unset"), "expected unset line, got:\n{}", content);
        assert!(content.contains("STALE_KEY"), "unset should mention STALE_KEY");
        assert!(content.contains("SHARED_KEY"), "unset should mention SHARED_KEY");
        // Then export the new values
        assert!(content.contains("export NEW_KEY='new_val'"));
        assert!(content.contains("export SHARED_KEY='fresh'"));

        fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_init_config() {
        let dir = std::env::temp_dir().join("claude-switch-test-init");
        let _ = fs::remove_dir_all(&dir);

        init_config(&dir).unwrap();

        assert!(dir.join("anthropic.example.env").exists());
        assert!(dir.join("custom-gateway.example.env").exists());

        // Running again should fail
        let result = init_config(&dir);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_backend_env() {
        let dir = std::env::temp_dir().join("claude-switch-test-save");
        let _ = fs::remove_dir_all(&dir);

        let path = save_backend_env(&dir, "my-api", "https://api.example.com", "sk-key", "Test API")
            .unwrap();

        assert_eq!(path, dir.join("my-api.env"));

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Test API"));
        assert!(content.contains("ANTHROPIC_BASE_URL=https://api.example.com"));
        assert!(content.contains("ANTHROPIC_API_KEY=sk-key"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_oauth_backend_env_roundtrip() {
        let dir = std::env::temp_dir().join("claude-switch-test-oauth-env");
        let _ = fs::remove_dir_all(&dir);

        let path = save_oauth_backend_env(
            &dir,
            "gpt",
            "ChatGPT via OAuth",
            "gpt-main",
            "gpt-mid",
            "gpt-small",
        )
        .unwrap();
        assert_eq!(path, dir.join("gpt.env"));

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("CS_BACKEND_KIND=chatgpt-oauth"));
        assert!(content.contains("ANTHROPIC_AUTH_TOKEN="));
        assert!(content.contains("ANTHROPIC_MODEL=gpt-main"));
        // The role keys are what make more than one model selectable in
        // Claude Code's /model picker.
        assert!(content.contains("ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-main"));
        assert!(content.contains("ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-mid"));
        assert!(content.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-small"));
        assert!(content.contains("ANTHROPIC_SMALL_FAST_MODEL=gpt-small"));
        assert!(content.contains("CLAUDE_CODE_SUBAGENT_MODEL=gpt-mid"));
        assert!(content.contains("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1"));

        // Round-trips through discovery and is recognized as an OAuth backend
        let backends = discover_backends(&dir).unwrap();
        assert_eq!(backends.len(), 1);
        assert!(is_oauth_backend(&backends[0]));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ensure_model_roles_upgrades_legacy_env() {
        let dir = std::env::temp_dir().join("claude-switch-test-roles-migrate");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A backend as written before the role keys existed.
        let path = dir.join("gpt.env");
        fs::write(
            &path,
            "# ChatGPT\nCS_BACKEND_KIND=chatgpt-oauth\nANTHROPIC_BASE_URL=http://127.0.0.1:18765\n\
             ANTHROPIC_MODEL=gpt-6-astra\nANTHROPIC_SMALL_FAST_MODEL=gpt-5.5\n",
        )
        .unwrap();

        assert!(ensure_model_roles(&path, "gpt-6-astra", "gpt-6-sol", "gpt-5.5").unwrap());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-6-astra"));
        assert!(content.contains("ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-6-sol"));
        assert!(content.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.5"));
        // Discovery is what actually adds picker rows; without it the roles
        // above only retarget the built-in rows.
        assert!(content.contains("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1"));
        // The pre-existing SMALL_FAST line is kept as-is, not duplicated.
        assert_eq!(content.matches("ANTHROPIC_SMALL_FAST_MODEL").count(), 1);
        assert!(content.contains("ANTHROPIC_SMALL_FAST_MODEL=gpt-5.5"));
        assert_eq!(content.matches("ANTHROPIC_MODEL=").count(), 1);
        // Inserted as a block right after ANTHROPIC_MODEL, nothing else moved.
        let model_line = content.find("ANTHROPIC_MODEL=gpt-6-astra").unwrap();
        let opus_line = content.find("ANTHROPIC_DEFAULT_OPUS_MODEL").unwrap();
        let base_line = content.find("ANTHROPIC_BASE_URL").unwrap();
        assert!(base_line < model_line && model_line < opus_line);

        // Idempotent: a second call leaves the file alone.
        let before = fs::read_to_string(&path).unwrap();
        assert!(!ensure_model_roles(&path, "x", "y", "z").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Backfill must not clobber a value the user edited by hand.
    #[test]
    fn test_ensure_model_roles_preserves_existing_values() {
        let dir = std::env::temp_dir().join("claude-switch-test-roles-preserve");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gpt.env");
        fs::write(
            &path,
            "ANTHROPIC_MODEL=gpt-6-astra\nANTHROPIC_SMALL_FAST_MODEL=hand-picked\n",
        )
        .unwrap();

        assert!(ensure_model_roles(&path, "gpt-6-astra", "gpt-6-sol", "gpt-5.5").unwrap());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("ANTHROPIC_SMALL_FAST_MODEL=hand-picked"));
        assert!(!content.contains("ANTHROPIC_SMALL_FAST_MODEL=gpt-5.5"));
        // ...while the genuinely missing keys still get filled in.
        assert!(content.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.5"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ensure_model_roles_appends_when_no_model_line() {
        let dir = std::env::temp_dir().join("claude-switch-test-roles-append");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("odd.env");
        fs::write(&path, "ANTHROPIC_BASE_URL=http://127.0.0.1:18765\n").unwrap();

        assert!(ensure_model_roles(&path, "a", "b", "c").unwrap());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("ANTHROPIC_DEFAULT_OPUS_MODEL=a"));
        assert!(content.contains("CLAUDE_CODE_SUBAGENT_MODEL=b"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_strip_cs_keys() {
        let backend = Backend {
            name: "gpt".into(),
            description: "".into(),
            env: HashMap::from([
                ("CS_BACKEND_KIND".into(), "chatgpt-oauth".into()),
                ("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:1".into()),
            ]),
        };
        let stripped = strip_cs_keys(&backend);
        assert!(!stripped.env.contains_key("CS_BACKEND_KIND"));
        assert!(stripped.env.contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn test_rewrite_env_value() {
        let dir = std::env::temp_dir().join("claude-switch-test-rewrite");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.env");
        fs::write(
            &path,
            "# comment\nANTHROPIC_BASE_URL=http://127.0.0.1:18765\nANTHROPIC_AUTH_TOKEN=x\n",
        )
        .unwrap();

        rewrite_env_value(&path, "ANTHROPIC_BASE_URL", "http://127.0.0.1:18799").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# comment"));
        assert!(content.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:18799"));
        assert!(!content.contains("18765"));
        assert!(content.contains("ANTHROPIC_AUTH_TOKEN=x"));

        // Missing key gets appended
        rewrite_env_value(&path, "CS_PROXY_PORT", "19000").unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("CS_PROXY_PORT=19000"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_random_hex_nonce() {
        let a = random_hex_nonce();
        let b = random_hex_nonce();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_load_backend_file() {
        let dir = std::env::temp_dir().join("claude-switch-test-load");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loaded.env");
        fs::write(&path, "KEY_A=val_a\n").unwrap();

        let backend = load_backend_file(&path).unwrap();
        assert_eq!(backend.name, "loaded");
        assert_eq!(backend.env.get("KEY_A").unwrap(), "val_a");

        let _ = fs::remove_dir_all(&dir);
    }
}
