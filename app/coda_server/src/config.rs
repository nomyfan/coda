use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use coda_agent::ToolApprovalMode;
use coda_core::llm::{ReasoningEffort, ToolCall};
use coda_openai::ProviderKind;

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

/// A model configured under a provider. `id` is the API model name sent in
/// requests; `name` is an optional human-readable label for the dashboard (falls
/// back to `id` when absent). `reasoning_efforts` declares which effort levels
/// the model accepts; an empty list means the model is not a reasoning model,
/// so the UI shows no reasoning controls for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    pub reasoning_efforts: Vec<ReasoningEffort>,
}

/// A configured LLM provider with one or more models. `api_key`, `base_url`,
/// `kind`, and `include_usage` are shared across all models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    pub api_key: String,
    pub base_url: String,
    pub include_usage: bool,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub providers: Vec<ProviderConfig>,
    pub workspaces: Vec<WorkspaceConfig>,
}

pub fn load_server_config(path: &Path) -> Result<ServerConfig, ConfigError> {
    let content = std::fs::read_to_string(path)?;
    parse_server_config(&content, path.parent().unwrap_or_else(|| Path::new(".")))
}

fn parse_server_config(content: &str, base_dir: &Path) -> Result<ServerConfig, ConfigError> {
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let providers = parse_providers(&doc)?;
    let workspaces = parse_workspaces(&doc, base_dir)?;

    Ok(ServerConfig {
        providers,
        workspaces,
    })
}

fn parse_providers(doc: &toml_edit::DocumentMut) -> Result<Vec<ProviderConfig>, ConfigError> {
    let providers = doc
        .get("providers")
        .and_then(|item| item.as_array_of_tables())
        .ok_or_else(|| ConfigError::Parse("missing [[providers]] table".to_string()))?;

    let mut seen = std::collections::HashSet::new();
    let mut parsed = Vec::new();
    for provider in providers {
        let id = require_str(provider, "id", "provider")?;
        if !seen.insert(id.clone()) {
            return Err(ConfigError::Parse(format!("duplicate provider id '{id}'")));
        }
        let kind = match provider.get("kind").and_then(|v| v.as_str()) {
            None | Some("generic") => ProviderKind::Generic,
            Some("deepseek") => ProviderKind::Deepseek,
            Some(other) => {
                return Err(ConfigError::Parse(format!(
                    "provider '{id}' has unknown kind '{other}' (expected 'generic' or 'deepseek')"
                )));
            }
        };
        let api_key = expand_env(&require_str(provider, "api_key", "provider")?)?;
        let base_url = expand_env(&require_str(provider, "base_url", "provider")?)?;
        let include_usage = provider
            .get("include_usage")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let models = parse_models(provider, &id)?;

        parsed.push(ProviderConfig {
            id,
            kind,
            api_key,
            base_url,
            include_usage,
            models,
        });
    }

    if parsed.is_empty() {
        return Err(ConfigError::Parse(
            "server config must define at least one provider".to_string(),
        ));
    }

    Ok(parsed)
}

/// Parse the per-provider `models` inline array. Each model requires an `id`
/// (the API model name) and optionally a `name` (display label, defaults to
/// `id`). Model ids must be unique within a provider.
fn parse_models(
    provider: &toml_edit::Table,
    provider_id: &str,
) -> Result<Vec<ModelConfig>, ConfigError> {
    let Some(array) = provider.get("models") else {
        return Err(ConfigError::Parse(format!(
            "provider '{provider_id}' must have a 'models' array"
        )));
    };
    let array = array.as_array().ok_or_else(|| {
        ConfigError::Parse(format!(
            "provider '{provider_id}' 'models' must be an array of tables"
        ))
    })?;

    if array.is_empty() {
        return Err(ConfigError::Parse(format!(
            "provider '{provider_id}' must define at least one model"
        )));
    }

    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for (index, item) in array.iter().enumerate() {
        let table = item.as_inline_table().ok_or_else(|| {
            ConfigError::Parse(format!(
                "provider '{provider_id}' model at index {index} must be an inline table"
            ))
        })?;
        let id = table
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ConfigError::Parse(format!(
                    "provider '{provider_id}' model at index {index} id must be a string"
                ))
            })?;
        if !seen.insert(id.clone()) {
            return Err(ConfigError::Parse(format!(
                "provider '{provider_id}' has duplicate model id '{id}'"
            )));
        }
        // `name` is optional: when absent, the dashboard shows `id`.
        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());
        let reasoning_efforts = parse_model_reasoning_efforts(table, provider_id, &id)?;
        models.push(ModelConfig {
            id,
            name,
            reasoning_efforts,
        });
    }

    Ok(models)
}

