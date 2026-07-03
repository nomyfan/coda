use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::Response,
    routing::get,
};
use clap::Parser;
use coda_agent::{
    AgentTeam, ModelProfile, OpenError, ResumeDecision, RunConfig, Session, SharedSystemPrompt,
    runtime::SessionStorage,
};
use coda_core::llm::{LLMProviderConfig, Message, Modality, ReasoningEffort};
use coda_openai::OpenAI;
use coda_server::{
    agents::{
        AgentFile, ToolRegistry, build_agent_team, load_agent_files, load_root_agent_file,
        resolve_agent_workspace,
    },
    ask_user::AskUserToolSpec,
    build_workspace_knowledge,
    config::{ToolApprovalConfig, WorkspaceConfig, load_server_config},
    hub::{
        AttachSession, CommandOutcome, ConnId, RelayEvent, SessionCommand, SessionHub, SessionKey,
        SessionOpener, SessionRelay,
    },
    mcp::McpServers,
    storage::{WorkspaceStorage, validate_session_id},
    transport::{Transport, WebSocketTransport},
    wire::{
        ClientMessage, PendingApprovalWire, ProviderInfoWire, ServerMessage, SessionSummaryWire,
        WireEvent, WorkspaceSummaryWire,
    },
};
use coda_tools::{BuildContext, ToolSpec};
use futures::stream::BoxStream;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_stream::{StreamExt as _, StreamMap};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Coda server
#[derive(Parser)]
#[command(name = "coda-server", version = coda_server::VERSION)]
struct Cli {
    /// Path to the server config file.
    #[arg(
        short,
        long,
        env = "CODA_SERVER_CONFIG",
        default_value = "coda-server.toml"
    )]
    config: PathBuf,

    /// Address the server listens on.
    #[arg(
        short,
        long,
        env = "CODA_LISTEN_ADDR",
        default_value = "127.0.0.1:3000"
    )]
    listen_addr: String,
}

struct AppState {
    /// All configured providers, keyed by id. The dashboard chooses which one a
    /// session uses; `default_provider` is the fallback until it selects.
    providers: HashMap<String, Arc<ProviderHandle>>,
    default_provider: String,
    shutdown: CancellationToken,
    workspaces: HashMap<String, Arc<WorkspaceState>>,
    /// Process-level session relay: live sessions belong here, not to the
    /// connection that opened them. See `coda_server::hub`.
    hub: Arc<dyn SessionRelay>,
}

/// A constructed provider model entry. `model_id` is the API model name sent in
/// requests; `model_name` is the human-readable label shown in the dashboard.
/// Several entries can share the same `provider` when the config declares
/// multiple models under one provider. `reasoning_efforts` is the list the UI
/// offers; empty means the model has no reasoning controls.
struct ProviderHandle {
    provider: Arc<OpenAI>,
    model_id: String,
    model_name: String,
    context_window: u32,
    /// The configured provider's id.
    provider_id: String,
    /// Effort levels surfaced to the dashboard so it can render reasoning controls.
    reasoning_efforts: Vec<ReasoningEffort>,
    /// Input kinds this model accepts (always includes text; image enables attachments).
    input_modalities: Vec<Modality>,
}

struct WorkspaceState {
    id: String,
    storage: WorkspaceStorage,
    workspace_str: String,
    /// Kept alive for the process lifetime so the MCP connections behind the
    /// prebuilt tools in `agent_team` stay open; torn down on shutdown.
    mcp_servers: McpServers,
    /// Validated team rooted at the top-level `coda` agent, with all
    /// file-configured sub-agents. Built into fresh `Agent` instances for every
    /// session. The `coda` spec holds the shared system prompt the `AGENTS.md`
    /// watcher updates, so live and newly opened sessions both pick up changes on
    /// their next turn.
    agent_team: AgentTeam,
    /// Static per-agent model overrides parsed from each agent's `AGENT.md`,
    /// keyed by agent name and validated against the provider catalog at startup.
    /// Agents absent here inherit the session's default (root) model.
    agent_models: HashMap<String, ModelSelection>,
    approval_config: ToolApprovalConfig,
}

/// A validated per-agent model override: a provider selection key plus the
/// normalized reasoning effort. Resolved to a `ModelProfile` at session open.
#[derive(Clone)]
struct ModelSelection {
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
}

const MAX_TASK_IMAGES: usize = 5;
const MAX_TASK_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_TASK_IMAGE_URL_BYTES: usize = 2048;
const ACCEPTED_TASK_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];

fn sanitize_task_images(images: Vec<String>) -> Vec<String> {
    images
        .into_iter()
        .filter(|image| task_image_is_allowed(image))
        .take(MAX_TASK_IMAGES)
        .collect()
}

fn task_image_is_allowed(image: &str) -> bool {
    if image.starts_with("https://") {
        return image.len() <= MAX_TASK_IMAGE_URL_BYTES;
    }

    task_image_data_uri_decoded_len(image).is_some_and(|len| len <= MAX_TASK_IMAGE_BYTES)
}

