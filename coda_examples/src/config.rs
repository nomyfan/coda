use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use coda_agent::ToolApprovalMode;
use coda_core::llm::ToolCall;

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config I/O error: {e}"),
            ConfigError::Parse(msg) => write!(f, "config parse error: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// Pattern-based permission rules for shell commands.
///
/// Evaluation: deny match → require approval, allow match → auto-approve,
/// no match → require approval (default).
#[derive(Clone)]
pub struct ToolApprovalConfig {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    allow: Vec<String>,
    deny: Vec<String>,
    config_path: PathBuf,
}

impl ToolApprovalConfig {
    /// Load from `.coda/config.toml` under the given workspace directory.
    /// Returns a default (empty rules → all shell calls require approval)
    /// if the file does not exist.
    pub fn load(workspace_dir: &Path) -> Result<Self, ConfigError> {
        let config_path = workspace_dir.join(".coda").join("config.toml");
        let (allow, deny) = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            parse_permissions(&content)?
        } else {
            (vec![], vec![])
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                allow,
                deny,
                config_path,
            })),
        })
    }

    /// Build a `ToolApprovalMode` that checks shell commands against these rules.
    ///
    /// The returned closure captures the inner `Arc` so that patterns added via
    /// [`add_allow_pattern`] take effect immediately for subsequent tool calls.
    pub fn into_approval_mode(self) -> ToolApprovalMode {
        ToolApprovalMode::RequireWhen(Arc::new(move |call| self.requires_approval(call)))
    }

    /// Whether `call` should be suspended for human approval.
    pub fn requires_approval(&self, call: &ToolCall) -> bool {
        if call.name != "shell" {
            return false;
        }
        let command = extract_shell_command(call);
        let inner = self.inner.lock().unwrap();

        if inner.deny.iter().any(|p| wildcard_match(p, &command)) {
            return true;
        }
        if inner.allow.iter().any(|p| wildcard_match(p, &command)) {
            return false;
        }
        true
    }

    /// Append a glob pattern to the allow-list, updating both in-memory state
    /// and the config file on disk.
    pub fn add_allow_pattern(&self, pattern: &str) -> Result<(), ConfigError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.allow.iter().any(|p| p == pattern) {
            return Ok(());
        }
        let mut candidate = inner.allow.clone();
        candidate.push(pattern.to_string());
        write_allow_patterns(&inner.config_path, &candidate)?;
        inner.allow = candidate;
        Ok(())
    }

    /// Derive a sensible glob pattern from a concrete command.
    /// Takes the first token and appends ` *`.
    /// E.g. `git status --short` → `git *`.
    pub fn derive_pattern(command: &str) -> String {
        let first_token = command.split_whitespace().next().unwrap_or(command);
        if command.contains(' ') {
            format!("{first_token} *")
        } else {
            first_token.to_string()
        }
    }
}

