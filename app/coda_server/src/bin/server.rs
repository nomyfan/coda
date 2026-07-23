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
use coda_core::llm::{LLMProviderConfig, Message, Modality};
use coda_openai::OpenAICompatible;
use coda_server::{
    WorkspaceKnowledge,
    agents::{
        AgentFile, ToolRegistry, build_agent_team, load_agent_files, load_root_agent_file,
        resolve_agent_workspace,
    },
    ask_user::AskUserToolSpec,
    build_available_skills, build_workspace_custom_instructions,
    config::{ToolApprovalConfig, WorkspaceConfig, load_server_config},
    hub::{
        AttachError, AttachSession, CommandOutcome, ConnId, RelayEvent, SessionCommand, SessionHub,
        SessionKey, SessionOpener, SessionRelay,
    },
    mcp::McpServers,
    rpc::{self, RpcError, RpcId, RpcOutgoing},
    storage::{RenameSessionError, SessionModelBinding, WorkspaceStorage, validate_session_id},
    transport::{Transport, WebSocketTransport},
    wire::{
        AddAllowPatternParams, DeleteSessionParams, EventParams, ModelSelection, OpenSessionParams,
        PendingApprovalWire, ProviderCatalog, ProviderInfoWire, RenameSessionParams, ResumeParams,
        SessionName, SessionRef, SessionSummaryWire, SetModelParams, Snapshot, TaskParams,
        WireEvent, WorkspaceCatalog, WorkspaceSummaryWire,
    },
};
use coda_tools::{BuildContext, ToolSpec};
use futures::stream::BoxStream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
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
    relay: Arc<dyn SessionRelay>,
}

/// A constructed provider model entry. `model_id` is the API model name sent in
/// requests; `model_name` is the human-readable label shown in the dashboard.
/// Several entries can share the same `provider` when the config declares
/// multiple models under one provider. `reasoning_efforts` is the list the UI
/// offers; empty means the model has no reasoning controls.
struct ProviderHandle {
    provider: Arc<OpenAICompatible>,
    model_id: String,
    model_name: String,
    context_window: u32,
    max_completion_tokens: Option<u32>,
    /// The configured provider's id.
    provider_id: String,
    /// Effort levels surfaced to the dashboard so it can render reasoning controls.
    reasoning_efforts: Vec<String>,
    /// The model's recommended initial effort level. `None` when unconfigured
    /// (the first entry in `reasoning_efforts` is the implicit default).
    default_reasoning_effort: Option<String>,
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
    agent_models: HashMap<String, AgentModelSelection>,
    approval_config: ToolApprovalConfig,
}

/// A validated per-agent model override: a provider selection key plus the
/// normalized reasoning effort. Resolved to a `ModelProfile` at session open.
#[derive(Clone)]
struct AgentModelSelection {
    provider_id: String,
    reasoning_effort: Option<String>,
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
        reasoning_effort: Option<String>,
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

