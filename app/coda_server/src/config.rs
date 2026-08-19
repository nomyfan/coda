use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_agent::{SUBAGENT_TOOL_PREFIX, ToolApprovalMode};
use coda_core::llm::{Modality, ToolCall};
use coda_openai::ProviderKind;
use serde::{Deserialize, Serialize};

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
/// back to `id` when absent). `context_window` is the model's total token
/// capacity. `reasoning_efforts` declares which effort levels the model accepts;
/// an empty list means the UI shows no reasoning controls for it.
/// `input_modalities` lists the input kinds the model accepts; every model
/// accepts text, and `image` additionally enables image attachments.
/// `auto_compact_threshold` is the token count at which a session on this
/// model automatically compacts context mid-conversation; `None` means the
/// caller should default to 80% of `context_window` — this type only carries
/// what was configured, not the resolved default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    pub context_window: u32,
    pub max_completion_tokens: Option<u32>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub input_modalities: Vec<Modality>,
    pub auto_compact_threshold: Option<u32>,
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

/// Tuning knobs for the process-level session relay (`hub::SessionHub`)'s
/// per-session in-memory event buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayConfig {
    /// Soft cap on buffered events for one turn. On overflow the oldest
    /// chunk-tier event is dropped first; message-bearing events are never
    /// dropped so the settle-fold cannot lose history.
    pub max_log_events: usize,
    /// Hard cap on buffered *message*-tier events for one turn. These can't
    /// be evicted like chunk-tier events without corrupting the fold, so a
    /// turn that buffers more than this (a runaway tool-calling loop, say) is
    /// treated like a lagged stream: the forwarder forces a resync instead of
    /// letting the log grow without bound. A turn that settles normally
    /// clears the log (and this count) long before reaching it.
    pub max_message_tier_events: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_log_events: 8192,
            max_message_tier_events: 4096,
        }
    }
}

/// How the WebSocket transport keeps an otherwise idle connection alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveConfig {
    /// How often an idle connection emits a Ping. Tune it below the idle
    /// timeout of whatever proxy or load balancer fronts the server.
    pub interval: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
        }
    }
}

/// Where sessions are persisted. Required: PostgreSQL is the only backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub providers: Vec<ProviderConfig>,
    pub workspaces: Vec<WorkspaceConfig>,
    pub database: DatabaseConfig,
    pub relay: RelayConfig,
    pub keepalive: KeepaliveConfig,
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
    let database = parse_database(&doc)?;
    let relay = parse_relay(&doc)?;
    let keepalive = parse_keepalive(&doc)?;

    Ok(ServerConfig {
        providers,
        workspaces,
        database,
        relay,
        keepalive,
    })
}

fn parse_database(doc: &toml_edit::DocumentMut) -> Result<DatabaseConfig, ConfigError> {
    let table = doc
        .get("database")
        .and_then(|item| item.as_table())
        .ok_or_else(|| {
            ConfigError::Parse(
                "missing [database] table: set `url` to a PostgreSQL connection string".to_string(),
            )
        })?;
    Ok(DatabaseConfig {
        url: expand_env(&require_str(table, "url", "database")?)?,
    })
}

/// Parse the optional `[relay]` table, falling back to `RelayConfig::default()`
/// for any field (or the whole table) that is absent.
fn parse_relay(doc: &toml_edit::DocumentMut) -> Result<RelayConfig, ConfigError> {
    let mut relay = RelayConfig::default();
    let Some(table) = doc.get("relay") else {
        return Ok(relay);
    };
    if let Some(value) = table.get("max_log_events") {
        relay.max_log_events = positive_usize(value, "relay.max_log_events")?;
    }
    if let Some(value) = table.get("max_message_tier_events") {
        relay.max_message_tier_events = positive_usize(value, "relay.max_message_tier_events")?;
    }
    Ok(relay)
}