fn task_image_data_uri_decoded_len(image: &str) -> Option<usize> {
    let (metadata, payload) = image.split_once(',')?;
    let mime_type = metadata.strip_prefix("data:")?.strip_suffix(";base64")?;

    if !ACCEPTED_TASK_IMAGE_MIME_TYPES.contains(&mime_type) || payload.is_empty() {
        return None;
    }

    if payload.len() % 4 != 0 {
        return None;
    }

    let padding = payload
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'=')
        .count();
    if padding > 2 {
        return None;
    }

    let unpadded = payload.len() - padding;
    if payload.as_bytes()[..unpadded].contains(&b'=') {
        return None;
    }

    if !payload
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return None;
    }

    payload
        .len()
        .checked_mul(3)?
        .checked_div(4)?
        .checked_sub(padding)
}

/// The hub's [`SessionOpener`]: builds sessions from the provider catalog and
/// workspace registry. Holds its own map clones so it exists independently of
/// `AppState` (the hub lives inside `AppState`).
struct AppOpener {
    providers: HashMap<String, Arc<ProviderHandle>>,
    workspaces: HashMap<String, Arc<WorkspaceState>>,
}

impl SessionOpener for AppOpener {
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<ReasoningEffort>,
        decisions: HashMap<String, ResumeDecision>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>> {
        Box::pin(async move {
            let workspace = self
                .workspaces
                .get(&key.0)
                .ok_or_else(|| OpenError::Storage(format!("unknown workspace '{}'", key.0)))?;
            open_session(
                &self.providers,
                workspace,
                &key.1,
                provider_id,
                reasoning_effort,
                decisions,
            )
            .await
        })
    }

    fn load_messages<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>> {
        Box::pin(async move {
            let Some(workspace) = self.workspaces.get(&key.0) else {
                return Vec::new();
            };
            workspace
                .storage
                .session(&key.1)
                .load_checkpoint(&key.1)
                .await
                .ok()
                .flatten()
                .map(|checkpoint| checkpoint.messages)
                .unwrap_or_default()
        })
    }
}

/// Open (or resume) the session for `session_id`, seeding it with the built-in
/// tools, MCP tools, and approval policy. `decisions` covers any pending
/// approvals carried over from a prior suspension.
async fn open_session(
    providers: &HashMap<String, Arc<ProviderHandle>>,
    workspace: &WorkspaceState,
    session_id: &str,
    provider_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    decisions: HashMap<String, ResumeDecision>,
) -> Result<Session, OpenError> {
    let provider = providers
        .get(provider_id)
        .expect("caller passes a validated provider id");
    // The root agent (and any agent without an override) runs on the session's
    // selected model.
    let default_model = ModelProfile {
        provider: provider.provider.clone(),
        model: provider.model_id.clone(),
        label: provider_id.to_string(),
        temperature: None,
        max_completion_tokens: Some(10_000),
        reasoning_effort,
    };
    // Sub-agents with a configured `model` run on their own provider/model. The
    // selections were validated against the catalog at startup, so every lookup
    // resolves.
    let agent_models = workspace
        .agent_models
        .iter()
        .map(|(name, selection)| {
            let handle = providers
                .get(&selection.provider_id)
                .expect("agent model selections are validated at startup");
            let profile = ModelProfile {
                provider: handle.provider.clone(),
                model: handle.model_id.clone(),
                label: selection.provider_id.clone(),
                temperature: None,
                max_completion_tokens: Some(10_000),
                reasoning_effort: selection.reasoning_effort,
            };
            (name.clone(), profile)
        })
        .collect();
    let config = RunConfig {
        default_model,
        agent_models,
        tool_approval: workspace.approval_config.clone().into_approval_mode(),
        // Disabled: approvals never auto-reject; a pending ask_user/tool call
        // waits indefinitely until the user resolves it.
        approval_timeout: None,
    };

    Session::builder()
        .storage(workspace.storage.session(session_id))
        .team(&workspace.agent_team, &workspace.workspace_str)
        .run_config(config)
        .session_id(session_id)
        .resume_decisions(decisions)
        .open()
        .await
}

async fn add_allow_pattern(config: ToolApprovalConfig, pattern: String) -> Result<(), String> {
    match tokio::task::spawn_blocking(move || config.add_allow_pattern(&pattern)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            let message = e.to_string();
            warn!("failed to add allow pattern: {message}");
            Err(message)
        }
        Err(e) => {
            let message = format!("failed to join allow-pattern writer: {e}");
            warn!("{message}");
            Err(message)
        }
    }
}

async fn send_allow_pattern_result<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    workspace_id: String,
    approval_config: ToolApprovalConfig,
    pattern: String,
) -> bool {
    let error = add_allow_pattern(approval_config, pattern.clone())
        .await
        .err();
    transport
        .send(&ServerMessage::AllowPatternResult {
            workspace_id,
            pattern,
            error,
        })
        .await
}