    fn update_reasoning_effort<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let workspace = self
                .workspaces
                .get(&key.0)
                .ok_or_else(|| format!("unknown workspace '{}'", key.0))?;
            let provider = self
                .providers
                .get(provider_id)
                .ok_or_else(|| format!("unknown provider/model '{provider_id}'"))?;
            workspace
                .storage
                .update_reasoning_effort(
                    &key.1,
                    &provider.provider_id,
                    &provider.model_id,
                    reasoning_effort,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
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
    reasoning_effort: Option<String>,
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
        max_completion_tokens: provider.max_completion_tokens,
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
                max_completion_tokens: handle.max_completion_tokens,
                reasoning_effort: selection.reasoning_effort.clone(),
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

/// Frame and send a server-initiated notification.
async fn send_notify<T: Transport>(transport: &T, method: &str, params: &impl Serialize) -> bool {
    transport.send(&rpc::notify(method, params)).await
}

/// Push one live event to the client as an `event` notification.
async fn send_event<T: Transport>(
    transport: &T,
    workspace_id: String,
    session_id: String,
    event: WireEvent,
) -> bool {
    send_notify(
        transport,
        "event",
        &EventParams {
            workspace_id,
            session_id,
            event,
        },
    )
    .await
}

/// Push an `Error` event describing a failed session open. Used on the
/// notification paths (`resume` promotion, `set_model` retry) where there is no
/// request id to answer against.
async fn send_open_error<T: Transport>(
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
    send_event(
        transport,
        workspace_id.to_string(),
        session_id.to_string(),
        event,
    )
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
                    name: session.name,
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

/// A session's initial reasoning effort: the configured default, the first
/// declared level, or `None` when the model has no reasoning controls.
fn initial_reasoning_effort(provider: &ProviderHandle) -> Option<String> {
    provider
        .default_reasoning_effort
        .clone()
        .or_else(|| provider.reasoning_efforts.first().cloned())
}

/// Normalize a client selection to the values exposed by the provider catalog.
/// Reasoning models use their first configured effort when the client omits one;
/// models without reasoning controls keep `None`.
fn normalize_reasoning_effort(
    configured: &[String],
    reasoning_effort: Option<String>,
) -> Option<Option<String>> {
    match reasoning_effort {
        None => Some(configured.first().cloned()),
        Some(effort) if configured.contains(&effort) => Some(Some(effort)),
        Some(_) => None,
    }
}

/// Resolve and normalize a client-supplied model selection. Invalid selections
/// fall back to the default provider at its initial effort.
fn resolve_selection(
    app: &AppState,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
) -> (String, Option<String>) {
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
    reasoning_effort: Option<String>,
) -> Option<Option<String>> {
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
        let configured = vec!["low".to_string(), "high".to_string()];
        assert_eq!(
            normalize_reasoning_effort(&configured, None),
            Some(Some("low".to_string()))
        );
    }

    #[test]
    fn reasoning_model_accepts_off_and_configured_levels() {
        let configured = vec!["off".to_string(), "low".to_string(), "high".to_string()];
        assert_eq!(
            normalize_reasoning_effort(&configured, Some("off".to_string())),
            Some(Some("off".to_string()))
        );
        assert_eq!(
            normalize_reasoning_effort(&configured, Some("high".to_string())),
            Some(Some("high".to_string()))
        );
    }

    #[test]
    fn reasoning_model_rejects_unconfigured_levels() {
        let configured = vec!["low".to_string()];
        assert_eq!(
            normalize_reasoning_effort(&configured, Some("high".to_string())),
            None
        );
        assert_eq!(
            normalize_reasoning_effort(&[], Some("off".to_string())),
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
            default_reasoning_effort: handle.default_reasoning_effort.clone(),
            input_modalities: handle.input_modalities.clone(),
        })
        .collect();
    infos.sort_by(|a, b| a.id.cmp(&b.id));
    infos
}

async fn send_pending_approval_events<T: Transport>(
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
        if !send_event(
            transport,
            workspace_id.to_string(),
            session_id.to_string(),
            event,
        )
        .await
        {
            return false;
        }
    }
    true
}

struct Selection {
    provider_id: String,
    reasoning_effort: Option<String>,
}

/// Attach to `key` in the hub, register the replay+live stream, and remember the
/// model selection the session actually uses so a hub-initiated close can
/// re-attach transparently. On success it *returns* the [`Snapshot`] payload
/// rather than sending it, so the caller frames it either as an `open_session`
/// request `result` (solicited) or a `snapshot` notification (unsolicited hub
/// re-attach) — the only difference between the two paths is the envelope.
#[allow(clippy::too_many_arguments)]
async fn attach_core(
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, Selection>,
    key: SessionKey,
    provider_id: String,
    reasoning_effort: Option<String>,
    takeover: bool,
) -> Result<Snapshot, AttachError> {
    let AttachSession { snapshot, events } = app
        .relay
        .attach(
            key.clone(),
            conn_id,
            provider_id,
            reasoning_effort,
            takeover,
        )
        .await?;
    selections.insert(
        key.clone(),
        Selection {
            provider_id: snapshot.provider_id.clone(),
            reasoning_effort: snapshot.reasoning_effort.clone(),
        },
    );
    let wire_snapshot = Snapshot {
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
    };
    // Register the stream *before* returning; the caller sends the snapshot
    // (result or notification) before the connection loop next polls the stream,
    // so the snapshot always precedes the replayed events.
    streams.insert(key, events);
    Ok(wire_snapshot)
}

/// Deserialize a request's `params` into the per-method type, mapping a failure
/// to a `-32602` invalid-params error.
fn parse_params<P: DeserializeOwned>(params: Value) -> Result<P, RpcError> {
    serde_json::from_value(params).map_err(|err| RpcError::invalid_params(err.to_string()))
}

/// Classify one decoded frame and act on it. A *request* always produces exactly
/// one framed reply (`result` or `error`); a *notification* runs for effect and
/// is never answered (an unknown method or bad params on a notification is
/// logged and dropped — there is no id to answer against); a structurally
/// invalid frame is answered with the recovered id (or `null`). Returns `false`
/// when the transport is gone and the connection should tear down.
async fn handle_frame<T: Transport>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, Selection>,
    frame: String,
) -> bool {
    match rpc::decode(&frame) {
        rpc::Incoming::Request { id, method, params } => {
            let reply =
                dispatch_request(app, conn_id, streams, selections, id, &method, params).await;
            transport.send(&reply).await
        }
        rpc::Incoming::Notification { method, params } => {
            dispatch_notification(
                transport, app, conn_id, streams, selections, &method, params,
            )
            .await
        }
        rpc::Incoming::Invalid { id, error } => {
            let reply: RpcOutgoing = (id, error).into();
            transport.send(&reply).await
        }
    }
}

