// ---------------------------------------------------------------------------
// Proxy daemon lifecycle management.
//
// The TUI spawns a detached `claude-switch --serve --backend <path>` process
// when an OAuth backend is activated and stops it when switching away or
// deleting the backend. PID files (`.serve-{stem}.pid`, holding "pid\nport")
// and a `/health` probe are the handshake.
// ---------------------------------------------------------------------------

use crate::config::Backend;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn pid_file(config_dir: &Path, stem: &str) -> PathBuf {
    config_dir.join(format!(".serve-{}.pid", stem))
}

pub fn log_file(config_dir: &Path, stem: &str) -> PathBuf {
    config_dir.join(format!(".serve-{}.log", stem))
}

/// Read a daemon's pid + port from its PID file, if present.
pub fn read_pid_file(config_dir: &Path, stem: &str) -> Option<(u32, u16)> {
    let content = fs::read_to_string(pid_file(config_dir, stem)).ok()?;
    let mut lines = content.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let port: u16 = lines.next()?.trim().parse().ok()?;
    Some((pid, port))
}

/// Write the PID file after a successful bind.
pub fn write_pid_file(config_dir: &Path, stem: &str, pid: u32, port: u16) -> anyhow::Result<()> {
    fs::create_dir_all(config_dir)?;
    fs::write(pid_file(config_dir, stem), format!("{}\n{}\n", pid, port))?;
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    // `kill -0` checks existence; `ps` confirms it is still our binary
    // (guards against PID reuse).
    let exists = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exists {
        return false;
    }
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("claude-switch"))
        .unwrap_or(false)
}

/// Port of a healthy daemon for `stem`, if one is running.
pub fn healthy_port(config_dir: &Path, stem: &str) -> Option<u16> {
    let (pid, port) = read_pid_file(config_dir, stem)?;
    if !process_alive(pid) {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(1))
        .timeout_read(Duration::from_secs(1))
        .build();
    match agent.get(&format!("http://127.0.0.1:{}/health", port)).call() {
        Ok(r) if r.status() == 200 => Some(port),
        _ => None,
    }
}

/// Ensure the proxy daemon for `backend` is running; return its port.
///
/// The daemon writes its PID file itself after binding, and persists any
/// fallback port back into the backend `.env` — polling `/health` here picks
/// up the real port either way.
pub fn ensure_daemon(config_dir: &Path, backend: &Backend) -> anyhow::Result<u16> {
    let stem = &backend.name;
    if let Some(port) = healthy_port(config_dir, stem) {
        return Ok(port);
    }

    let exe = std::env::current_exe()?;
    let env_path = PathBuf::from(&backend.description);
    let log = fs::File::create(log_file(config_dir, stem))?;
    let mut cmd = Command::new(exe);
    cmd.arg("--serve")
        .arg("--backend")
        .arg(&env_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Detach from the TUI's process group so terminal hangups (SIGHUP on
    // window close) don't take the proxy down with the switcher.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;

    // Wait for the daemon to come up. The budget is generous because a daemon
    // whose credentials predate the model catalog does one bounded upstream
    // fetch before it binds (that only happens on the first start, though).
    for _ in 0..75 {
        if let Some(port) = healthy_port(config_dir, stem) {
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!(
        "proxy daemon did not become healthy; see {}",
        log_file(config_dir, stem).display()
    )
}

/// Stop the daemon for `stem` and remove its PID file.
pub fn kill_daemon(config_dir: &Path, stem: &str) {
    if let Some((pid, _)) = read_pid_file(config_dir, stem) {
        if process_alive(pid) {
            let _ = Command::new("kill")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .status();
            std::thread::sleep(Duration::from_millis(300));
            if process_alive(pid) {
                let _ = Command::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(Stdio::null())
                    .stdout(Stdio::null())
                    .status();
            }
        }
    }
    let _ = fs::remove_file(pid_file(config_dir, stem));
}

/// Stop every proxy daemon except `keep` (pass "" to stop all).
pub fn kill_all_except(config_dir: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(config_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name
            .strip_prefix(".serve-")
            .and_then(|s| s.strip_suffix(".pid"))
        {
            if stem != keep {
                kill_daemon(config_dir, stem);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_roundtrip() {
        let dir = std::env::temp_dir().join("claude-switch-daemon-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_pid_file(&dir, "gpt", 12345, 18765).unwrap();
        let (pid, port) = read_pid_file(&dir, "gpt").unwrap();
        assert_eq!(pid, 12345);
        assert_eq!(port, 18765);
        assert!(pid_file(&dir, "gpt").exists());

        kill_daemon(&dir, "gpt");
        assert!(!pid_file(&dir, "gpt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_kill_all_except() {
        let dir = std::env::temp_dir().join("claude-switch-daemon-test2");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_pid_file(&dir, "a", 1, 100).unwrap();
        write_pid_file(&dir, "b", 2, 200).unwrap();
        write_pid_file(&dir, "c", 3, 300).unwrap();

        kill_all_except(&dir, "b");
        assert!(!pid_file(&dir, "a").exists());
        assert!(pid_file(&dir, "b").exists());
        assert!(!pid_file(&dir, "c").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