/// Send an `Error` event describing a failed session open.
async fn send_open_error<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: &T,
    workspace_id: &str,
    session_id: &str,
    err: OpenError,
) {
    let event = WireEvent::Error {
        agent_name: String::new(),
        thread_id: session_id.to_string(),
        message: format!("failed to open session: {err}"),
    };
    transport
        .send(&ServerMessage::Event {
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            event,
        })
        .await;
}

async fn workspace_catalog(app: &AppState) -> Vec<WorkspaceSummaryWire> {
    let mut ids: Vec<_> = app.workspaces.keys().cloned().collect();
    ids.sort();

    let mut workspaces = Vec::new();
    for id in ids {
        let workspace = app
            .workspaces
            .get(&id)
            .expect("workspace id came from workspace map");
        let sessions = match workspace.storage.list_sessions().await {
            Ok(sessions) => sessions
                .into_iter()
                .map(|session| SessionSummaryWire {
                    id: session.session_id,
                    updated_at_ms: session.updated_at_ms,
                    first_user_message: session.first_user_message,
                    has_pending_approval: session.has_pending_approval,
                })
                .collect(),
            Err(err) => {
                warn!(workspace_id = %workspace.id, "failed to list sessions: {err}");
                Vec::new()
            }
        };
        workspaces.push(WorkspaceSummaryWire {
            id: workspace.id.clone(),
            path: workspace.workspace_str.clone(),
            sessions,
        });
    }

    workspaces
}

/// A session's initial reasoning effort: the provider's first declared level, or
/// `None` when the model has no reasoning controls. The dashboard switches it
/// afterward with `SetModel`.
fn initial_reasoning_effort(provider: &ProviderHandle) -> Option<ReasoningEffort> {
    provider.reasoning_efforts.first().copied()
}

/// Normalize a client selection to the values exposed by the provider catalog.
/// Reasoning models use their first configured effort when the client omits one;
/// models without reasoning controls keep `None`.
fn normalize_reasoning_effort(
    configured: &[ReasoningEffort],
    reasoning_effort: Option<ReasoningEffort>,
) -> Option<Option<ReasoningEffort>> {
    match reasoning_effort {
        None => Some(configured.first().copied()),
        Some(ReasoningEffort::None) if !configured.is_empty() => Some(Some(ReasoningEffort::None)),
        Some(effort) if configured.contains(&effort) => Some(Some(effort)),
        Some(_) => None,
    }
}

/// Resolve and normalize a client-supplied model selection. Invalid selections
/// fall back to the default provider at its initial effort.
fn resolve_selection(
    app: &AppState,
    provider_id: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
) -> (String, Option<ReasoningEffort>) {
    if let Some(id) = provider_id
        && let Some(provider) = app.providers.get(&id)
        && let Some(reasoning_effort) =
            normalize_reasoning_effort(&provider.reasoning_efforts, reasoning_effort)
    {
        return (id, reasoning_effort);
    }
    let id = app.default_provider.clone();
    let effort = initial_reasoning_effort(
        app.providers
            .get(&id)
            .expect("default provider always present"),
    );
    (id, effort)
}