/// Handle a request and build its single reply. Every arm returns exactly one
/// `RpcOutgoing` — a `result` or a typed `error`; no request path drops silently.
async fn dispatch_request(
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, Selection>,
    id: RpcId,
    method: &str,
    params: Value,
) -> RpcOutgoing {
    match method {
        "list_workspaces" => (
            id,
            &WorkspaceCatalog {
                workspaces: workspace_catalog(app).await,
            },
        )
            .into(),
        "list_providers" => (
            id,
            &ProviderCatalog {
                providers: provider_infos(app),
                default_provider: app.default_provider.clone(),
            },
        )
            .into(),
        "open_session" => {
            let params: OpenSessionParams = match parse_params(params) {
                Ok(params) => params,
                Err(err) => return (id, err).into(),
            };
            if let Err(err) = validate_session_id(&params.session_id) {
                return (
                    id,
                    RpcError::with_detail(rpc::INVALID_SESSION_ID, "invalid session id", err),
                )
                    .into();
            }
            let Some(workspace) = app.workspaces.get(&params.workspace_id) else {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::UNKNOWN_WORKSPACE,
                        "unknown workspace",
                        params.workspace_id,
                    ),
                )
                    .into();
            };
            // Resolve the client selection only for first creation. Existing
            // sessions reopen with their durable binding, regardless of the
            // browser's latest workspace preference.
            let (requested_provider_id, requested_reasoning_effort) =
                resolve_selection(app, params.provider_id, params.reasoning_effort);
            let requested_provider = app
                .providers
                .get(&requested_provider_id)
                .expect("resolved provider selection exists");
            let initialized = match workspace
                .storage
                .initialize_session(
                    &params.session_id,
                    SessionModelBinding {
                        provider_id: requested_provider.provider_id.clone(),
                        model_id: requested_provider.model_id.clone(),
                        reasoning_effort: requested_reasoning_effort,
                    },
                )
                .await
            {
                Ok(initialized) => initialized,
                Err(err) => {
                    return (
                        id,
                        RpcError::with_detail(
                            rpc::OPEN_FAILED,
                            "failed to initialize session",
                            err.to_string(),
                        ),
                    )
                        .into();
                }
            };
            let provider_id = initialized.metadata.binding.selection_key();
            let Some(reasoning_effort) = normalize_provider_selection(
                app,
                &provider_id,
                initialized.metadata.binding.reasoning_effort,
            ) else {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::OPEN_FAILED,
                        "session model binding is unavailable",
                        provider_id,
                    ),
                )
                    .into();
            };
            let key = (params.workspace_id, params.session_id);
            match attach_core(
                app,
                conn_id,
                streams,
                selections,
                key,
                provider_id,
                reasoning_effort,
                params.takeover,
            )
            .await
            {
                Ok(snapshot) => (id, &snapshot).into(),
                // Held by another client: the client asks the user, then retries
                // with takeover. Nothing changed server-side.
                Err(AttachError::Busy) => (
                    id,
                    RpcError::new(rpc::SESSION_BUSY, "session is held by another client"),
                )
                    .into(),
                Err(AttachError::Open(err)) => (
                    id,
                    RpcError::with_detail(
                        rpc::OPEN_FAILED,
                        "failed to open session",
                        err.to_string(),
                    ),
                )
                    .into(),
            }
        }
        "set_model" => {
            let params: SetModelParams = match parse_params(params) {
                Ok(params) => params,
                Err(err) => return (id, err).into(),
            };
            // Invalid selections are caught here, before the hub — `OpenError`
            // has no "bad model" variant (Decision 8).
            let Some(reasoning_effort) =
                normalize_provider_selection(app, &params.provider_id, params.reasoning_effort)
            else {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::INVALID_MODEL_SELECTION,
                        "invalid model selection",
                        params.provider_id,
                    ),
                )
                    .into();
            };
            let key = (params.workspace_id, params.session_id);
            let requested_provider = params.provider_id.clone();
            let requested_effort = reasoning_effort.clone();
            match app
                .relay
                .command(
                    key.clone(),
                    conn_id,
                    SessionCommand::SetModel {
                        provider_id: params.provider_id,
                        reasoning_effort,
                    },
                )
                .await
            {
                CommandOutcome::ModelChanged {
                    provider_id,
                    reasoning_effort,
                } => {
                    // Keep the re-attach cache on the new selection so a
                    // hub-initiated close doesn't reopen on the old model.
                    selections.insert(
                        key,
                        Selection {
                            provider_id: provider_id.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                        },
                    );
                    (
                        id,
                        &ModelSelection {
                            provider_id,
                            reasoning_effort,
                        },
                    )
                        .into()
                }
                // Already the selected model: idempotent success echoing it back.
                CommandOutcome::Unchanged => (
                    id,
                    &ModelSelection {
                        provider_id: requested_provider,
                        reasoning_effort: requested_effort,
                    },
                )
                    .into(),
                CommandOutcome::TurnRunning => (
                    id,
                    RpcError::new(
                        rpc::MODEL_SWITCH_WHILE_RUNNING,
                        "cannot switch model while a turn is running",
                    ),
                )
                    .into(),
                CommandOutcome::ModelLocked => (
                    id,
                    RpcError::new(
                        rpc::MODEL_LOCKED,
                        "provider/model is locked for this session",
                    ),
                )
                    .into(),
                CommandOutcome::PersistenceFailed(detail) => (
                    id,
                    RpcError::with_detail(
                        rpc::OPEN_FAILED,
                        "failed to persist reasoning effort",
                        detail,
                    ),
                )
                    .into(),
                CommandOutcome::OpenFailed(err) => (
                    id,
                    RpcError::with_detail(
                        rpc::OPEN_FAILED,
                        "failed to switch model",
                        err.to_string(),
                    ),
                )
                    .into(),
                // Residual `Ignored` — the stale/not-attached guard or the
                // non-`Live` phase, both meaning "not live" (Decision 8).
                _ => (
                    id,
                    RpcError::new(rpc::SESSION_NOT_LIVE, "session is not live"),
                )
                    .into(),
            }
        }
        "add_allow_pattern" => {
            let params: AddAllowPatternParams = match parse_params(params) {
                Ok(params) => params,
                Err(err) => return (id, err).into(),
            };
            let Some(workspace) = app.workspaces.get(&params.workspace_id) else {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::UNKNOWN_WORKSPACE,
                        "unknown workspace",
                        params.workspace_id,
                    ),
                )
                    .into();
            };
            match add_allow_pattern(workspace.approval_config.clone(), params.pattern).await {
                Ok(()) => (id, &serde_json::json!({})).into(),
                Err(message) => (
                    id,
                    RpcError::with_detail(
                        rpc::ALLOW_PATTERN_FAILED,
                        "failed to add allow pattern",
                        message,
                    ),
                )
                    .into(),
            }
        }
        "delete_session" => {
            let params: DeleteSessionParams = match parse_params(params) {
                Ok(params) => params,
                Err(err) => return (id, err).into(),
            };
            if let Err(err) = validate_session_id(&params.session_id) {
                return (
                    id,
                    RpcError::with_detail(rpc::INVALID_SESSION_ID, "invalid session id", err),
                )
                    .into();
            }
            if !app.workspaces.contains_key(&params.workspace_id) {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::UNKNOWN_WORKSPACE,
                        "unknown workspace",
                        params.workspace_id,
                    ),
                )
                    .into();
            }
            let key = (params.workspace_id.clone(), params.session_id.clone());
            // Stop the runtime before removing its files so no checkpoint is
            // written back after deletion. Refused when another connection is
            // attached — a stale client must not erase work someone else is
            // driving (the persisted state stays too).
            if !app.relay.delete(key.clone(), conn_id).await {
                return (
                    id,
                    RpcError::new(rpc::NOT_OWNER, "another client is driving this session"),
                )
                    .into();
            }
            // Drop our own stream (the hub evicted our attachment, if any).
            streams.remove(&key);
            selections.remove(&key);
            let workspace = app
                .workspaces
                .get(&params.workspace_id)
                .expect("workspace presence checked above");
            if let Err(err) = workspace.storage.delete_session(&params.session_id).await {
                warn!(workspace_id = %params.workspace_id, session_id = %params.session_id, "failed to delete session: {err}");
                return (
                    id,
                    RpcError::with_detail(rpc::DELETE_FAILED, "failed to delete session", err),
                )
                    .into();
            }
            // The catalog is authoritative only after a durable delete.
            (
                id,
                &WorkspaceCatalog {
                    workspaces: workspace_catalog(app).await,
                },
            )
                .into()
        }
        "rename_session" => {
            let params: RenameSessionParams = match parse_params(params) {
                Ok(params) => params,
                Err(err) => return (id, err).into(),
            };
            let Some(workspace) = app.workspaces.get(&params.workspace_id) else {
                return (
                    id,
                    RpcError::with_detail(
                        rpc::UNKNOWN_WORKSPACE,
                        "unknown workspace",
                        params.workspace_id,
                    ),
                )
                    .into();
            };
            match workspace
                .storage
                .rename_session(&params.session_id, params.name.as_deref())
                .await
            {
                Ok(name) => (id, &SessionName { name }).into(),
                Err(RenameSessionError::InvalidSessionId(detail)) => (
                    id,
                    RpcError::with_detail(rpc::INVALID_SESSION_ID, "invalid session id", detail),
                )
                    .into(),
                Err(RenameSessionError::InvalidName(message)) => {
                    (id, RpcError::new(rpc::INVALID_PARAMS, message)).into()
                }
                Err(RenameSessionError::SessionNotFound) => (
                    id,
                    RpcError::new(rpc::SESSION_NOT_FOUND, "session not found"),
                )
                    .into(),
                Err(RenameSessionError::Persistence(detail)) => {
                    warn!(
                        workspace_id = %params.workspace_id,
                        session_id = %params.session_id,
                        "failed to rename session: {detail}"
                    );
                    (
                        id,
                        RpcError::with_detail(
                            rpc::RENAME_FAILED,
                            "failed to rename session",
                            detail,
                        ),
                    )
                        .into()
                }
            }
        }
        other => (id, RpcError::method_not_found(other)).into(),
    }
}

