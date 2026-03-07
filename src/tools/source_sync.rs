// elfClaw: source_sync tool — clone/pull git repositories into workspace for
// source-level debug analysis.
//
// Supports two sync strategies:
// 1. Git (preferred): clone/pull via git binary
// 2. HTTP ZIP fallback: download GitHub ZIP archive when git is unavailable
//
// Restricted to a hardcoded URL allowlist and workspace-sandboxed target dirs.
// Agent uses this alongside check_logs + file_read + content_search for debug.

use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Maximum time for a single git operation (seconds).
const GIT_TIMEOUT_SECS: u64 = 120;

/// Maximum time for HTTP download (seconds).
const HTTP_TIMEOUT_SECS: u64 = 180;

/// Hardcoded allowlist — only elfClaw and its upstream are permitted.
const ALLOWED_REPOS: &[(&str, &str)] = &[
    ("elfclaw", "https://github.com/VK7KSM/eflClaw.git"),
    ("zeroclaw", "https://github.com/zeroclaw-labs/zeroclaw.git"),
];

/// HTTP ZIP download allowlist — repo_id → (zip_url, commits_api_url).
const ALLOWED_REPOS_HTTP: &[(&str, &str, &str)] = &[
    (
        "elfclaw",
        "https://github.com/VK7KSM/eflClaw/archive/refs/heads/main.zip",
        "https://api.github.com/repos/VK7KSM/eflClaw/commits/main",
    ),
    (
        "zeroclaw",
        "https://github.com/zeroclaw-labs/zeroclaw/archive/refs/heads/main.zip",
        "https://api.github.com/repos/zeroclaw-labs/zeroclaw/commits/main",
    ),
];

/// Subdirectory under workspace where source repos are cloned.
const SOURCE_DIR: &str = "workspace/github";

pub struct SourceSyncTool {
    security: Arc<SecurityPolicy>,
}