fn normalize_provider_selection(
    app: &AppState,
    provider_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Option<Option<ReasoningEffort>> {
    let provider = app.providers.get(provider_id)?;
    normalize_reasoning_effort(&provider.reasoning_efforts, reasoning_effort)
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn no_reasoning_controls_keep_none() {
        assert_eq!(normalize_reasoning_effort(&[], None), Some(None));
    }

    #[test]
    fn omitted_effort_uses_first_configured_level() {
        assert_eq!(
            normalize_reasoning_effort(&[ReasoningEffort::Low, ReasoningEffort::High], None),
            Some(Some(ReasoningEffort::Low))
        );
    }

    #[test]
    fn reasoning_model_accepts_off_and_configured_levels() {
        let configured = [ReasoningEffort::Low, ReasoningEffort::High];
        assert_eq!(
            normalize_reasoning_effort(&configured, Some(ReasoningEffort::None)),
            Some(Some(ReasoningEffort::None))
        );
        assert_eq!(
            normalize_reasoning_effort(&configured, Some(ReasoningEffort::High)),
            Some(Some(ReasoningEffort::High))
        );
    }

    #[test]
    fn reasoning_model_rejects_unconfigured_levels() {
        assert_eq!(
            normalize_reasoning_effort(&[ReasoningEffort::Low], Some(ReasoningEffort::High)),
            None
        );
        assert_eq!(
            normalize_reasoning_effort(&[], Some(ReasoningEffort::None)),
            None
        );
    }

    #[test]
    fn task_image_sanitizer_keeps_valid_images_up_to_the_limit() {
        let images = (0..(MAX_TASK_IMAGES + 1))
            .map(|index| format!("https://example.com/{index}.png"))
            .collect();

        let sanitized = sanitize_task_images(images);

        assert_eq!(sanitized.len(), MAX_TASK_IMAGES);
        assert_eq!(sanitized[0], "https://example.com/0.png");
        assert_eq!(sanitized[4], "https://example.com/4.png");
    }

    #[test]
    fn task_image_sanitizer_accepts_supported_data_uri_images() {
        let sanitized = sanitize_task_images(vec!["data:image/png;base64,AAAA".to_string()]);

        assert_eq!(sanitized, vec!["data:image/png;base64,AAAA"]);
    }

    #[test]
    fn task_image_sanitizer_drops_oversized_and_invalid_images() {
        let oversized_url = format!(
            "https://example.com/{}",
            "a".repeat(MAX_TASK_IMAGE_URL_BYTES)
        );
        let oversized_data = format!(
            "data:image/png;base64,{}",
            "A".repeat(((MAX_TASK_IMAGE_BYTES + 1).div_ceil(3)) * 4)
        );
        let images = vec![
            "http://example.com/image.png".to_string(),
            "data:text/plain;base64,AAAA".to_string(),
            "data:image/png;base64,AAA".to_string(),
            "data:image/png;base64,AA=A".to_string(),
            oversized_url,
            oversized_data,
        ];

        let sanitized = sanitize_task_images(images);

        assert!(sanitized.is_empty());
    }
}

/// The selectable models, sorted by id for a stable dashboard ordering. Each
/// entry's `id` is `{provider_id}:{model_id}`; `model` is the display name.
fn provider_infos(app: &AppState) -> Vec<ProviderInfoWire> {
    let mut infos: Vec<ProviderInfoWire> = app
        .providers
        .iter()
        .map(|(id, handle)| ProviderInfoWire {
            id: id.clone(),
            provider: handle.provider_id.clone(),
            model: handle.model_name.clone(),
            context_window: handle.context_window,
            reasoning_efforts: handle.reasoning_efforts.clone(),
            input_modalities: handle.input_modalities.clone(),
        })
        .collect();
    infos.sort_by(|a, b| a.id.cmp(&b.id));
    infos
}

async fn send_workspace_catalog<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    app: &AppState,
) -> bool {
    transport
        .send(&ServerMessage::WorkspaceCatalog {
            workspaces: workspace_catalog(app).await,
        })
        .await
}

async fn send_provider_catalog<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: &T,
    app: &AppState,
) -> bool {
    transport
        .send(&ServerMessage::ProviderCatalog {
            providers: provider_infos(app),
            default_provider: app.default_provider.clone(),
        })
        .await
}

async fn send_pending_approval_events<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    workspace_id: &str,
    session_id: &str,
    approvals: &[coda_agent::PendingApproval],
) -> bool {
    for approval in approvals {
        let event = WireEvent::Suspended {
            agent_name: approval.agent_name.clone(),
            thread_id: approval.thread_id.clone(),
            approval: PendingApprovalWire::from_agent(approval.clone()),
        };
        if !transport
            .send(&ServerMessage::Event {
                workspace_id: workspace_id.to_string(),
                session_id: session_id.to_string(),
                event,
            })
            .await
        {
            return false;
        }
    }
    true
}

/// Attach to `key` in the hub and wire the result to this connection: send the
/// `Snapshot`, register the replay+live stream, and remember the model
/// selection the session actually uses so a hub-initiated close can re-attach
/// transparently. Returns `false` when the transport is gone.
#[allow(clippy::too_many_arguments)]
async fn attach_and_stream<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, (String, Option<ReasoningEffort>)>,
    key: SessionKey,
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
) -> bool {
    match app
        .hub
        .attach(key.clone(), conn_id, provider_id, reasoning_effort)
        .await
    {
        Ok(AttachSession { snapshot, events }) => {
            selections.insert(
                key.clone(),
                (snapshot.provider_id.clone(), snapshot.reasoning_effort),
            );
            if !transport
                .send(&ServerMessage::Snapshot {
                    workspace_id: key.0.clone(),
                    session_id: key.1.clone(),
                    messages: snapshot.messages,
                    pending_approvals: snapshot
                        .pending_approvals
                        .into_iter()
                        .map(PendingApprovalWire::from_agent)
                        .collect(),
                    provider_id: snapshot.provider_id,
                    reasoning_effort: snapshot.reasoning_effort,
                    turn_running: snapshot.turn_running,
                })
                .await
            {
                return false;
            }
            // Replay + live events flow through the connection's stream map
            // from here on.
            streams.insert(key, events);
            true
        }
        Err(err) => {
            send_open_error(transport, &key.0, &key.1, err).await;
            true
        }
    }
}