/// Run a notification for effect. No reply is ever produced; a bad method or
/// bad params is logged and dropped. Returns `false` when a mid-handler push
/// found the transport gone.
async fn dispatch_notification<T: Transport>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    streams: &mut StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: &mut HashMap<SessionKey, Selection>,
    method: &str,
    params: Value,
) -> bool {
    match method {
        "task" => match parse_params::<TaskParams>(params) {
            Ok(params) => handle_task(transport, app, conn_id, params).await,
            Err(err) => {
                warn!("ignoring malformed task notification: {}", err.message);
                true
            }
        },
        "resume" => match parse_params::<ResumeParams>(params) {
            Ok(params) => handle_resume(transport, app, conn_id, params).await,
            Err(err) => {
                warn!("ignoring malformed resume notification: {}", err.message);
                true
            }
        },
        "abort" => match parse_params::<SessionRef>(params) {
            Ok(params) => {
                app.relay
                    .command(
                        (params.workspace_id, params.session_id),
                        conn_id,
                        SessionCommand::Abort,
                    )
                    .await;
                true
            }
            Err(err) => {
                warn!("ignoring malformed abort notification: {}", err.message);
                true
            }
        },
        "close_session" => match parse_params::<SessionRef>(params) {
            Ok(params) => {
                let key = (params.workspace_id, params.session_id);
                streams.remove(&key);
                selections.remove(&key);
                // The hub keeps the session alive while a turn is in flight and
                // releases it once idle — no per-connection deferral needed.
                app.relay.detach(key, conn_id).await;
                true
            }
            Err(err) => {
                warn!(
                    "ignoring malformed close_session notification: {}",
                    err.message
                );
                true
            }
        },
        other => {
            warn!("ignoring unknown notification method: {other}");
            true
        }
    }
}

