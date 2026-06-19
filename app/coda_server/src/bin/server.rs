use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::Response,
    routing::get,
};
use coda_agent::{
    AgentTeam, ModelProfile, OpenError, ResumeDecision, RunConfig, Session, SharedSystemPrompt,
    Shutdown, SystemPrompt, runtime::SessionStorage,
};
use coda_core::llm::{LLMProviderConfig, Modality, ReasoningEffort};
use coda_openai::OpenAI;
use coda_server::{
    agents::{AgentFile, ToolRegistry, build_agent_team, load_agent_files, load_root_agent_file},
    ask_user::AskUserToolSpec,
    build_system_prompt,
    config::{ToolApprovalConfig, WorkspaceConfig, load_server_config},
    mcp::McpServers,
    storage::{WorkspaceStorage, validate_session_id},
    transport::{Transport, WebSocketTransport},
    wire::{
        ClientMessage, ProviderInfoWire, ServerMessage, SessionSummaryWire, WireEvent,
        WorkspaceSummaryWire,
    },
};
use coda_tools::{BuildContext, ToolSpec};
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

struct AppState {
    /// All configured providers, keyed by id. The dashboard chooses which one a
    /// session uses; `default_provider` is the fallback until it selects.
    providers: HashMap<String, Arc<ProviderHandle>>,
    default_provider: String,
    shutdown: CancellationToken,
    workspaces: HashMap<String, Arc<WorkspaceState>>,
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

struct ActiveSession {
    generation: u64,
    session: Session,
    root_name: String,
    turn_running: bool,
    /// The provider/model and reasoning setting this session was opened with.
    /// Switching them reopens the session (see `SetModel`).
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
}

struct PendingOpen {
    workspace: Arc<WorkspaceState>,
    session_id: String,
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
    needed: HashSet<String>,
    decisions: HashMap<String, ResumeDecision>,
}

enum OpenedSession {
    Live(Session),
    Pending(Vec<coda_agent::PendingApproval>),
}

const MAX_TASK_IMAGES: usize = 5;
const MAX_TASK_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_TASK_IMAGE_URL_BYTES: usize = 2048;
const ACCEPTED_TASK_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];

enum SessionEnvelope {
    Event {
        workspace_id: String,
        session_id: String,
        generation: u64,
        // Boxed: `WireEvent` is far larger than the other variant, so keeping it
        // inline would bloat every `SessionEnvelope` (clippy large_enum_variant).
        event: Box<WireEvent>,
    },
    Closed {
        workspace_id: String,
        session_id: String,
        generation: u64,
    },
}

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

/// Open (or resume) the session for `session_id`, seeding it with the built-in
/// tools, MCP tools, and approval policy. `decisions` covers any pending
/// approvals carried over from a prior suspension.
async fn open_session(
    app: &AppState,
    workspace: &WorkspaceState,
    session_id: &str,
    provider_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    decisions: HashMap<String, ResumeDecision>,
) -> Result<Session, OpenError> {
    let provider = app
        .providers
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
            let handle = app
                .providers
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

async fn open_session_and_send_snapshot<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    app: &AppState,
    workspace: &WorkspaceState,
    session_id: &str,
    provider_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    decisions: HashMap<String, ResumeDecision>,
) -> Option<OpenedSession> {
    let first = match open_session(
        app,
        workspace,
        session_id,
        provider_id,
        reasoning_effort,
        HashMap::new(),
    )
    .await
    {
        Ok(session) => Ok(session),
        Err(OpenError::PendingApprovalsRequired(pending)) => Err(pending),
        Err(err) => {
            send_open_error(transport, &workspace.id, session_id, err).await;
            return None;
        }
    };
    let pending = match &first {
        Err(pending) => pending.clone(),
        _ => Vec::new(),
    };

    let messages = workspace
        .storage
        .session(session_id)
        .load_checkpoint(session_id)
        .await
        .ok()
        .flatten()
        .map(|checkpoint| checkpoint.messages)
        .unwrap_or_default();
    if !transport
        .send(&ServerMessage::Snapshot {
            workspace_id: workspace.id.clone(),
            session_id: session_id.to_string(),
            messages,
            pending_approvals: pending.clone(),
            provider_id: provider_id.to_string(),
            reasoning_effort,
        })
        .await
    {
        return None;
    }

    match first {
        Ok(session) => Some(OpenedSession::Live(session)),
        Err(_) if decisions.is_empty() => Some(OpenedSession::Pending(pending)),
        Err(_) => match open_session(
            app,
            workspace,
            session_id,
            provider_id,
            reasoning_effort,
            decisions,
        )
        .await
        {
            Ok(session) => Some(OpenedSession::Live(session)),
            Err(OpenError::PendingApprovalsRequired(more)) => Some(OpenedSession::Pending(more)),
            Err(err) => {
                send_open_error(transport, &workspace.id, session_id, err).await;
                None
            }
        },
    }
}