async fn handle_dashboard_command<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, (String, Option<ReasoningEffort>)>,
    command: ClientMessage,
) -> bool {
    match command {
        ClientMessage::ListWorkspaces => send_workspace_catalog(transport, app).await,
        ClientMessage::ListProviders => send_provider_catalog(transport, app).await,
        ClientMessage::OpenSession {
            workspace_id,
            session_id,
            provider_id,
            reasoning_effort,
        } => {
            if let Err(err) = validate_session_id(&session_id) {
                warn!(workspace_id = %workspace_id, "rejecting open: {err}");
                return true;
            }
            if !app.workspaces.contains_key(&workspace_id) {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            }
            // Honor the client's chosen model (e.g. picked on a new session),
            // otherwise fall back to the default provider. Ignored when the
            // session is already live in the hub.
            let (provider_id, reasoning_effort) =
                resolve_selection(app, provider_id, reasoning_effort);
            let key = (workspace_id, session_id);
            if !attach_and_stream(
                transport,
                app,
                conn_id,
                streams,
                selections,
                key,
                provider_id,
                reasoning_effort,
            )
            .await
            {
                return false;
            }
            send_workspace_catalog(transport, app).await
        }
        ClientMessage::Task {
            workspace_id,
            session_id,
            task,
            images,
        } => {
            let key = (workspace_id.clone(), session_id.clone());
            // Reject image input when the active model does not accept it. The
            // frontend already disables image sends for such models, so reaching
            // here means a UI bypass — surface an error rather than silently
            // dropping the attachments.
            let accepts_images = match app.hub.provider_of(key.clone()).await {
                Some(provider_id) => app
                    .providers
                    .get(&provider_id)
                    .is_some_and(|h| h.input_modalities.contains(&Modality::Image)),
                None => return true, // no live/pending session for this key
            };
            if !accepts_images && !images.is_empty() {
                let event = WireEvent::Error {
                    agent_name: String::new(),
                    thread_id: session_id.clone(),
                    message: "the selected model does not accept image input".to_string(),
                };
                return transport
                    .send(&ServerMessage::Event {
                        workspace_id,
                        session_id,
                        event,
                    })
                    .await;
            }
            let images = sanitize_task_images(images);
            let task = task.trim().to_string();
            if task.is_empty() && images.is_empty() {
                return true;
            }
            app.hub
                .command(key, conn_id, SessionCommand::Task { task, images })
                .await;
            true
        }
        ClientMessage::Resume {
            workspace_id,
            session_id,
            agent_name,
            thread_id,
            decision,
        } => {
            let key = (workspace_id.clone(), session_id.clone());
            match app
                .hub
                .command(
                    key,
                    conn_id,
                    SessionCommand::Resume {
                        agent_name,
                        thread_id,
                        decision,
                    },
                )
                .await
            {
                CommandOutcome::StillPending(more) => {
                    send_pending_approval_events(transport, &workspace_id, &session_id, &more).await
                }
                CommandOutcome::OpenFailed(err) => {
                    send_open_error(transport, &workspace_id, &session_id, err).await;
                    true
                }
                _ => true,
            }
        }
        ClientMessage::Abort {
            workspace_id,
            session_id,
        } => {
            app.hub
                .command((workspace_id, session_id), conn_id, SessionCommand::Abort)
                .await;
            true
        }
        ClientMessage::DeleteSession {
            workspace_id,
            session_id,
        } => {
            if let Err(err) = validate_session_id(&session_id) {
                warn!(workspace_id = %workspace_id, "rejecting delete: {err}");
                return true;
            }
            let key = (workspace_id.clone(), session_id.clone());
            // Drop our own stream first so this connection doesn't see its own
            // eviction; then stop the runtime before removing its files so no
            // checkpoint is written back after deletion.
            streams.remove(&key);
            selections.remove(&key);
            app.hub.delete(key).await;
            let Some(workspace) = app.workspaces.get(&workspace_id) else {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            };
            if let Err(err) = workspace.storage.delete_session(&session_id).await {
                warn!(workspace_id = %workspace_id, session_id = %session_id, "failed to delete session: {err}");
            }
            send_workspace_catalog(transport, app).await
        }
        ClientMessage::CloseSession {
            workspace_id,
            session_id,
        } => {
            let key = (workspace_id, session_id);
            streams.remove(&key);
            selections.remove(&key);
            // The hub keeps the session alive while a turn is in flight and
            // releases it once idle — no per-connection deferral needed.
            app.hub.detach(key, conn_id).await;
            true
        }
        ClientMessage::AddAllowPattern {
            workspace_id,
            pattern,
        } => {
            let Some(workspace) = app.workspaces.get(&workspace_id) else {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            };
            send_allow_pattern_result(
                transport,
                workspace_id,
                workspace.approval_config.clone(),
                pattern,
            )
            .await
        }
        ClientMessage::SetModel {
            workspace_id,
            session_id,
            provider_id,
            reasoning_effort,
        } => {
            let Some(reasoning_effort) =
                normalize_provider_selection(app, &provider_id, reasoning_effort)
            else {
                warn!(workspace_id = %workspace_id, %provider_id, "set_model has an invalid selection");
                return true;
            };
            let key = (workspace_id.clone(), session_id.clone());
            match app
                .hub
                .command(
                    key,
                    conn_id,
                    SessionCommand::SetModel {
                        provider_id,
                        reasoning_effort,
                    },
                )
                .await
            {
                CommandOutcome::ModelChanged {
                    provider_id,
                    reasoning_effort,
                } => {
                    transport
                        .send(&ServerMessage::ModelChanged {
                            workspace_id,
                            session_id,
                            provider_id,
                            reasoning_effort,
                        })
                        .await
                }
                // Pending approvals: already warned in the hub, nothing to send.
                CommandOutcome::OpenFailed(OpenError::PendingApprovalsRequired(_)) => true,
                CommandOutcome::OpenFailed(err) => {
                    send_open_error(transport, &workspace_id, &session_id, err).await;
                    true
                }
                _ => true,
            }
        }
    }
}