/// Start a new turn. Rejects image input when the active model doesn't accept it
/// (the frontend disables it, so reaching here is a UI bypass — surface an error
/// event rather than silently dropping the attachments).
async fn handle_task<T: Transport>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    params: TaskParams,
) -> bool {
    let key = (params.workspace_id.clone(), params.session_id.clone());
    let accepts_images = match app.relay.provider_of(key.clone()).await {
        Some(provider_id) => app
            .providers
            .get(&provider_id)
            .is_some_and(|handle| handle.input_modalities.contains(&Modality::Image)),
        None => return true, // no live/pending session for this key
    };
    if !accepts_images && !params.images.is_empty() {
        let event = WireEvent::Error {
            agent_name: String::new(),
            thread_id: params.session_id.clone(),
            message: "the selected model does not accept image input".to_string(),
        };
        return send_event(transport, params.workspace_id, params.session_id, event).await;
    }
    let images = sanitize_task_images(params.images);
    let task = params.task.trim().to_string();
    if task.is_empty() && images.is_empty() {
        return true;
    }
    app.relay
        .command(key, conn_id, SessionCommand::Task { task, images })
        .await;
    true
}

/// Answer a suspended tool call. Any follow-up (more pending approvals, an open
/// failure) streams back as `event` notifications; there is no request id here.
async fn handle_resume<T: Transport>(
    transport: &T,
    app: &Arc<AppState>,
    conn_id: ConnId,
    params: ResumeParams,
) -> bool {
    let key = (params.workspace_id.clone(), params.session_id.clone());
    match app
        .relay
        .command(
            key,
            conn_id,
            SessionCommand::Resume {
                agent_name: params.agent_name,
                thread_id: params.thread_id,
                decision: params.decision,
            },
        )
        .await
    {
        CommandOutcome::StillPending(more) => {
            send_pending_approval_events(transport, &params.workspace_id, &params.session_id, &more)
                .await
        }
        CommandOutcome::OpenFailed(err) => {
            send_open_error(transport, &params.workspace_id, &params.session_id, err).await;
            true
        }
        _ => true,
    }
}