/// Parse the optional `[keepalive]` table, falling back to
/// `KeepaliveConfig::default()` when it (or its field) is absent.
fn parse_keepalive(doc: &toml_edit::DocumentMut) -> Result<KeepaliveConfig, ConfigError> {
    let mut keepalive = KeepaliveConfig::default();
    let Some(table) = doc.get("keepalive") else {
        return Ok(keepalive);
    };
    if let Some(value) = table.get("interval_secs") {
        let secs = positive_usize(value, "keepalive.interval_secs")?;
        keepalive.interval = Duration::from_secs(secs as u64);
    }
    Ok(keepalive)
}

fn positive_usize(value: &toml_edit::Item, field: &str) -> Result<usize, ConfigError> {
    value
        .as_integer()
        .filter(|v| *v > 0)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| ConfigError::Parse(format!("{field} must be a positive integer")))
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
            Some("openrouter") => ProviderKind::OpenRouter,
            Some(other) => {
                return Err(ConfigError::Parse(format!(
                    "provider '{id}' has unknown kind '{other}' (expected 'generic', 'deepseek', or 'openrouter')"
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
            "provider '{provider_id}' 'models' must be an array of inline tables"
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
        let context_window = table
            .get("context_window")
            .and_then(|value| value.as_integer())
            .filter(|value| *value > 0)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                ConfigError::Parse(format!(
                    "provider '{provider_id}' model '{id}' context_window must be a positive integer"
                ))
            })?;
        let reasoning_efforts = parse_model_reasoning_efforts(table, provider_id, &id)?;
        let default_reasoning_effort =
            parse_default_reasoning_effort(table, provider_id, &id, &reasoning_efforts)?;
        let input_modalities = parse_model_input_modalities(table, provider_id, &id)?;
        let max_completion_tokens =
            parse_max_completion_tokens(table, provider_id, &id, context_window)?;
        let auto_compact_threshold =
            parse_auto_compact_threshold(table, provider_id, &id, context_window)?;
        models.push(ModelConfig {
            id,
            name,
            context_window,
            max_completion_tokens,
            reasoning_efforts,
            default_reasoning_effort,
            input_modalities,
            auto_compact_threshold,
        });
    }

    Ok(models)
}

fn parse_max_completion_tokens(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
    context_window: u32,
) -> Result<Option<u32>, ConfigError> {
    let Some(value) = model.get("max_completion_tokens") else {
        return Ok(None);
    };
    let max_completion_tokens = value
        .as_integer()
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            ConfigError::Parse(format!(
                "provider '{provider_id}' model '{model_name}' max_completion_tokens must be a positive integer"
            ))
        })?;
    if max_completion_tokens > context_window {
        return Err(ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' max_completion_tokens ({max_completion_tokens}) must not exceed context_window ({context_window})"
        )));
    }
    Ok(Some(max_completion_tokens))
}

fn parse_auto_compact_threshold(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
    context_window: u32,
) -> Result<Option<u32>, ConfigError> {
    let Some(value) = model.get("auto_compact_threshold") else {
        return Ok(None);
    };
    let auto_compact_threshold = value
        .as_integer()
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            ConfigError::Parse(format!(
                "provider '{provider_id}' model '{model_name}' auto_compact_threshold must be a positive integer"
            ))
        })?;
    if auto_compact_threshold > context_window {
        return Err(ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' auto_compact_threshold ({auto_compact_threshold}) must not exceed context_window ({context_window})"
        )));
    }
    Ok(Some(auto_compact_threshold))
}

fn parse_model_reasoning_efforts(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
) -> Result<Vec<String>, ConfigError> {
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
            value
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    ConfigError::Parse(format!(
                        "provider '{provider_id}' model '{model_name}' reasoning_efforts must be strings"
                    ))
                })
        })
        .collect()
}

fn parse_default_reasoning_effort(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
    reasoning_efforts: &[String],
) -> Result<Option<String>, ConfigError> {
    let Some(value) = model.get("default_reasoning_effort") else {
        return Ok(None);
    };
    let effort = value.as_str().ok_or_else(|| {
        ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' default_reasoning_effort must be a string"
        ))
    })?;
    if !reasoning_efforts.contains(&effort.to_string()) {
        return Err(ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' default_reasoning_effort '{effort}' is not in reasoning_efforts"
        )));
    }
    Ok(Some(effort.to_string()))
}