/// Connection ids distinguish clients inside the hub (latest-wins eviction and
/// stale-command rejection). Monotonic per process.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

async fn run_dashboard<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: T,
    app: Arc<AppState>,
) {
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    // Event streams of the sessions this connection is attached to. A stream
    // that ends silently (detach/takeover elsewhere) is dropped from the map
    // automatically.
    let mut streams: StreamMap<SessionKey, BoxStream<'static, RelayEvent>> = StreamMap::new();
    // The model selection each attached session actually uses, so a
    // hub-initiated close can re-attach with the same model.
    let mut selections: HashMap<SessionKey, (String, Option<ReasoningEffort>)> = HashMap::new();
    // Keys re-attached after a `Closed` that have produced no event since:
    // a session that closes again right away is not retried (no reopen loop).
    let mut reattached: std::collections::HashSet<SessionKey> = std::collections::HashSet::new();

    if !send_workspace_catalog(&transport, &app).await {
        return;
    }
    if !send_provider_catalog(&transport, &app).await {
        return;
    }

    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            command = transport.recv() => {
                match command {
                    Some(command) => {
                        if !handle_dashboard_command(
                            &transport,
                            &app,
                            conn_id,
                            &mut streams,
                            &mut selections,
                            command,
                        ).await {
                            break;
                        }
                    }
                    // Disconnect: sessions live in the hub; running turns keep
                    // going and can be re-attached mid-flight on reconnect.
                    None => break,
                }
            }
            Some((key, event)) = streams.next(), if !streams.is_empty() => {
                let forwarded = match event {
                    RelayEvent::Event(event) => {
                        reattached.remove(&key);
                        transport.send(&ServerMessage::Event {
                            workspace_id: key.0.clone(),
                            session_id: key.1.clone(),
                            event: *event,
                        }).await
                    }
                    RelayEvent::Evicted => {
                        streams.remove(&key);
                        selections.remove(&key);
                        reattached.remove(&key);
                        transport.send(&ServerMessage::SessionEvicted {
                            workspace_id: key.0,
                            session_id: key.1,
                        }).await
                    }
                    // The hub ended this stream: a lag drain (resync from
                    // disk) or the runtime terminating. Re-attach with a fresh
                    // snapshot instead of leaving the client without a signal;
                    // the hub's release barrier makes the fresh attach wait for
                    // the final checkpoint. Guarded to one silent retry so a
                    // session that dies on open can't loop, and skipped during
                    // process shutdown.
                    RelayEvent::Closed => {
                        streams.remove(&key);
                        let selection = selections.remove(&key);
                        if let Some((provider_id, reasoning_effort)) = selection
                            && !app.shutdown.is_cancelled()
                            && reattached.insert(key.clone())
                        {
                            info!(workspace_id = %key.0, session_id = %key.1, "session closed by hub; re-attaching");
                            attach_and_stream(
                                &transport,
                                &app,
                                conn_id,
                                &mut streams,
                                &mut selections,
                                key,
                                provider_id,
                                reasoning_effort,
                            ).await
                        } else {
                            true
                        }
                    }
                };
                if !forwarded {
                    break;
                }
            }
        }
    }

    app.hub.detach_all(conn_id).await;
}

async fn dashboard_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        info!("dashboard connected");
        run_dashboard(WebSocketTransport::new(socket), state).await;
        info!("dashboard connection closed");
    })
}

/// Poll the workspace's knowledge sources — `AGENTS.md` and the skills directory
/// — and refresh the shared workspace-knowledge handle in place whenever the
/// rendered text changes. Polling (over an OS watcher) keeps this
/// dependency-free and robust to editor atomic-saves; the few-second latency is
/// immaterial since the prompt is only read at the start of a turn.
fn spawn_workspace_knowledge_watcher(
    workspace_id: String,
    workspace_str: String,
    workspace_knowledge: SharedSystemPrompt,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut last = workspace_knowledge.get();
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let current = build_workspace_knowledge(&workspace_str);
                    if current != last {
                        last = current.clone();
                        workspace_knowledge.set(current);
                        info!(workspace_id = %workspace_id, path = %workspace_str, "workspace knowledge changed, reloaded");
                    }
                }
            }
        }
    });
}