/// Connection ids distinguish clients inside the hub (latest-wins eviction and
/// stale-command rejection). Monotonic per process.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

async fn run_connection<T: Transport>(transport: T, app: Arc<AppState>) {
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    // Event streams of the sessions this connection is attached to. A stream
    // that ends silently (detach/takeover elsewhere) is dropped from the map
    // automatically.
    let mut streams: StreamMap<SessionKey, BoxStream<'static, RelayEvent>> = StreamMap::new();
    // The model selection each attached session actually uses, so a
    // hub-initiated close can re-attach with the same model.
    let mut selections: HashMap<SessionKey, Selection> = HashMap::new();
    // Keys re-attached after a `Closed` that have produced no event since:
    // a session that closes again right away is not retried (no reopen loop).
    let mut reattached: std::collections::HashSet<SessionKey> = std::collections::HashSet::new();

    // No eager catalog pushes: the client requests `list_workspaces` and
    // `list_providers` on connect and applies the results at the call site.

    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            frame = transport.recv() => {
                match frame {
                    Some(frame) => {
                        if !handle_frame(
                            &transport,
                            &app,
                            conn_id,
                            &mut streams,
                            &mut selections,
                            frame,
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
                        send_event(&transport, key.0.clone(), key.1.clone(), *event).await
                    }
                    RelayEvent::Evicted => {
                        streams.remove(&key);
                        selections.remove(&key);
                        reattached.remove(&key);
                        send_notify(&transport, "session_evicted", &SessionRef {
                            workspace_id: key.0,
                            session_id: key.1,
                        }).await
                    }
                    // The hub ended this stream: a forced resync (lag or
                    // buffer overflow) or the runtime terminating. Re-attach
                    // with a fresh snapshot instead of leaving the client
                    // without a signal; the hub's release barrier makes the
                    // fresh attach wait for the final checkpoint. Guarded to
                    // one silent retry so a session that dies on open can't
                    // loop, and skipped during process shutdown.
                    RelayEvent::Closed => {
                        streams.remove(&key);
                        let selection = selections.remove(&key);
                        if let Some(Selection {
                            provider_id,
                            reasoning_effort,
                        }) = selection
                            && !app.shutdown.is_cancelled()
                            && reattached.insert(key.clone())
                        {
                            info!(workspace_id = %key.0, session_id = %key.1, "session closed by hub; re-attaching");
                            // No takeover: if another connection got there
                            // first the client is told it lost drive rights
                            // (`session_evicted`) rather than silently evicting
                            // whoever now holds it.
                            match attach_core(
                                &app,
                                conn_id,
                                &mut streams,
                                &mut selections,
                                key.clone(),
                                provider_id,
                                reasoning_effort,
                                false,
                            ).await {
                                Ok(snapshot) => send_notify(&transport, "snapshot", &snapshot).await,
                                Err(AttachError::Busy) => send_notify(&transport, "session_evicted", &SessionRef {
                                    workspace_id: key.0,
                                    session_id: key.1,
                                }).await,
                                Err(AttachError::Open(err)) => {
                                    send_open_error(&transport, &key.0, &key.1, err).await;
                                    true
                                }
                            }
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

    app.relay.detach_all(conn_id).await;
}

async fn connection_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        info!("connection opened");
        run_connection(WebSocketTransport::new(socket), state).await;
        info!("connection closed");
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
    knowledge: WorkspaceKnowledge,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut last_skills = knowledge.available_skills.get();
        let mut last_instructions = knowledge.custom_instructions.get();
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let skills = build_available_skills(&workspace_str);
                    if skills != last_skills {
                        last_skills = skills.clone();
                        knowledge.available_skills.set(skills);
                        info!(workspace_id = %workspace_id, path = %workspace_str, "workspace skills changed, reloaded");
                    }
                    let instructions = build_workspace_custom_instructions(&workspace_str);
                    if instructions != last_instructions {
                        last_instructions = instructions.clone();
                        knowledge.custom_instructions.set(instructions);
                        info!(workspace_id = %workspace_id, path = %workspace_str, "workspace custom instructions changed, reloaded");
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
) -> Result<HashMap<String, AgentModelSelection>, String> {
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
            AgentModelSelection {
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

    // One set of workspace-knowledge handles (and one watcher) per distinct
    // workspace, shared by every agent rooted there. The watcher refreshes the
    // skills/AGENTS.md text in place; the vars provider reads it every turn.
    let distinct: BTreeSet<String> = std::iter::once(workspace_str.clone())
        .chain(agent_workspaces.values().cloned())
        .collect();
    let mut knowledge: HashMap<String, WorkspaceKnowledge> = HashMap::new();
    for ws in distinct {
        let handles = WorkspaceKnowledge::load(&ws);
        spawn_workspace_knowledge_watcher(
            workspace.id.clone(),
            ws.clone(),
            handles.clone(),
            shutdown.clone(),
        );
        knowledge.insert(ws, handles);
    }

    let agent_team = build_agent_team(
        &workspace_str,
        root_base,
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
            let shared_provider = Arc::new(OpenAICompatible::new(
                LLMProviderConfig {
                    api_key: p.api_key,
                    base_url: p.base_url,
                    include_usage: p.include_usage,
                },
                p.kind,
                p.id.clone(),
            ));
            p.models.into_iter().map(move |m| {
                let id = format!("{}:{}", p.id, m.id);
                let handle = ProviderHandle {
                    provider: shared_provider.clone(),
                    model_id: m.id,
                    model_name: m.name,
                    context_window: m.context_window,
                    max_completion_tokens: m.max_completion_tokens,
                    provider_id: p.id.clone(),
                    reasoning_efforts: m.reasoning_efforts,
                    default_reasoning_effort: m.default_reasoning_effort,
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

    // The relay owns live sessions process-wide; its opener holds its own map
    // clones so it exists independently of `AppState`.
    let relay: Arc<dyn SessionRelay> = Arc::new(SessionHub::new(
        Arc::new(AppOpener {
            providers: providers.clone(),
            workspaces: workspaces.clone(),
        }),
        server_config.relay,
    ));

    let state = Arc::new(AppState {
        providers,
        default_provider,
        shutdown: shutdown.clone(),
        workspaces,
        relay,
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
        .route("/ws", get(connection_ws_handler))
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
    state.relay.shutdown_all().await;

    match Arc::try_unwrap(state) {
        Ok(app_state) => {
            // Drop the hub first: its opener holds workspace references that
            // would otherwise keep the `Arc::try_unwrap` below from succeeding.
            drop(app_state.relay);
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