impl SourceSyncTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }

    /// Resolve repo_id to (local_path, remote_url). Rejects unknown ids.
    fn resolve_repo(&self, repo_id: &str) -> anyhow::Result<(PathBuf, &'static str)> {
        let normalized = repo_id.trim().to_lowercase();
        let (name, url) = ALLOWED_REPOS
            .iter()
            .find(|(id, _)| *id == normalized)
            .ok_or_else(|| {
                let valid: Vec<&str> = ALLOWED_REPOS.iter().map(|(id, _)| *id).collect();
                anyhow::anyhow!(
                    "Unknown repo_id '{}'. Allowed: {}",
                    repo_id,
                    valid.join(", ")
                )
            })?;
        let local_path = self.security.workspace_dir.join(SOURCE_DIR).join(name);
        Ok((local_path, url))
    }

    /// Check if git is available on this system. Cached via `OnceLock`.
    fn git_available() -> bool {
        static GIT_OK: OnceLock<bool> = OnceLock::new();
        *GIT_OK.get_or_init(|| {
            // Try bare "git" first
            if std::process::Command::new("git")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return true;
            }
            // Try common Windows paths
            let candidates = [
                r"C:\Program Files\Git\bin\git.exe",
                r"C:\Program Files (x86)\Git\bin\git.exe",
                r"C:\Program Files\Git\cmd\git.exe",
            ];
            for path in &candidates {
                if std::path::Path::new(path).exists() {
                    return true;
                }
            }
            false
        })
    }

    /// Resolve the git binary path. Tries bare "git" first (works when git is
    /// in PATH), then probes common Windows installation directories. The result
    /// is cached via `OnceLock` so the probe runs at most once per process.
    fn find_git() -> &'static str {
        static GIT_PATH: OnceLock<String> = OnceLock::new();
        GIT_PATH.get_or_init(|| {
            // 1. Bare name — works if git is in PATH
            if std::process::Command::new("git")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return "git".to_string();
            }
            // 2. Common Windows locations
            let mut candidates: Vec<String> = vec![
                r"C:\Program Files\Git\bin\git.exe".into(),
                r"C:\Program Files (x86)\Git\bin\git.exe".into(),
                r"C:\Program Files\Git\cmd\git.exe".into(),
            ];
            if let Ok(pf) = std::env::var("PROGRAMFILES") {
                candidates.push(format!(r"{pf}\Git\bin\git.exe"));
                candidates.push(format!(r"{pf}\Git\cmd\git.exe"));
            }
            for path in &candidates {
                if std::path::Path::new(path).exists() {
                    return path.clone();
                }
            }
            // Give up — will fail at call site with a clear error
            "git".to_string()
        })
    }

    /// Run a git command with timeout. Returns (success, stdout, stderr).
    async fn run_git(
        args: &[&str],
        cwd: &Path,
    ) -> anyhow::Result<(bool, String, String)> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(GIT_TIMEOUT_SECS),
            tokio::process::Command::new(Self::find_git())
                .args(args)
                .current_dir(cwd)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("git timed out after {}s", GIT_TIMEOUT_SECS))?
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;

        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }

    /// Fetch latest commit SHA from GitHub API (best-effort, for display only).
    async fn fetch_latest_commit(api_url: &str) -> Option<String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("elfClaw-source-sync")
            .build()
            .ok()?;
        let resp = client.get(api_url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        json.get("sha")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(7).collect())
    }

    /// Sync a repo via HTTP ZIP download (fallback when git is unavailable).
    async fn sync_via_http(&self, repo_id: &str) -> anyhow::Result<String> {
        let normalized = repo_id.trim().to_lowercase();
        let (_, zip_url, api_url) = ALLOWED_REPOS_HTTP
            .iter()
            .find(|(id, _, _)| *id == normalized)
            .ok_or_else(|| anyhow::anyhow!("No HTTP fallback for repo '{repo_id}'"))?;

        let local_path = self.security.workspace_dir.join(SOURCE_DIR).join(&normalized);

        // Fetch commit SHA (best-effort, for display)
        let commit_sha = Self::fetch_latest_commit(api_url).await;

        // Download ZIP
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
            .user_agent("elfClaw-source-sync")
            .build()
            .map_err(|e| anyhow::anyhow!("HTTP client error: {e}"))?;

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(HTTP_TIMEOUT_SECS),
            client.get(*zip_url).send(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("HTTP download timed out after {HTTP_TIMEOUT_SECS}s"))?
        .map_err(|e| anyhow::anyhow!("HTTP download failed: {e}"))?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "HTTP download failed: status {} for {}",
                resp.status(),
                zip_url
            );
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

        // Remove old directory if exists, then extract
        if local_path.exists() {
            tokio::fs::remove_dir_all(&local_path).await.ok();
        }
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Extract ZIP in blocking context (zip crate is sync)
        let local_path_clone = local_path.clone();
        let bytes_vec = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            extract_zip(&bytes_vec, &local_path_clone)
        })
        .await
        .map_err(|e| anyhow::anyhow!("ZIP extraction task failed: {e}"))??;

        let commit_info = commit_sha
            .as_deref()
            .unwrap_or("unknown");

        Ok(format!(
            "Synced {repo_id} via HTTP ZIP\nLatest: {commit_info}\nPath: {}",
            local_path.display()
        ))
    }

    /// Clone or update a repository. Tries git first, falls back to HTTP.
    async fn sync_repo(&self, repo_id: &str) -> anyhow::Result<String> {
        let (local_path, url) = self.resolve_repo(repo_id)?;

        if Self::git_available() {
            // ── Git path ──
            if local_path.join(".git").exists() {
                // Already cloned — fetch latest and reset to remote HEAD
                let (ok, _stdout, stderr) =
                    Self::run_git(&["fetch", "--depth=1", "origin"], &local_path).await?;
                if !ok {
                    anyhow::bail!("git fetch failed: {}", stderr.trim());
                }

                let (ok2, branch_out, _) =
                    Self::run_git(&["rev-parse", "--abbrev-ref", "origin/HEAD"], &local_path)
                        .await?;
                let target = if ok2 {
                    branch_out.trim().to_string()
                } else {
                    "origin/main".to_string()
                };

                let (ok3, _stdout3, stderr3) =
                    Self::run_git(&["reset", "--hard", &target], &local_path).await?;
                if !ok3 {
                    anyhow::bail!("git reset failed: {}", stderr3.trim());
                }

                let (_, log_out, _) =
                    Self::run_git(&["log", "-1", "--pretty=format:%h %s (%ai)"], &local_path)
                        .await?;

                Ok(format!(
                    "Updated {repo_id} → {target}\nLatest: {}\nPath: {}",
                    log_out.trim(),
                    local_path.display()
                ))
            } else {
                // First clone — shallow for speed
                if let Some(parent) = local_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let target_str = local_path.to_string_lossy().to_string();
                let (ok, _stdout, stderr) = Self::run_git(
                    &["clone", "--depth=1", url, &target_str],
                    &self.security.workspace_dir,
                )
                .await?;
                if !ok {
                    anyhow::bail!("git clone failed: {}", stderr.trim());
                }

                let (_, log_out, _) =
                    Self::run_git(&["log", "-1", "--pretty=format:%h %s (%ai)"], &local_path)
                        .await?;

                Ok(format!(
                    "Cloned {repo_id} from {url}\nLatest: {}\nPath: {}",
                    log_out.trim(),
                    local_path.display()
                ))
            }
        } else {
            // ── HTTP fallback ──
            tracing::info!("git not available, falling back to HTTP ZIP for {repo_id}");
            self.sync_via_http(repo_id).await
        }
    }

    /// Check status of a repo without modifying it.
    async fn repo_status(&self, repo_id: &str) -> anyhow::Result<String> {
        let (local_path, url) = self.resolve_repo(repo_id)?;

        // Check both .git (git-cloned) and Cargo.toml (HTTP-synced) markers
        let has_git = local_path.join(".git").exists();
        let has_cargo = local_path.join("Cargo.toml").exists();

        if !has_git && !has_cargo {
            return Ok(format!(
                "{repo_id}: not synced yet\nRemote: {url}\nExpected path: {}",
                local_path.display()
            ));
        }

        if has_git && Self::git_available() {
            let (_, log_out, _) =
                Self::run_git(&["log", "-1", "--pretty=format:%h %s (%ai)"], &local_path).await?;
            let (_, branch_out, _) =
                Self::run_git(&["rev-parse", "--abbrev-ref", "HEAD"], &local_path).await?;
            Ok(format!(
                "{repo_id}: cloned (git)\nBranch: {}\nLatest: {}\nPath: {}",
                branch_out.trim(),
                log_out.trim(),
                local_path.display()
            ))
        } else {
            // HTTP-synced or git unavailable
            Ok(format!(
                "{repo_id}: synced (HTTP ZIP)\nPath: {}",
                local_path.display()
            ))
        }
    }
}