fn display_path(path: &FsPath) -> String {
    path.to_string_lossy().into_owned()
}

/// Validate each agent file's optional `model` override against the provider
/// catalog, returning a map of agent name → resolved selection. A reference to
/// an unknown model, or a reasoning effort the model doesn't offer, is a hard
/// startup error — so a misconfigured agent can never silently fall back to the
/// default model. Reasoning effort is normalized exactly like a dashboard
/// selection (omitted → the model's first configured level).
fn resolve_agent_model_selections(
    files: &[AgentFile],
    providers: &HashMap<String, Arc<ProviderHandle>>,
) -> Result<HashMap<String, ModelSelection>, String> {
    let mut selections = HashMap::new();
    for file in files {
        let Some(model_key) = file.model() else {
            continue;
        };
        let Some(handle) = providers.get(model_key) else {
            return Err(format!(
                "agent '{}' references unknown model '{}'",
                file.name(),
                model_key
            ));
        };
        let Some(reasoning_effort) =
            normalize_reasoning_effort(&handle.reasoning_efforts, file.reasoning_effort())
        else {
            return Err(format!(
                "agent '{}' model '{}' does not support reasoning effort {:?}",
                file.name(),
                model_key,
                file.reasoning_effort()
            ));
        };
        selections.insert(
            file.name().to_string(),
            ModelSelection {
                provider_id: model_key.to_string(),
                reasoning_effort,
            },
        );
    }
    Ok(selections)
}

async fn build_workspace(
    workspace: WorkspaceConfig,
    providers: &HashMap<String, Arc<ProviderHandle>>,
    shutdown: &CancellationToken,
) -> Result<WorkspaceState, String> {
    let workspace_dir = workspace.path.canonicalize().map_err(|e| {
        format!(
            "failed to resolve workspace '{}': {e}",
            workspace.path.display()
        )
    })?;
    let workspace_str = display_path(&workspace_dir);

    let root_agent = load_root_agent_file(&workspace_dir).map_err(|e| e.to_string())?;
    let base_prompt = root_agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| coda_server::SYSTEM_PROMPT.to_string());
    let root_base = SharedSystemPrompt::new(base_prompt);

    let checkpoint_dir = workspace_dir.join(".coda").join("sessions");
    let storage = WorkspaceStorage::new(checkpoint_dir);

    let mcp_servers = coda_server::mcp::load_mcp_servers(&workspace_dir)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                workspace_id = %workspace.id,
                "failed to load MCP servers: {e}"
            );
            McpServers::empty()
        });

    let registry_ctx = BuildContext::new(workspace_str.clone());
    let mut registry = ToolRegistry::new();
    registry.insert(AskUserToolSpec.build(&registry_ctx));
    for tool in mcp_servers.tool_objects() {
        registry.insert(tool);
    }

    let agent_files = load_agent_files(&workspace_dir).map_err(|e| e.to_string())?;
    let agent_models = resolve_agent_model_selections(&agent_files, providers)?;

    // Resolve each agent's workspace (relative to the root, default = root). The
    // root is the session workspace itself.
    let mut agent_workspaces: HashMap<String, String> = HashMap::new();
    for file in &agent_files {
        let resolved = resolve_agent_workspace(file.name(), &workspace_str, file.workspace())
            .map_err(|e| e.to_string())?;
        if resolved != workspace_str {
            agent_workspaces.insert(file.name().to_string(), resolved);
        }
    }

    // One workspace-knowledge handle (and watcher) per distinct workspace, shared
    // by every agent rooted there. The watcher refreshes the text in place; the
    // env block is rendered fresh every turn.
    let distinct: BTreeSet<String> = std::iter::once(workspace_str.clone())
        .chain(agent_workspaces.values().cloned())
        .collect();
    let mut knowledge: HashMap<String, SharedSystemPrompt> = HashMap::new();
    for ws in distinct {
        let handle = SharedSystemPrompt::new(build_workspace_knowledge(&ws));
        spawn_workspace_knowledge_watcher(
            workspace.id.clone(),
            ws.clone(),
            handle.clone(),
            shutdown.clone(),
        );
        knowledge.insert(ws, handle);
    }

    let agent_team = build_agent_team(
        &workspace_str,
        root_base,
        root_agent.env,
        &knowledge,
        &agent_workspaces,
        &registry,
        agent_files,
        root_agent.tools,
        root_agent.subagents,
    )
    .map_err(|e| e.to_string())?;

    let approval_config = ToolApprovalConfig::load(&workspace_dir).unwrap_or_else(|e| {
        tracing::warn!(
            workspace_id = %workspace.id,
            "failed to load approval config: {e}"
        );
        ToolApprovalConfig::default_for(&workspace_dir)
    });

    info!(
        workspace_id = %workspace.id,
        path = %workspace_str,
        "workspace loaded"
    );

    Ok(WorkspaceState {
        id: workspace.id,
        storage,
        workspace_str,
        mcp_servers,
        agent_team,
        agent_models,
        approval_config,
    })
}