fn parse_model_reasoning_efforts(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
) -> Result<Vec<ReasoningEffort>, ConfigError> {
    let Some(array) = model.get("reasoning_efforts") else {
        return Ok(Vec::new());
    };
    let array = array.as_array().ok_or_else(|| {
        ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' reasoning_efforts must be an array"
        ))
    })?;
    array
        .iter()
        .map(|value| {
            let raw = value.as_str().ok_or_else(|| {
                ConfigError::Parse(format!(
                    "provider '{provider_id}' model '{model_name}' reasoning_efforts must be strings"
                ))
            })?;
            match raw {
                "minimal" => Ok(ReasoningEffort::Minimal),
                "low" => Ok(ReasoningEffort::Low),
                "medium" => Ok(ReasoningEffort::Medium),
                "high" => Ok(ReasoningEffort::High),
                "xhigh" => Ok(ReasoningEffort::Xhigh),
                // `none` is the thinking-off state, not an offered level.
                other => Err(ConfigError::Parse(format!(
                    "provider '{provider_id}' model '{model_name}' has unknown reasoning effort '{other}' (expected 'minimal', 'low', 'medium', 'high', or 'xhigh')"
                ))),
            }
        })
        .collect()
}

fn parse_workspaces(
    doc: &toml_edit::DocumentMut,
    base_dir: &Path,
) -> Result<Vec<WorkspaceConfig>, ConfigError> {
    let workspaces = doc
        .get("workspaces")
        .and_then(|item| item.as_array_of_tables())
        .ok_or_else(|| ConfigError::Parse("missing [[workspaces]] table".to_string()))?;

    let mut seen = std::collections::HashSet::new();
    let mut parsed = Vec::new();
    for workspace in workspaces {
        let id = require_str(workspace, "id", "workspace")?;
        if !is_workspace_id(&id) {
            return Err(ConfigError::Parse(format!(
                "workspace id '{id}' may only contain letters, digits, '.', '_', and '-'"
            )));
        }
        if !seen.insert(id.clone()) {
            return Err(ConfigError::Parse(format!("duplicate workspace id '{id}'")));
        }

        let raw_path = require_str(workspace, "path", "workspace")?;
        let path = resolve_workspace_path(base_dir, &raw_path);
        parsed.push(WorkspaceConfig { id, path });
    }

    if parsed.is_empty() {
        return Err(ConfigError::Parse(
            "server config must define at least one workspace".to_string(),
        ));
    }

    Ok(parsed)
}

/// Read a required string field, producing a `{kind} '{field}' must be a string`
/// style error when it is missing or not a string.
fn require_str(table: &toml_edit::Table, field: &str, kind: &str) -> Result<String, ConfigError> {
    table
        .get(field)
        .and_then(|value| value.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ConfigError::Parse(format!("{kind} {field} must be a string")))
}

/// Expand a single leading `${VAR}` reference from the environment so secrets
/// (API keys) can stay out of the config file. A value without the `${...}`
/// wrapper is returned unchanged.
fn expand_env(value: &str) -> Result<String, ConfigError> {
    let Some(var) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) else {
        return Ok(value.to_string());
    };
    std::env::var(var)
        .map_err(|_| ConfigError::Parse(format!("environment variable '{var}' is not set")))
}

fn is_workspace_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn resolve_workspace_path(base_dir: &Path, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

/// Pattern-based permission rules for shell commands.
///
/// Evaluation order: deny match → require approval, shell operators
/// (`;`, `&`, `|`, `>`, `<`, backticks, `$()`, newlines) → require approval,
/// allow match → auto-approve, no match → require approval (default).
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
    /// Create a default config (empty rules → all shell calls require approval)
    /// that writes to `.coda/config.toml` under the given workspace directory.
    pub fn default_for(workspace_dir: &Path) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                allow: vec![],
                deny: vec![],
                config_path: workspace_dir.join(".coda").join("config.toml"),
            })),
        }
    }

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
        ToolApprovalMode::RequireWhen(Arc::new(move |call| {
            // `ask_user` has no real execution — it must always suspend so the
            // caller can interactively answer and resolve it.
            call.name == "ask_user" || self.requires_approval(call)
        }))
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
        if has_shell_operators(&command) {
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
        if command.contains(|c: char| c.is_whitespace()) {
            format!("{first_token} *")
        } else {
            first_token.to_string()
        }
    }
}