fn extract_shell_command(call: &ToolCall) -> String {
    let args = call.arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v["command"].as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_permissions(content: &str) -> Result<(Vec<String>, Vec<String>), ConfigError> {
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let shell = match doc.get("permissions").and_then(|p| p.get("shell")) {
        Some(t) => t,
        None => return Ok((vec![], vec![])),
    };

    let allow = shell
        .get("allow")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let deny = shell
        .get("deny")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok((allow, deny))
}

/// Rewrite the `[permissions.shell].allow` array in the config file,
/// preserving all other content.
fn write_allow_patterns(path: &Path, patterns: &[String]) -> Result<(), ConfigError> {
    let content = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let permissions = &mut doc["permissions"];
    if !permissions.is_table_like() {
        *permissions = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let shell = &mut permissions["shell"];
    if !shell.is_table_like() {
        *shell = toml_edit::Item::Table(toml_edit::Table::new());
    }

    let mut arr = toml_edit::Array::new();
    for p in patterns {
        arr.push(p.as_str());
    }
    shell["allow"] = toml_edit::value(arr);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Simple wildcard matching: `*` matches zero or more of any character.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && (p[pi] == t[ti] || p[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if let Some((spi, ref mut sti)) = star {
            *sti += 1;
            pi = spi + 1;
            ti = *sti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_basics() {
        assert!(wildcard_match("git *", "git status"));
        assert!(wildcard_match("git *", "git push --force"));
        assert!(!wildcard_match("git *", "gitk"));
        assert!(!wildcard_match("git *", "git"));
        assert!(wildcard_match("cargo *", "cargo test --release"));
        assert!(wildcard_match("rm -rf *", "rm -rf /"));
        assert!(!wildcard_match("rm -rf *", "rm file.txt"));
    }

    #[test]
    fn wildcard_no_space() {
        assert!(wildcard_match("git*", "git"));
        assert!(wildcard_match("git*", "gitk"));
        assert!(wildcard_match("git*", "git status"));
    }

    #[test]
    fn wildcard_question_mark() {
        assert!(wildcard_match("l?", "ls"));
        assert!(!wildcard_match("l?", "lss"));
    }

    #[test]
    fn wildcard_exact() {
        assert!(wildcard_match("ls", "ls"));
        assert!(!wildcard_match("ls", "lsof"));
    }

    #[test]
    fn parse_empty_config() {
        let (allow, deny) = parse_permissions("").unwrap();
        assert!(allow.is_empty());
        assert!(deny.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[permissions.shell]
allow = ["git *", "cargo *"]
deny = ["rm -rf *"]
"#;
        let (allow, deny) = parse_permissions(toml).unwrap();
        assert_eq!(allow, vec!["git *", "cargo *"]);
        assert_eq!(deny, vec!["rm -rf *"]);
    }

    #[test]
    fn config_load_nonexistent() {
        let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
        assert!(config.requires_approval(&shell_call("ls")));
    }

    #[test]
    fn config_deny_overrides_allow() {
        let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
        {
            let mut inner = config.inner.lock().unwrap();
            inner.allow.push("rm *".to_string());
            inner.deny.push("rm -rf *".to_string());
        }
        assert!(!config.requires_approval(&shell_call("rm file.txt")));
        assert!(config.requires_approval(&shell_call("rm -rf /")));
    }

    #[test]
    fn config_non_shell_tools_skip() {
        let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: None,
        };
        assert!(!config.requires_approval(&call));
    }

    #[test]
    fn derive_pattern_works() {
        assert_eq!(
            ToolApprovalConfig::derive_pattern("git status --short"),
            "git *"
        );
        assert_eq!(ToolApprovalConfig::derive_pattern("ls"), "ls");
        assert_eq!(
            ToolApprovalConfig::derive_pattern("cargo test --release"),
            "cargo *"
        );
    }

    #[test]
    fn add_allow_pattern_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        config.add_allow_pattern("git *").unwrap();
        config.add_allow_pattern("cargo *").unwrap();
        // duplicate should be ignored
        config.add_allow_pattern("git *").unwrap();

        let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
        assert!(!reloaded.requires_approval(&shell_call("git status")));
        assert!(!reloaded.requires_approval(&shell_call("cargo test")));
        assert!(reloaded.requires_approval(&shell_call("rm file")));
    }

    #[test]
    fn add_allow_preserves_deny() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".coda").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[permissions.shell]\ndeny = [\"rm -rf *\"]\n").unwrap();

        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        config.add_allow_pattern("git *").unwrap();

        let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
        assert!(!reloaded.requires_approval(&shell_call("git push")));
        assert!(reloaded.requires_approval(&shell_call("rm -rf /")));
    }

    #[test]
    fn add_allow_not_persisted_on_write_failure() {
        let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
        // config_path points to /nonexistent/.coda/config.toml — write will fail
        let result = config.add_allow_pattern("git *");
        assert!(result.is_err());
        // in-memory state must remain unchanged
        assert!(config.requires_approval(&shell_call("git status")));
    }

    #[test]
    fn write_handles_wrong_shaped_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".coda").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "permissions = \"not a table\"\n").unwrap();

        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        config.add_allow_pattern("git *").unwrap();

        let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
        assert!(!reloaded.requires_approval(&shell_call("git status")));
    }

    #[test]
    fn write_handles_wrong_shaped_shell() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".coda").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[permissions]\nshell = \"not a table\"\n").unwrap();

        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        config.add_allow_pattern("cargo *").unwrap();

        let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
        assert!(!reloaded.requires_approval(&shell_call("cargo test")));
    }

    #[test]
    fn write_preserves_inline_table_deny() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".coda").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[permissions]\nshell = { deny = [\"rm -rf *\"] }\n",
        )
        .unwrap();

        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        config.add_allow_pattern("git *").unwrap();

        let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
        assert!(!reloaded.requires_approval(&shell_call("git push")));
        assert!(reloaded.requires_approval(&shell_call("rm -rf /")));
    }

    fn shell_call(command: &str) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: "shell".into(),
            arguments: Some(format!(r#"{{"command":"{}"}}"#, command)),
        }
    }
}