fn event_settles_turn(event: &WireEvent, root_name: &str) -> bool {
    match event {
        WireEvent::LlmEnd {
            agent_name,
            message,
            ..
        } => agent_name == root_name && message.tool_calls.is_empty(),
        WireEvent::Suspended { .. } => true,
        WireEvent::Aborted { agent_name, .. } | WireEvent::Error { agent_name, .. } => {
            agent_name == root_name
        }
        _ => false,
    }
}

fn make_pending_open(
    workspace: Arc<WorkspaceState>,
    session_id: String,
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
    approvals: Vec<coda_agent::PendingApproval>,
) -> PendingOpen {
    PendingOpen {
        workspace,
        session_id,
        provider_id,
        reasoning_effort,
        needed: approvals
            .into_iter()
            .map(|approval| approval.thread_id)
            .collect(),
        decisions: HashMap::new(),
    }
}

fn spawn_session_forwarder(
    workspace_id: String,
    session_id: String,
    generation: u64,
    session: Session,
    root_name: String,
    tx: mpsc::UnboundedSender<SessionEnvelope>,
) {
    tokio::spawn(async move {
        while let Some(event) = session.recv().await {
            let event = WireEvent::from_session_event(event, &root_name);
            if tx
                .send(SessionEnvelope::Event {
                    workspace_id: workspace_id.clone(),
                    session_id: session_id.clone(),
                    generation,
                    event: Box::new(event),
                })
                .is_err()
            {
                return;
            }
        }
        let _ = tx.send(SessionEnvelope::Closed {
            workspace_id,
            session_id,
            generation,
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn insert_active_session(
    active: &mut HashMap<(String, String), ActiveSession>,
    tx: &mpsc::UnboundedSender<SessionEnvelope>,
    next_generation: &mut u64,
    workspace: Arc<WorkspaceState>,
    session_id: String,
    session: Session,
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
) {
    let generation = *next_generation;
    *next_generation += 1;
    let workspace_id = workspace.id.clone();
    let root_name = session.root_name().to_string();
    let turn_running = session.has_resuming_agents();
    spawn_session_forwarder(
        workspace_id.clone(),
        session_id.clone(),
        generation,
        session.clone(),
        root_name.clone(),
        tx.clone(),
    );
    active.insert(
        (workspace_id.clone(), session_id.clone()),
        ActiveSession {
            generation,
            session,
            root_name,
            turn_running,
            provider_id,
            reasoning_effort,
        },
    );
    info!(workspace_id = %workspace_id, session_id = %session_id, "session opened");
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
            approval: approval.clone(),
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

async fn handle_dashboard_command<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    app: &Arc<AppState>,
    active: &mut HashMap<(String, String), ActiveSession>,
    pending: &mut HashMap<(String, String), PendingOpen>,
    tx: &mpsc::UnboundedSender<SessionEnvelope>,
    next_generation: &mut u64,
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
            let key = (workspace_id.clone(), session_id.clone());
            if active.contains_key(&key) || pending.contains_key(&key) {
                return true;
            }
            let Some(workspace) = app.workspaces.get(&workspace_id).cloned() else {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            };
            // Honor the client's chosen model (e.g. picked on a new session),
            // otherwise fall back to the default provider.
            let (provider_id, reasoning_effort) =
                resolve_selection(app, provider_id, reasoning_effort);
            match open_session_and_send_snapshot(
                transport,
                app,
                &workspace,
                &session_id,
                &provider_id,
                reasoning_effort,
                HashMap::new(),
            )
            .await
            {
                Some(OpenedSession::Live(session)) => {
                    insert_active_session(
                        active,
                        tx,
                        next_generation,
                        workspace,
                        session_id,
                        session,
                        provider_id,
                        reasoning_effort,
                    );
                    send_workspace_catalog(transport, app).await
                }
                Some(OpenedSession::Pending(approvals)) => {
                    pending.insert(
                        key,
                        make_pending_open(
                            workspace,
                            session_id,
                            provider_id,
                            reasoning_effort,
                            approvals,
                        ),
                    );
                    true
                }
                None => true,
            }
        }
        ClientMessage::Task {
            workspace_id,
            session_id,
            task,
            images,
        } => {
            let Some(active_session) = active.get_mut(&(workspace_id.clone(), session_id.clone()))
            else {
                return true;
            };
            // Reject image input when the active model does not accept it. The
            // frontend already disables image sends for such models, so reaching
            // here means a UI bypass — surface an error rather than silently
            // dropping the attachments.
            let accepts_images = app
                .providers
                .get(&active_session.provider_id)
                .is_some_and(|h| h.input_modalities.contains(&Modality::Image));
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
            if let Err(err) = active_session.session.send(task, images).await {
                warn!(workspace_id = %workspace_id, session_id = %session_id, "failed to send task: {err}");
            } else {
                active_session.turn_running = true;
            }
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
            if let Some(active_session) = active.get_mut(&key) {
                if let Err(err) = active_session
                    .session
                    .resume(&agent_name, &thread_id, decision)
                    .await
                {
                    warn!(workspace_id = %workspace_id, session_id = %session_id, "failed to resume: {err}");
                } else {
                    active_session.turn_running = true;
                }
                return true;
            }

            let Some(pending_open) = pending.get_mut(&key) else {
                return true;
            };
            pending_open.needed.remove(&thread_id);
            pending_open.decisions.insert(thread_id, decision);
            if !pending_open.needed.is_empty() {
                return true;
            }

            let Some(mut pending_open) = pending.remove(&key) else {
                return true;
            };
            let provider_id = pending_open.provider_id.clone();
            let reasoning_effort = pending_open.reasoning_effort;
            match open_session(
                app,
                &pending_open.workspace,
                &pending_open.session_id,
                &provider_id,
                reasoning_effort,
                std::mem::take(&mut pending_open.decisions),
            )
            .await
            {
                Ok(session) => {
                    insert_active_session(
                        active,
                        tx,
                        next_generation,
                        pending_open.workspace,
                        pending_open.session_id,
                        session,
                        provider_id,
                        reasoning_effort,
                    );
                }
                Err(OpenError::PendingApprovalsRequired(more)) => {
                    if !send_pending_approval_events(transport, &workspace_id, &session_id, &more)
                        .await
                    {
                        return false;
                    }
                    pending.insert(
                        key,
                        make_pending_open(
                            pending_open.workspace,
                            pending_open.session_id,
                            provider_id,
                            reasoning_effort,
                            more,
                        ),
                    );
                }
                Err(err) => send_open_error(transport, &workspace_id, &session_id, err).await,
            }
            true
        }
        ClientMessage::Abort {
            workspace_id,
            session_id,
        } => {
            if let Some(active_session) = active.get(&(workspace_id, session_id)) {
                active_session.session.abort().await;
            }
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
            // Stop a live session before removing its files so no checkpoint is
            // written back after deletion.
            if let Some(active_session) = active.remove(&key) {
                active_session.session.shutdown(Shutdown::Abort).await;
            }
            pending.remove(&key);
            let Some(workspace) = app.workspaces.get(&workspace_id) else {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            };
            if let Err(err) = workspace.storage.delete_session(&session_id).await {
                warn!(workspace_id = %workspace_id, session_id = %session_id, "failed to delete session: {err}");
            }
            send_workspace_catalog(transport, app).await
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
            let key = (workspace_id.clone(), session_id.clone());
            let Some(reasoning_effort) =
                normalize_provider_selection(app, &provider_id, reasoning_effort)
            else {
                warn!(workspace_id = %workspace_id, %provider_id, "set_model has an invalid selection");
                return true;
            };
            let Some(active_session) = active.get(&key) else {
                return true;
            };
            // Nothing to do if the selection is unchanged.
            if active_session.provider_id == provider_id
                && active_session.reasoning_effort == reasoning_effort
            {
                return true;
            }
            // The session is rebuilt with a new RunConfig; only safe while idle.
            if active_session.turn_running {
                warn!(workspace_id = %workspace_id, session_id = %session_id, "ignoring set_model while a turn is running");
                return true;
            }
            let Some(workspace) = app.workspaces.get(&workspace_id).cloned() else {
                warn!(workspace_id = %workspace_id, "unknown workspace requested");
                return true;
            };
            // Open the replacement before tearing down the current session, so a
            // failed open leaves the existing one intact.
            match open_session(
                app,
                &workspace,
                &session_id,
                &provider_id,
                reasoning_effort,
                HashMap::new(),
            )
            .await
            {
                Ok(session) => {
                    if let Some(old) = active.remove(&key) {
                        old.session.shutdown(Shutdown::Abort).await;
                    }
                    insert_active_session(
                        active,
                        tx,
                        next_generation,
                        workspace,
                        session_id.clone(),
                        session,
                        provider_id.clone(),
                        reasoning_effort,
                    );
                    return transport
                        .send(&ServerMessage::ModelChanged {
                            workspace_id,
                            session_id,
                            provider_id,
                            reasoning_effort,
                        })
                        .await;
                }
                Err(OpenError::PendingApprovalsRequired(_)) => {
                    warn!(workspace_id = %workspace_id, session_id = %session_id, "cannot switch model while approvals are pending");
                }
                Err(err) => send_open_error(transport, &workspace_id, &session_id, err).await,
            }
            true
        }
    }
}

async fn handle_session_envelope<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    active: &mut HashMap<(String, String), ActiveSession>,
    envelope: SessionEnvelope,
    client_connected: bool,
) -> bool {
    match envelope {
        SessionEnvelope::Event {
            workspace_id,
            session_id,
            generation,
            event,
        } => {
            let key = (workspace_id.clone(), session_id.clone());
            let Some(active_session) = active.get_mut(&key) else {
                return true;
            };
            if active_session.generation != generation {
                return true;
            }
            if event_settles_turn(&event, &active_session.root_name) {
                active_session.turn_running = false;
            }
            if client_connected {
                return transport
                    .send(&ServerMessage::Event {
                        workspace_id,
                        session_id,
                        event: *event,
                    })
                    .await;
            }
            true
        }
        SessionEnvelope::Closed {
            workspace_id,
            session_id,
            generation,
        } => {
            let key = (workspace_id, session_id);
            if active
                .get(&key)
                .is_some_and(|active_session| active_session.generation == generation)
            {
                active.remove(&key);
            }
            true
        }
    }
}

fn any_turn_running(active: &HashMap<(String, String), ActiveSession>) -> bool {
    active.values().any(|session| session.turn_running)
}

async fn shutdown_active_sessions(active: HashMap<(String, String), ActiveSession>) {
    for ((workspace_id, session_id), active_session) in active {
        active_session
            .session
            .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(5)))
            .await;
        info!(workspace_id = %workspace_id, session_id = %session_id, "session shut down");
    }
}