fn has_shell_operators(command: &str) -> bool {
    command.contains(';')
        || command.contains('&')
        || command.contains('|')
        || command.contains('>')
        || command.contains('<')
        || command.contains('`')
        || command.contains("$(")
        || command.contains('\n')
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
        } else if pi < p.len()
            && (p[pi] == t[ti]
                || p[pi] == b'?'
                || (p[pi].is_ascii_whitespace() && t[ti].is_ascii_whitespace()))
        {
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
    use std::ffi::OsString;

    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: this test owns its unique environment variable name.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: this guard restores the unique variable it owns.
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

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

    const PROVIDERS: &str = r#"
[[providers]]
id = "deepseek"
kind = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", name = "DeepSeek R1", reasoning_efforts = ["low", "medium", "high"] },
]
"#;

    #[test]
    fn parse_server_config_resolves_workspaces() {
        let config = parse_server_config(
            &format!(
                r#"{PROVIDERS}
[[workspaces]]
id = "coda"
path = "projects/coda"

[[workspaces]]
id = "scratch"
path = "/tmp/scratch"
"#
            ),
            Path::new("/srv"),
        )
        .unwrap();

        assert_eq!(
            config.workspaces,
            vec![
                WorkspaceConfig {
                    id: "coda".to_string(),
                    path: PathBuf::from("/srv/projects/coda"),
                },
                WorkspaceConfig {
                    id: "scratch".to_string(),
                    path: PathBuf::from("/tmp/scratch"),
                },
            ]
        );
        assert_eq!(
            config.providers,
            vec![ProviderConfig {
                id: "deepseek".to_string(),
                kind: ProviderKind::Deepseek,
                api_key: "sk-test".to_string(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                include_usage: true,
                models: vec![ModelConfig {
                    id: "deepseek-reasoner".to_string(),
                    name: "DeepSeek R1".to_string(),
                    reasoning_efforts: vec![
                        ReasoningEffort::Low,
                        ReasoningEffort::Medium,
                        ReasoningEffort::High,
                    ],
                }],
            }]
        );
    }

    #[test]
    fn parse_server_config_expands_env_api_key() {
        let _env = EnvVarGuard::set("CODA_TEST_KEY", "secret-from-env");
        let config = parse_server_config(
            r#"
[[providers]]
id = "deepseek"
api_key = "${CODA_TEST_KEY}"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner" },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
            Path::new("/srv"),
        )
        .unwrap();
        assert_eq!(config.providers[0].api_key, "secret-from-env");
        assert_eq!(config.providers[0].kind, ProviderKind::Generic);
        assert_eq!(config.providers[0].models.len(), 1);
        assert_eq!(config.providers[0].models[0].id, "deepseek-reasoner");
        assert_eq!(config.providers[0].models[0].name, "deepseek-reasoner");
        assert!(config.providers[0].models[0].reasoning_efforts.is_empty());
    }

    #[test]
    fn parse_server_config_rejects_unknown_reasoning_effort() {
        let err = parse_server_config(
            r#"
[[providers]]
id = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", reasoning_efforts = ["ultra"] },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
            Path::new("/srv"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown reasoning effort 'ultra'"));
    }

    #[test]
    fn parse_server_config_rejects_duplicate_ids() {
        let err = parse_server_config(
            &format!(
                r#"{PROVIDERS}
[[workspaces]]
id = "coda"
path = "/tmp/a"

[[workspaces]]
id = "coda"
path = "/tmp/b"
"#
            ),
            Path::new("/srv"),
        )
        .unwrap_err();

        assert!(err.to_string().contains("duplicate workspace id"));
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
        let dir = tempfile::tempdir().unwrap();
        // Place a file where the .coda directory needs to be, so create_dir_all fails.
        std::fs::write(dir.path().join(".coda"), "blocker").unwrap();
        let config = ToolApprovalConfig::load(dir.path()).unwrap();
        let result = config.add_allow_pattern("git *");
        assert!(result.is_err());
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

    #[test]
    fn compound_commands_require_approval() {
        let config = ToolApprovalConfig::default_for(Path::new("/tmp"));
        {
            let mut inner = config.inner.lock().unwrap();
            inner.allow.push("git *".to_string());
        }
        assert!(!config.requires_approval(&shell_call("git status")));
        assert!(config.requires_approval(&shell_call("git status; rm -rf /")));
        assert!(config.requires_approval(&shell_call("git status && echo done")));
        assert!(config.requires_approval(&shell_call("git status & rm -rf /")));
        assert!(config.requires_approval(&shell_call("git status\nrm -rf /")));
        assert!(config.requires_approval(&shell_call("git log | head")));
        assert!(config.requires_approval(&shell_call("git status > /tmp/out")));
        assert!(config.requires_approval(&shell_call("git status < /dev/null")));
        assert!(config.requires_approval(&shell_call("echo `whoami`")));
        assert!(config.requires_approval(&shell_call("echo $(whoami)")));
    }

    #[test]
    fn derive_pattern_with_tab() {
        let pattern = ToolApprovalConfig::derive_pattern("git\tstatus");
        assert_eq!(pattern, "git *");
        assert!(wildcard_match(&pattern, "git\tstatus"));
        assert!(wildcard_match(&pattern, "git status"));
    }

    #[test]
    fn wildcard_whitespace_matches_any_whitespace() {
        assert!(wildcard_match("git *", "git\tstatus"));
        assert!(wildcard_match("rm -rf *", "rm\t-rf /"));
        assert!(!wildcard_match("git *", "gitk"));
    }

    fn shell_call(command: &str) -> ToolCall {
        let args = serde_json::json!({"command": command}).to_string();
        ToolCall {
            id: "test".into(),
            name: "shell".into(),
            arguments: Some(args),
        }
    }
}
