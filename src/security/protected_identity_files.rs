use std::path::Path;

/// elfClaw 2026-09-24: core identity/config files the agent must never write
/// to itself (elfclaw.md §3 point 4). In production the agent rewrote
/// HEARTBEAT.md 14 times and a third-party skill (`self-improving`)
/// explicitly wants write access to AGENTS.md/SOUL.md/HEARTBEAT.md — this
/// list is the canonical OpenClaw identity file set (see
/// `src/onboard/wizard.rs`'s scaffolding list) plus `config.toml`.
///
/// `config.toml` is matched by name because the deployed layout keeps it one
/// level above `workspace/`, and with `workspace_only = false` the resolved-
/// path check only vets the parent directory — a relative `"config.toml"`
/// entry in `forbidden_paths` never matches the real absolute path.
///
/// `HEARTBEAT_DATA.md` (the auxiliary, agent-writable data file) is
/// deliberately NOT on this list.
const PROTECTED_IDENTITY_FILENAMES: &[&str] = &[
    "IDENTITY.md",
    "AGENTS.md",
    "HEARTBEAT.md",
    "SOUL.md",
    "USER.md",
    "TOOLS.md",
    "BOOTSTRAP.md",
    "config.toml",
];

/// Returns true when a path's file name matches a protected identity file,
/// regardless of directory depth (mirrors [`crate::security::sensitive_paths::is_sensitive_file_path`]'s
/// exact-filename matching).
pub fn is_protected_identity_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    PROTECTED_IDENTITY_FILENAMES
        .iter()
        .any(|protected| name.eq_ignore_ascii_case(protected))
}

/// Human-readable rejection message shared by file_write/file_edit/apply_patch.
pub fn protected_identity_file_block_message(path: &str) -> String {
    format!(
        "Writing to '{path}' is blocked: it is a protected core identity/config file \
(see elfclaw.md §3 point 4). If you need to persist data, write it to HEARTBEAT_DATA.md \
or another non-core file instead."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_protected_filenames_case_insensitive() {
        assert!(is_protected_identity_file(Path::new("HEARTBEAT.md")));
        assert!(is_protected_identity_file(Path::new("heartbeat.md")));
        assert!(is_protected_identity_file(Path::new("AGENTS.md")));
        assert!(is_protected_identity_file(Path::new("soul.md")));
    }

    #[test]
    fn detects_config_toml_including_absolute_path_outside_workspace() {
        assert!(is_protected_identity_file(Path::new("config.toml")));
        assert!(is_protected_identity_file(Path::new(
            "C:/dev/elfClaw/ZeroClaw_Workspace/config.toml"
        )));
        assert!(!is_protected_identity_file(Path::new(
            "config.example.toml"
        )));
    }

    #[test]
    fn detects_protected_filenames_in_subdirectories() {
        assert!(is_protected_identity_file(Path::new(
            "workspace/HEARTBEAT.md"
        )));
        assert!(is_protected_identity_file(Path::new(
            "some/nested/dir/AGENTS.md"
        )));
    }

    #[test]
    fn allows_heartbeat_data_and_unrelated_files() {
        assert!(!is_protected_identity_file(Path::new("HEARTBEAT_DATA.md")));
        assert!(!is_protected_identity_file(Path::new("homework/notes.md")));
        assert!(!is_protected_identity_file(Path::new(
            "workers/news_fetcher.md"
        )));
    }
}
