use std::path::Path;

/// elfClaw 2026-09-24: core config files the agent must never write to itself
/// (elfclaw.md §3 point 4). In production the agent rewrote HEARTBEAT.md 14
/// times; a broken write to any of these breaks scheduling, the system prompt
/// or startup.
///
/// SOUL.md / USER.md / IDENTITY.md / TOOLS.md are deliberately NOT here: they
/// are designed for the agent to update (preferences, speaking style,
/// identity, local notes), and a bad edit to them cannot make elfClaw fail.
/// Only lock what can break a run.
///
/// `config.toml` is matched by name because the deployed layout keeps it one
/// level above `workspace/`, and with `workspace_only = false` the resolved-
/// path check only vets the parent directory — a relative `"config.toml"`
/// entry in `forbidden_paths` never matches the real absolute path.
///
/// `HEARTBEAT_DATA.toml` is agent-*modifiable* but only through the
/// `news_schedule`/`news_report` tools, which validate and write it in code;
/// a free-form `file_write` could leave it unparseable.
const PROTECTED_IDENTITY_FILENAMES: &[&str] = &[
    "AGENTS.md",
    "HEARTBEAT.md",
    "BOOTSTRAP.md",
    "config.toml",
    "HEARTBEAT_DATA.toml",
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
(see elfclaw.md §3 point 4). News slots/sources are changed with the news_schedule tool; \
other data can go to MEMORY.md or another non-core file."
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
        assert!(is_protected_identity_file(Path::new("bootstrap.md")));
    }

    #[test]
    fn personality_and_notes_files_stay_writable() {
        for name in ["SOUL.md", "USER.md", "IDENTITY.md", "TOOLS.md", "soul.md"] {
            assert!(!is_protected_identity_file(Path::new(name)), "{name}");
        }
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
    fn allows_non_core_files_but_not_the_news_data_file() {
        assert!(is_protected_identity_file(Path::new("HEARTBEAT_DATA.toml")));
        assert!(!is_protected_identity_file(Path::new("MEMORY.md")));
        assert!(!is_protected_identity_file(Path::new("homework/notes.md")));
        assert!(!is_protected_identity_file(Path::new(
            "workers/news_fetcher.md"
        )));
    }
}