async fn run_dashboard<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: T,
    app: Arc<AppState>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = HashMap::<(String, String), ActiveSession>::new();
    let mut pending = HashMap::<(String, String), PendingOpen>::new();
    let mut next_generation = 1;
    let mut client_connected = true;

    if !send_workspace_catalog(&transport, &app).await {
        return;
    }
    if !send_provider_catalog(&transport, &app).await {
        return;
    }

    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            command = transport.recv(), if client_connected => {
                match command {
                    Some(command) => {
                        client_connected = handle_dashboard_command(
                            &transport,
                            &app,
                            &mut active,
                            &mut pending,
                            &tx,
                            &mut next_generation,
                            command,
                        ).await;
                    }
                    None => {
                        client_connected = false;
                        pending.clear();
                        if !any_turn_running(&active) {
                            break;
                        }
                        info!("dashboard disconnected, waiting for active turns to settle");
                    }
                }
            }
            envelope = rx.recv() => {
                let Some(envelope) = envelope else {
                    break;
                };
                if !handle_session_envelope(&transport, &mut active, envelope, client_connected).await {
                    client_connected = false;
                    pending.clear();
                }
                if !client_connected && !any_turn_running(&active) {
                    break;
                }
            }
        }
    }

    shutdown_active_sessions(active).await;
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