/// Parses `input_modalities`. Absent means text-only (`[text]`).
fn parse_model_input_modalities(
    model: &toml_edit::InlineTable,
    provider_id: &str,
    model_name: &str,
) -> Result<Vec<Modality>, ConfigError> {
    let Some(array) = model.get("input_modalities") else {
        return Ok(vec![Modality::Text]);
    };
    let array = array.as_array().ok_or_else(|| {
        ConfigError::Parse(format!(
            "provider '{provider_id}' model '{model_name}' input_modalities must be an array"
        ))
    })?;
    let parsed = array
        .iter()
        .map(|value| {
            let raw = value.as_str().ok_or_else(|| {
                ConfigError::Parse(format!(
                    "provider '{provider_id}' model '{model_name}' input_modalities must be strings"
                ))
            })?;
            match raw {
                "text" => Ok(Modality::Text),
                "image" => Ok(Modality::Image),
                other => Err(ConfigError::Parse(format!(
                    "provider '{provider_id}' model '{model_name}' has unknown input modality '{other}' (expected 'text' or 'image')"
                ))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Normalize: text is always supported, and order/duplicates carry no meaning.
    let mut modalities = vec![Modality::Text];
    for modality in parsed {
        if !modalities.contains(&modality) {
            modalities.push(modality);
        }
    }
    Ok(modalities)
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

/// How much a session is trusted to do without stopping to ask.
///
/// A mode is an **allow-list**: the tools it names run unattended and
/// everything else suspends for human approval. That is the inverse of
/// `[permissions.tools].approval_required`, which survives as a workspace-level
/// *tightening* list — it can force approval for a tool the mode would have
/// waved through, never the other way round.
///
/// The mode is per session and live-editable: [`PermissionModeCell`] is what
/// the approval closure reads, so switching takes effect on the next tool call
/// without rebuilding the runtime — mid-turn and mid-suspension included.
///
/// Not to be confused with `coda_agent::ToolApprovalMode`, which sits one layer
/// down: that is the runtime's *mechanism* (approve everything, ask about
/// everything, or ask this closure), while this is the user's *choice*.
/// [`ToolApprovalConfig::into_approval_mode`] is where one becomes the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Built-in inspection tools. Workspace shell rules may separately
    /// auto-approve specific commands.
    Explore,
    /// [`Self::Explore`] plus the file-editing tools. The default for a new
    /// session.
    #[default]
    AcceptEdits,
    /// Auto-approve everything, `shell` included — everything, that is, that
    /// the three rules outranking the mode leave alone: `ask_user` still opens
    /// the UI (it asks a question rather than seeking permission), the
    /// workspace's `approval_required` list still holds, and its shell `deny`
    /// rules still bite. See [`ToolApprovalConfig::requires_approval`].
    Yolo,
}

/// Tools [`PermissionMode::Explore`] runs unattended: they read the workspace
/// (or, for the todos pair, only the agent's own scratch state) and change
/// nothing on disk.
const EXPLORE_TOOLS: &[&str] = &[
    "ls",
    "read_file",
    "glob",
    "grep",
    "read_todos",
    "write_todos",
];

/// What [`PermissionMode::AcceptEdits`] adds on top of [`EXPLORE_TOOLS`].
const ACCEPT_EDITS_TOOLS: &[&str] = &["write_file", "edit_file"];

impl PermissionMode {
    /// Whether this mode lets `tool_name` run without asking. Says nothing
    /// about the workspace's own rules — [`ToolApprovalConfig::requires_approval`]
    /// applies those on top.
    pub fn auto_approves(self, tool_name: &str) -> bool {
        // Delegation is waved through at every level: handing work to a
        // sub-agent has no effect of its own, and each tool the sub-agent then
        // calls is checked against this same policy. Gating the `agent__*` call
        // as well would just charge the user two approvals for one action.
        if tool_name.starts_with(SUBAGENT_TOOL_PREFIX) {
            return true;
        }
        match self {
            Self::Yolo => true,
            Self::AcceptEdits => {
                EXPLORE_TOOLS.contains(&tool_name) || ACCEPT_EDITS_TOOLS.contains(&tool_name)
            }
            Self::Explore => EXPLORE_TOOLS.contains(&tool_name),
        }
    }
}

/// A session's live [`PermissionMode`], shared between the hub entry that owns
/// the session and the approval closure inside its runtime.
///
/// Cloning shares the cell, so a `set_permission_mode` writes what the next
/// approval check reads. The hub holds it for the life of the session, which
/// outlives any one connection: a client that reconnects to a running session is
/// told the value in here, not the one it remembered locally.
#[derive(Clone, Default)]
pub struct PermissionModeCell(Arc<Mutex<PermissionMode>>);

impl PermissionModeCell {
    pub fn new(mode: PermissionMode) -> Self {
        Self(Arc::new(Mutex::new(mode)))
    }

    pub fn get(&self) -> PermissionMode {
        *self.0.lock().unwrap()
    }

    pub fn set(&self, mode: PermissionMode) {
        *self.0.lock().unwrap() = mode;
    }
}

impl fmt::Debug for PermissionModeCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PermissionModeCell")
            .field(&self.get())
            .finish()
    }
}

/// Pattern-based permission rules for `shell` commands.
///
/// A command is parsed into its constituent simple commands (splitting on `;`,
/// newlines, `&&`, `||`, and `|`) and each is evaluated independently against
/// the rules.
/// The whole call auto-approves only when **every** simple command is allowed
/// and **none** is denied. A command auto-approves iff:
///
/// - it parses as a valid shell program, and
/// - it uses only sequencing/and-or/pipe operators — no backgrounding (`&`),
///   redirections (`>`, `<`), command substitution (`$(...)`, backticks),
///   process substitution, compound commands (subshells, loops, `if`/`case`),
///   or function definitions, and
/// - every simple command matches an `allow` pattern, and
/// - no simple command matches a `deny` pattern.
///
/// Anything that can't be statically reduced this way falls back to requiring
/// human approval (the safe default).
#[derive(Clone)]
pub struct ToolApprovalConfig {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    allow: Vec<String>,
    deny: Vec<String>,
    approval_required_tools: Vec<String>,
    config_path: PathBuf,
}

#[derive(Debug)]
struct Permissions {
    shell_allow: Vec<String>,
    shell_deny: Vec<String>,
    approval_required_tools: Vec<String>,
}

const INTERACTIVE_TOOLS: &[&str] = &["ask_user"];
/// Empty on purpose: the session's [`PermissionMode`] carries the baseline
/// policy now, and `[permissions.tools].approval_required` exists only for a
/// workspace that wants to lock something down further than the mode does.
const DEFAULT_APPROVAL_REQUIRED_TOOLS: &[&str] = &[];

impl ToolApprovalConfig {
    /// Create a default config (empty rules → all shell calls require approval)
    /// that writes to `.coda/config.toml` under the given workspace directory.
    pub fn default_for(workspace_dir: &Path) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                allow: vec![],
                deny: vec![],
                approval_required_tools: default_approval_required_tools(),
                config_path: workspace_dir.join(".coda").join("config.toml"),
            })),
        }
    }

    /// Load from `.coda/config.toml` under the given workspace directory.
    /// Returns a default (empty rules → all shell calls require approval)
    /// if the file does not exist.
    pub fn load(workspace_dir: &Path) -> Result<Self, ConfigError> {
        let config_path = workspace_dir.join(".coda").join("config.toml");
        let permissions = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            parse_permissions(&content)?
        } else {
            Permissions::default()
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                allow: permissions.shell_allow,
                deny: permissions.shell_deny,
                approval_required_tools: permissions.approval_required_tools,
                config_path,
            })),
        })
    }

    /// Build a `ToolApprovalMode` that judges calls against these rules and the
    /// session's live mode.
    ///
    /// The returned closure captures both `Arc`s, so patterns added via
    /// [`add_allow_pattern`] *and* a mode switched mid-session take effect
    /// immediately for subsequent tool calls.
    pub fn into_approval_mode(self, mode: PermissionModeCell) -> ToolApprovalMode {
        ToolApprovalMode::RequireWhen(Arc::new(move |call| {
            self.requires_approval(mode.get(), call)
        }))
    }

    /// Whether `call` should be suspended for human approval under `mode`.
    ///
    /// The order matters, and the first three rules outrank the mode — even
    /// [`PermissionMode::Yolo`]:
    ///
    /// 1. `ask_user` always opens the UI; it is a question, not a permission.
    /// 2. `[permissions.tools].approval_required` is the workspace's own lock,
    ///    and a mode must not be able to pick it.
    /// 3. A `shell` command matching a `deny` glob stays denied.
    ///
    /// Past those, `Yolo` waves everything through; the other modes check
    /// `shell` against the allow/deny rules and every other tool against the
    /// mode's own list.
    pub fn requires_approval(&self, mode: PermissionMode, call: &ToolCall) -> bool {
        if INTERACTIVE_TOOLS.iter().any(|tool| tool == &call.name) {
            return true;
        }
        let inner = self.inner.lock().unwrap();
        if inner
            .approval_required_tools
            .iter()
            .any(|pattern| wildcard_match(pattern, &call.name))
        {
            return true;
        }
        if call.name == "shell" {
            let command = extract_shell_command(call);
            if mode == PermissionMode::Yolo {
                // Yolo skips the allow-list, not the deny-list. A command that
                // can't be decomposed can't be checked against `deny` either,
                // and yolo is the mode that runs it anyway.
                return matches_deny(&command, &inner.deny);
            }
            return !is_auto_approved(&command, &inner.allow, &inner.deny);
        }
        !mode.auto_approves(&call.name)
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
    /// Keeps the command plus subcommand, then appends ` *` when arguments
    /// remain. E.g. `git status --short` → `git status *`.
    ///
    /// Leading blank and comment lines (`# …`) are stripped before deriving
    /// the pattern, because the server-side shell decomposer strips comments
    /// too. The resulting pattern matches the command the decomposer evaluates.
    fn derive_pattern(command: &str) -> String {
        let first_line = command
            .lines()
            .find(|line| {
                let trimmed = line.trim_start();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .unwrap_or(command);
        let mut tokens = first_line.split_whitespace();
        let Some(command) = tokens.next() else {
            return first_line.to_string();
        };
        let Some(subcommand) = tokens.next() else {
            return command.to_string();
        };
        let prefix = format!("{command} {subcommand}");
        if tokens.next().is_some() {
            format!("{prefix} *")
        } else {
            prefix
        }
    }

    pub(crate) fn derive_shell_allow_pattern(command: &str) -> Option<String> {
        let commands = decompose(command)?;
        let [command] = commands.as_slice() else {
            return None;
        };
        Some(Self::derive_pattern(command))
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            shell_allow: vec![],
            shell_deny: vec![],
            approval_required_tools: default_approval_required_tools(),
        }
    }
}

fn default_approval_required_tools() -> Vec<String> {
    DEFAULT_APPROVAL_REQUIRED_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect()
}

/// Whether any simple command in `command` matches a `deny` glob.
///
/// The deny-list is the one rule [`PermissionMode::Yolo`] still respects, so
/// this is deliberately the *only* question asked there: a command that fails to
/// parse yields `false` (nothing matched), and yolo runs it. Under every other
/// mode the same command goes through [`is_auto_approved`], where failing to
/// parse means "ask".
fn matches_deny(command: &str, deny: &[String]) -> bool {
    let Some(simple_commands) = decompose(command) else {
        return false;
    };
    simple_commands
        .iter()
        .any(|cmd| deny.iter().any(|p| wildcard_match(p, cmd)))
}

/// Whether `command` can be auto-approved against the given rules.
///
/// Parses the command and reduces it to a flat list of plain simple commands.
/// Returns `true` only when every simple command matches `allow` and none
/// matches `deny`. Any construct that can't be statically reduced — a parse
/// error, backgrounding, redirections, substitutions, compound commands, etc.
/// — yields `false` (require approval).
fn is_auto_approved(command: &str, allow: &[String], deny: &[String]) -> bool {
    let Some(simple_commands) = decompose(command) else {
        return false;
    };
    if simple_commands.is_empty() {
        return false;
    }
    simple_commands.iter().all(|cmd| {
        !deny.iter().any(|p| wildcard_match(p, cmd)) && allow.iter().any(|p| wildcard_match(p, cmd))
    })
}

/// Parse `command` and reduce it to the textual form of each simple command it
/// runs, e.g. `cd app && cargo test` → `["cd app", "cargo test"]`.
///
/// Returns `None` when the command can't be statically reduced to plain simple
/// commands joined by `;`/newline/`&&`/`||`/`|`: a parse error, an async (`&`)
/// separator, a redirection, a command/process substitution, or any compound
/// command (subshell, loop, conditional) or function definition.
fn decompose(command: &str) -> Option<Vec<String>> {
    use brush_parser::ast;

    let tokens = brush_parser::tokenize_str(command).ok()?;
    let program =
        brush_parser::parse_tokens(&tokens, &brush_parser::ParserOptions::default()).ok()?;

    let mut out = Vec::new();
    for ast::CompoundList(items) in &program.complete_commands {
        for ast::CompoundListItem(and_or, separator) in items {
            if !matches!(separator, ast::SeparatorOperator::Sequence) {
                return None; // backgrounding with `&`
            }
            collect_pipeline(&and_or.first, &mut out)?;
            for tail in &and_or.additional {
                let (ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline)) = tail;
                collect_pipeline(pipeline, &mut out)?;
            }
        }
    }
    Some(out)
}