/// Extract a GitHub ZIP archive into target_dir.
/// GitHub ZIPs contain a single top-level directory (e.g. `eflClaw-main/`);
/// this function strips that prefix so files land directly in target_dir.
fn extract_zip(zip_bytes: &[u8], target_dir: &Path) -> anyhow::Result<()> {
    use std::io::Read;

    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| anyhow::anyhow!("Invalid ZIP archive: {e}"))?;

    // Detect the top-level prefix to strip (e.g. "eflClaw-main/")
    let prefix = {
        let first = archive
            .by_index(0)
            .map_err(|e| anyhow::anyhow!("Empty ZIP archive: {e}"))?;
        let name = first.name().to_string();
        // GitHub ZIP always has "repo-branch/" as first entry
        if let Some(slash_pos) = name.find('/') {
            name[..=slash_pos].to_string()
        } else {
            String::new()
        }
    };

    std::fs::create_dir_all(target_dir)?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| anyhow::anyhow!("ZIP entry error: {e}"))?;

        let raw_name = file.name().to_string();
        // Strip the top-level prefix
        let relative = if !prefix.is_empty() && raw_name.starts_with(&prefix) {
            &raw_name[prefix.len()..]
        } else {
            &raw_name
        };

        if relative.is_empty() {
            continue;
        }

        let out_path = target_dir.join(relative);

        // Security: reject paths that escape target_dir
        if !out_path.starts_with(target_dir) {
            continue;
        }

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = std::fs::File::create(&out_path)?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| anyhow::anyhow!("Failed to read ZIP entry: {e}"))?;
            std::io::Write::write_all(&mut out_file, &buf)?;
        }
    }

    Ok(())
}