/// Poll the workspace's `AGENTS.md` for content changes and refresh the shared
/// system prompt in place whenever it changes. Polling (over an OS watcher)
/// keeps this dependency-free and robust to editor atomic-saves; the few-second
/// latency is immaterial since the prompt is only read at the start of a turn.
fn spawn_custom_instructions_watcher(
    workspace_id: String,
    workspace_str: String,
    base_prompt: String,
    system_prompt: SharedSystemPrompt,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut last = coda_server::read_custom_instructions(&workspace_str);
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let current = coda_server::read_custom_instructions(&workspace_str);
                    if current != last {
                        last = current;
                        system_prompt.set(build_system_prompt(&workspace_str, &base_prompt));
                        info!(workspace_id = %workspace_id, "AGENTS.md changed, system prompt reloaded");
                    }
                }
            }
        }
    });
}

fn server_config_path() -> PathBuf {
    std::env::var("CODA_SERVER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("coda-server.toml"))
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
    let system_prompt = SharedSystemPrompt::new(build_system_prompt(&workspace_str, &base_prompt));

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
    let agent_team = build_agent_team(
        SystemPrompt::from(system_prompt.clone()),
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

    spawn_custom_instructions_watcher(
        workspace.id.clone(),
        workspace_str.clone(),
        base_prompt,
        system_prompt.clone(),
        shutdown.clone(),
    );

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
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let shutdown = CancellationToken::new();

    let config_path = server_config_path();
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

    let state = Arc::new(AppState {
        providers,
        default_provider,
        shutdown: shutdown.clone(),
        workspaces,
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

    let listen_addr =
        std::env::var("CODA_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_addr).await.unwrap();
    info!("coda_server listening on ws://{listen_addr}/ws");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .unwrap();

    match Arc::try_unwrap(state) {
        Ok(app_state) => {
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
