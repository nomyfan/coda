use axum::{
    Router,
    extract::{Path, State, ws::WebSocketUpgrade},
    response::Response,
    routing::get,
};
use coda_agent::{
    OpenError, ResumeDecision, RunConfig, Session, Shutdown, runtime::SessionStorage,
};
use coda_core::llm::LLMProviderConfig;
use coda_openai::OpenAI;
use coda_server::{
    ask_user::AskUserToolSpec,
    build_agent_spec, build_system_prompt,
    config::ToolApprovalConfig,
    mcp::McpServers,
    storage::JsonFileStorage,
    transport::{Transport, WebSocketTransport},
    wire::{ClientMessage, ServerMessage, WireEvent},
};
use coda_tools::{BuildContext, PrebuiltToolSpec, ToolSpec};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

struct AppState {
    storage: JsonFileStorage,
    system_prompt: String,
    workspace_str: String,
    provider: Arc<OpenAI>,
    model: String,
    mcp_servers: McpServers,
    approval_config: ToolApprovalConfig,
    /// Cancelled on process shutdown signal; parent of every connection token.
    shutdown: CancellationToken,
    /// Cancellation handle of the current connection, if any. Enforces the
    /// single-client invariant by latest-wins: a new connection cancels it.
    current: Mutex<Option<CancellationToken>>,
}

/// Open (or resume) the session for `session_id`, seeding it with the built-in
/// tools, MCP tools, and approval policy. `decisions` covers any pending
/// approvals carried over from a prior suspension.
async fn open_session(
    state: &AppState,
    session_id: &str,
    decisions: HashMap<String, ResumeDecision>,
) -> Result<Session, OpenError> {
    let config = RunConfig {
        provider: state.provider.clone(),
        model: state.model.clone(),
        max_completion_tokens: Some(5000),
        temperature: Some(0.7),
        tool_approval: state.approval_config.clone().into_approval_mode(),
        approval_timeout: Some(Duration::from_secs(300)),
    };

    let mut extra_tools: Vec<Box<dyn ToolSpec>> = vec![Box::new(AskUserToolSpec)];
    extra_tools.extend(
        state
            .mcp_servers
            .tool_objects()
            .into_iter()
            .map(|t| Box::new(PrebuiltToolSpec::new(t)) as Box<dyn ToolSpec>),
    );
    let spec = build_agent_spec(state.system_prompt.clone(), extra_tools);
    let ctx = BuildContext::new(state.workspace_str.clone());

    Session::builder()
        .storage(state.storage.clone())
        .root(spec)
        .build_context(ctx)
        .run_config(config)
        .session_id(session_id)
        .resume_decisions(decisions)
        .open()
        .await
}

async fn add_allow_pattern(config: ToolApprovalConfig, pattern: String) {
    match tokio::task::spawn_blocking(move || config.add_allow_pattern(&pattern)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("failed to add allow pattern: {e}"),
        Err(e) => warn!("failed to join allow-pattern writer: {e}"),
    }
}

/// Apply a client command to the live session.
async fn dispatch(session: &Session, state: &Arc<AppState>, msg: ClientMessage) {
    match msg {
        ClientMessage::Task { task } => {
            if let Err(e) = session.send(task).await {
                warn!("failed to send task: {e}");
            }
        }
        ClientMessage::Resume {
            agent_name,
            thread_id,
            decision,
        } => {
            if let Err(e) = session.resume(&agent_name, &thread_id, decision).await {
                warn!("failed to resume: {e}");
            }
        }
        ClientMessage::Abort => session.abort().await,
        ClientMessage::AddAllowPattern { pattern } => {
            add_allow_pattern(state.approval_config.clone(), pattern).await;
        }
    }
}

/// Read `Resume` commands until every pending thread is covered. Allow-pattern
/// commands sent alongside (e.g. choosing "always") are persisted in place.
/// Returns `None` if the client disconnects, or if `cancel` fires (eviction or
/// shutdown); bailing here keeps an evicted client from opening a second
/// session by sending late decisions.
async fn collect_resume_decisions<
    T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>,
>(
    transport: &T,
    approval_config: ToolApprovalConfig,
    pending: &[coda_agent::PendingApproval],
    cancel: &CancellationToken,
) -> Option<HashMap<String, ResumeDecision>> {
    let mut needed: HashSet<String> = pending.iter().map(|p| p.thread_id.clone()).collect();
    let mut decisions: HashMap<String, ResumeDecision> = HashMap::new();
    while !needed.is_empty() {
        let msg = tokio::select! {
            _ = cancel.cancelled() => return None,
            msg = transport.recv() => msg?,
        };
        match msg {
            ClientMessage::Resume {
                thread_id,
                decision,
                ..
            } => {
                needed.remove(&thread_id);
                decisions.insert(thread_id, decision);
            }
            ClientMessage::AddAllowPattern { pattern } => {
                add_allow_pattern(approval_config.clone(), pattern).await;
            }
            _ => {} // ignore task/abort while resolving
        }
    }
    Some(decisions)
}

/// Send an `Error` event describing a failed session open.
async fn send_open_error<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: &T,
    session_id: &str,
    err: OpenError,
) {
    let event = WireEvent::Error {
        agent_name: String::new(),
        thread_id: session_id.to_string(),
        message: format!("failed to open session: {err}"),
    };
    transport.send(&ServerMessage::Event { event }).await;
}