#[async_trait]
impl Tool for SourceSyncTool {
    fn name(&self) -> &str {
        "source_sync"
    }

    fn description(&self) -> &str {
        "Sync (clone or update) elfClaw or zeroclaw source code repositories \
         into workspace/github/ for source-level debug analysis. \
         Supports git clone/pull (preferred) with HTTP ZIP fallback when git is unavailable. \
         Always sync before analyzing to ensure latest code. \
         After sync, use file_read + content_search + glob_search to inspect source."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "repo_id": {
                    "type": "string",
                    "enum": ["elfclaw", "zeroclaw"],
                    "description": "Repository to sync: 'elfclaw' (our fork) or 'zeroclaw' (upstream)"
                },
                "action": {
                    "type": "string",
                    "enum": ["sync", "status"],
                    "description": "Action: 'sync' = clone or update to latest, 'status' = check state without modifying. Default: sync",
                    "default": "sync"
                }
            },
            "required": ["repo_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Blocked: autonomy is read-only".into()),
            });
        }

        let repo_id = args
            .get("repo_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter 'repo_id'"))?;

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("sync");

        match action {
            "status" => match self.repo_status(repo_id).await {
                Ok(msg) => Ok(ToolResult {
                    success: true,
                    output: msg,
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("status check failed: {e}")),
                }),
            },
            "sync" => {
                if !self.security.record_action() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Blocked: action rate limit exceeded".into()),
                    });
                }
                match self.sync_repo(repo_id).await {
                    Ok(msg) => Ok(ToolResult {
                        success: true,
                        output: msg,
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("source_sync failed: {e}")),
                    }),
                }
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use 'sync' or 'status'."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn test_policy(level: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn tool_name() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::Full));
        assert_eq!(tool.name(), "source_sync");
    }

    #[test]
    fn schema_requires_repo_id() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::Full));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("repo_id")));
    }

    #[test]
    fn resolve_repo_allowlist() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::Full));
        assert!(tool.resolve_repo("elfclaw").is_ok());
        assert!(tool.resolve_repo("zeroclaw").is_ok());
        assert!(tool.resolve_repo("malicious").is_err());
        assert!(tool.resolve_repo("").is_err());
    }

    #[test]
    fn resolve_repo_case_insensitive() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::Full));
        assert!(tool.resolve_repo("ElfClaw").is_ok());
        assert!(tool.resolve_repo("ZEROCLAW").is_ok());
        assert!(tool.resolve_repo("  elfclaw  ").is_ok());
    }

    #[tokio::test]
    async fn blocks_readonly() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::ReadOnly));
        let result = tool.execute(json!({"repo_id": "elfclaw"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let tool = SourceSyncTool::new(test_policy(AutonomyLevel::Full));
        let result = tool
            .execute(json!({"repo_id": "elfclaw", "action": "delete"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[test]
    fn extract_zip_strips_prefix() {
        // Create a minimal ZIP with a prefix directory
        let dir = std::env::temp_dir().join("elfclaw_zip_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options = zip::write::SimpleFileOptions::default();
            writer.add_directory("repo-main/", options).unwrap();
            writer.start_file("repo-main/Cargo.toml", options).unwrap();
            std::io::Write::write_all(&mut writer, b"[package]\nname = \"test\"\n").unwrap();
            writer.start_file("repo-main/src/main.rs", options).unwrap();
            std::io::Write::write_all(&mut writer, b"fn main() {}\n").unwrap();
            writer.finish().unwrap();
        }

        extract_zip(&buf, &dir).unwrap();

        assert!(dir.join("Cargo.toml").exists());
        assert!(dir.join("src/main.rs").exists());
        // prefix dir should NOT exist at top level
        assert!(!dir.join("repo-main").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