fn collect_pipeline(pipeline: &brush_parser::ast::Pipeline, out: &mut Vec<String>) -> Option<()> {
    use brush_parser::ast::Command;
    for command in &pipeline.seq {
        let Command::Simple(simple) = command else {
            return None; // compound command, function, or extended test
        };
        out.push(simple_command_text(simple)?);
    }
    Some(())
}

/// The textual form of a plain simple command, or `None` if it carries anything
/// we can't statically vet: a missing command name, a redirection, a process
/// substitution, or a command substitution.
fn simple_command_text(simple: &brush_parser::ast::SimpleCommand) -> Option<String> {
    use brush_parser::ast::CommandPrefixOrSuffixItem as Item;

    simple.word_or_name.as_ref()?;

    let has_risky_item = |items: &[Item]| {
        items
            .iter()
            .any(|i| matches!(i, Item::IoRedirect(_) | Item::ProcessSubstitution(..)))
    };
    if simple.prefix.as_ref().is_some_and(|p| has_risky_item(&p.0)) {
        return None;
    }
    if simple.suffix.as_ref().is_some_and(|s| has_risky_item(&s.0)) {
        return None;
    }

    let text = simple.to_string();
    // brush keeps command/arithmetic substitution as a flat word string; reject
    // anything we can't resolve to a fixed command.
    if text.contains("$(") || text.contains('`') {
        return None;
    }
    Some(text)
}

pub(crate) fn extract_shell_command(call: &ToolCall) -> String {
    let args = call.arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v["command"].as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_permissions(content: &str) -> Result<Permissions, ConfigError> {
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let mut permissions = Permissions::default();
    let Some(root) = doc.get("permissions") else {
        return Ok(permissions);
    };

    if let Some(shell) = root.get("shell") {
        permissions.shell_allow = optional_string_array(shell, "allow")?.unwrap_or_default();
        permissions.shell_deny = optional_string_array(shell, "deny")?.unwrap_or_default();
    }

    if let Some(tools) = root.get("tools") {
        permissions.approval_required_tools = optional_string_array(tools, "approval_required")?
            .unwrap_or(permissions.approval_required_tools);
    }

    Ok(permissions)
}

fn optional_string_array(
    table: &toml_edit::Item,
    key: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| ConfigError::Parse(format!("permissions {key} must be an array")))?;
    let mut parsed = Vec::new();
    for item in array {
        let value = item
            .as_str()
            .ok_or_else(|| ConfigError::Parse(format!("permissions {key} must be strings")))?;
        parsed.push(value.to_string());
    }
    Ok(Some(parsed))
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
#[path = "config_tests.rs"]
mod tests;