/// Drive a single client session over `transport`: send a snapshot, resolve any
/// carried-over pending approvals, then pump commands in and runtime events out
/// for the connection's lifetime. Transport-agnostic.
async fn handle_session<T: Transport<Incoming = ClientMessage, Outgoing = ServerMessage>>(
    transport: T,
    state: Arc<AppState>,
    session_id: String,
    cancel: CancellationToken,
) {
    // 1. First open attempt, used both to bring the session up and to discover
    //    whether it resumed into a pending approval.
    let first = open_session(&state, &session_id, HashMap::new()).await;
    let pending = match &first {
        Err(OpenError::PendingApprovalsRequired(p)) => p.clone(),
        _ => Vec::new(),
    };

    // 2. Send the snapshot: resumed history plus any pending approvals the
    //    client must answer before the session can resume.
    let messages = state
        .storage
        .load_checkpoint(&session_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.messages)
        .unwrap_or_default();
    if !transport
        .send(&ServerMessage::Snapshot {
            session_id: session_id.clone(),
            messages,
            pending_approvals: pending.clone(),
        })
        .await
    {
        return;
    }

    // 3. Resolve into a live session, collecting `Resume` decisions for any
    //    pending approvals (re-prompting via events should the runtime suspend
    //    again before fully resuming).
    let session = match first {
        Ok(s) => s,
        Err(OpenError::PendingApprovalsRequired(_)) => {
            let Some(mut decisions) = collect_resume_decisions(
                &transport,
                state.approval_config.clone(),
                &pending,
                &cancel,
            )
            .await
            else {
                return;
            };
            loop {
                match open_session(&state, &session_id, std::mem::take(&mut decisions)).await {
                    Ok(s) => break s,
                    Err(OpenError::PendingApprovalsRequired(more)) => {
                        for p in &more {
                            let event = WireEvent::Suspended {
                                agent_name: p.agent_name.clone(),
                                thread_id: p.thread_id.clone(),
                                approval: p.clone(),
                            };
                            if !transport.send(&ServerMessage::Event { event }).await {
                                return;
                            }
                        }
                        match collect_resume_decisions(
                            &transport,
                            state.approval_config.clone(),
                            &more,
                            &cancel,
                        )
                        .await
                        {
                            Some(d) => decisions = d,
                            None => return,
                        }
                    }
                    Err(e) => {
                        send_open_error(&transport, &session_id, e).await;
                        return;
                    }
                }
            }
        }
        Err(e) => {
            send_open_error(&transport, &session_id, e).await;
            return;
        }
    };
    let root_name = session.root_name().to_string();
    info!(session_id = %session_id, "session opened");

    // 4. Pump: client commands in, runtime events out, for the connection's life.
    //    The cancel token fires when a newer connection evicts us or the process
    //    is shutting down (it is a child of the global shutdown token).
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            cmd = transport.recv() => {
                match cmd {
                    Some(msg) => dispatch(&session, &state, msg).await,
                    None => break, // client disconnected
                }
            }
            event = session.recv() => {
                match event {
                    Some(ev) => {
                        let event = WireEvent::from_session_event(ev, &root_name);
                        if !transport.send(&ServerMessage::Event { event }).await {
                            break;
                        }
                    }
                    None => break, // runtime shut down
                }
            }
        }
    }

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(5)))
        .await;
    info!(session_id = %session_id, "connection closed, session shut down");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    // Latest-wins: install this connection's token and cancel any predecessor so
    // its session is torn down. The token is a child of the global shutdown
    // token, so a process signal cancels it too.
    let cancel = state.shutdown.child_token();
    if let Some(previous) = state.current.lock().await.replace(cancel.clone()) {
        previous.cancel();
        info!("evicting previous client connection");
    }
    info!(session_id = %session_id, "client connected");
    ws.on_upgrade(move |socket| {
        handle_session(WebSocketTransport::new(socket), state, session_id, cancel)
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

    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let base_url = std::env::var("OPENAI_BASE_URL").expect("OPENAI_BASE_URL must be set");
    let model = std::env::var("OPENAI_MODEL").expect("OPENAI_MODEL must be set");

    let workspace_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let workspace_str = workspace_dir.to_string_lossy().into_owned();
    let system_prompt = build_system_prompt(&workspace_str);

    let provider = Arc::new(OpenAI::new(LLMProviderConfig {
        api_key,
        base_url,
        stream: true,
    }));

    let checkpoint_dir = workspace_dir.join(".coda").join("sessions");
    let storage = JsonFileStorage::new(checkpoint_dir);

    let mcp_servers = coda_server::mcp::load_mcp_servers(&workspace_dir)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("failed to load MCP servers: {e}");
            McpServers::empty()
        });

    let approval_config = ToolApprovalConfig::load(&workspace_dir).unwrap_or_else(|e| {
        tracing::warn!("failed to load approval config: {e}");
        ToolApprovalConfig::default_for(&workspace_dir)
    });

    let shutdown = CancellationToken::new();
    let state = Arc::new(AppState {
        storage,
        system_prompt,
        workspace_str,
        provider,
        model,
        mcp_servers,
        approval_config,
        shutdown: shutdown.clone(),
        current: Mutex::new(None),
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
        .route("/ws/{session_id}", get(ws_handler))
        .with_state(state.clone());

    let listen_addr =
        std::env::var("CODA_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_addr).await.unwrap();
    info!("coda_server listening on ws://{listen_addr}/ws/:session_id");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .unwrap();

    match Arc::try_unwrap(state) {
        Ok(app_state) => app_state.mcp_servers.shutdown().await,
        Err(_) => warn!("cannot shutdown MCP servers: outstanding references"),
    }

    info!("server stopped");
}