/// Resolve once the process receives SIGINT (Ctrl+C) or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[tokio::main]
async fn main() {
    // Load `.env` before parsing so the env-var fallbacks on `Cli` see it.
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let shutdown = CancellationToken::new();

    let config_path = cli.config;
    let server_config = load_server_config(&config_path).unwrap_or_else(|e| {
        eprintln!("error loading {}: {e}", config_path.display());
        eprintln!("example:");
        eprintln!("[[providers]]");
        eprintln!("id = \"deepseek\"");
        eprintln!("kind = \"deepseek\"");
        eprintln!("api_key = \"${{DEEPSEEK_API_KEY}}\"");
        eprintln!("base_url = \"https://api.deepseek.com\"");
        eprintln!("models = [");
        eprintln!("  {{ id = \"deepseek-reasoner\", name = \"DeepSeek R1\", context_window = 128000, reasoning_efforts = [\"low\", \"medium\", \"high\"] }},");
        eprintln!("]");
        eprintln!();
        eprintln!("[[workspaces]]");
        eprintln!("id = \"coda\"");
        eprintln!("path = \"/path/to/workspace\"");
        std::process::exit(1);
    });

    // Config guarantees at least one provider with at least one model;
    // the first model is the session default.
    let default_provider = {
        let first = &server_config.providers[0];
        format!("{}:{}", first.id, first.models[0].id)
    };
    let providers: HashMap<String, Arc<ProviderHandle>> = server_config
        .providers
        .into_iter()
        .flat_map(|p| {
            let shared_provider = Arc::new(OpenAI::new(
                LLMProviderConfig {
                    api_key: p.api_key,
                    base_url: p.base_url,
                    include_usage: p.include_usage,
                },
                p.kind,
            ));
            p.models.into_iter().map(move |m| {
                let id = format!("{}:{}", p.id, m.id);
                let handle = ProviderHandle {
                    provider: shared_provider.clone(),
                    model_id: m.id,
                    model_name: m.name,
                    context_window: m.context_window,
                    provider_id: p.id.clone(),
                    reasoning_efforts: m.reasoning_efforts,
                    input_modalities: m.input_modalities,
                };
                (id, Arc::new(handle))
            })
        })
        .collect();

    let mut workspaces = HashMap::new();
    for workspace in server_config.workspaces {
        let id = workspace.id.clone();
        let state = build_workspace(workspace, &providers, &shutdown)
            .await
            .unwrap_or_else(|e| {
                eprintln!("error loading workspace '{id}': {e}");
                std::process::exit(1);
            });
        workspaces.insert(id, Arc::new(state));
    }

    // The hub owns live sessions process-wide; its opener holds its own map
    // clones so it exists independently of `AppState`.
    let hub: Arc<dyn SessionRelay> = Arc::new(SessionHub::new(Arc::new(AppOpener {
        providers: providers.clone(),
        workspaces: workspaces.clone(),
    })));

    let state = Arc::new(AppState {
        providers,
        default_provider,
        shutdown: shutdown.clone(),
        workspaces,
        hub,
    });

    // On a shutdown signal, cancel the token: this both ends `axum::serve`'s
    // graceful shutdown and (via child tokens) tears down the live session.
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown_signal().await;
            info!("shutdown signal received");
            shutdown.cancel();
        }
    });

    let app = Router::new()
        .route("/ws", get(dashboard_ws_handler))
        .with_state(state.clone());

    let listen_addr = cli.listen_addr;
    let listener = tokio::net::TcpListener::bind(&listen_addr).await.unwrap();
    info!(
        "coda_server {} listening on ws://{listen_addr}/ws",
        coda_server::VERSION
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .unwrap();

    // Stop every live session (graceful, checkpoints on disk) before tearing
    // down the MCP connections their tools may still be using.
    state.hub.shutdown_all().await;

    match Arc::try_unwrap(state) {
        Ok(app_state) => {
            // Drop the hub first: its opener holds workspace references that
            // would otherwise keep the `Arc::try_unwrap` below from succeeding.
            drop(app_state.hub);
            for (workspace_id, workspace) in app_state.workspaces {
                match Arc::try_unwrap(workspace) {
                    Ok(workspace) => workspace.mcp_servers.shutdown().await,
                    Err(_) => warn!(
                        workspace_id = %workspace_id,
                        "cannot shutdown MCP servers: outstanding references"
                    ),
                }
            }
        }
        Err(_) => warn!("cannot shutdown MCP servers: outstanding references"),
    }

    info!("server stopped");
}
