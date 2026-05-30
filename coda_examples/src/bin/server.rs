use axum::http::StatusCode;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use coda_agent::{AgentEvent, OpenError, RunConfig, Session, Shutdown, runtime::SessionStorage};
use coda_core::llm::LLMProviderConfig;
use coda_examples::{
    build_agent_spec, build_system_prompt,
    config::ToolApprovalConfig,
    mcp::McpServers,
    storage::JsonFileStorage,
    wire::{
        AddAllowPatternRequest, ChatRequest, ChatResponse, ChatStatus, HistoryResponse, WireEvent,
    },
};
use coda_openai::OpenAI;
use coda_tools::{BuildContext, PrebuiltToolSpec, ToolSpec};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

struct AppState {
    storage: JsonFileStorage,
    system_prompt: String,
    workspace_str: String,
    provider: Arc<OpenAI>,
    model: String,
    mcp_servers: McpServers,
    approval_config: ToolApprovalConfig,
}

async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, String> {
    info!(
        session_id = %req.session_id,
        has_task = req.task.is_some(),
        decisions = req.resume_decisions.len(),
        "incoming request"
    );

    let config = RunConfig {
        provider: state.provider.clone(),
        model: state.model.clone(),
        max_completion_tokens: Some(5000),
        temperature: Some(0.7),
        tool_approval: state.approval_config.clone().into_approval_mode(),
        approval_timeout: Some(Duration::from_secs(300)),
    };

    let mcp_tool_specs: Vec<Box<dyn ToolSpec>> = state
        .mcp_servers
        .tool_objects()
        .into_iter()
        .map(|t| Box::new(PrebuiltToolSpec::new(t)) as Box<dyn ToolSpec>)
        .collect();
    let spec = build_agent_spec(state.system_prompt.clone(), mcp_tool_specs);
    let ctx = BuildContext::new(state.workspace_str.clone());

    let session = match Session::builder()
        .storage(state.storage.clone())
        .root(spec)
        .build_context(ctx)
        .run_config(config)
        .session_id(&req.session_id)
        .resume_decisions(req.resume_decisions)
        .open()
        .await
    {
        Ok(s) => s,
        Err(OpenError::PendingApprovalsRequired(pending)) => {
            return Ok(Json(ChatResponse {
                status: ChatStatus::PendingApproval,
                events: vec![],
                pending_approvals: pending,
            }));
        }
        Err(e) => {
            return Ok(Json(ChatResponse {
                status: ChatStatus::Error(format!("failed to open session: {e}")),
                events: vec![],
                pending_approvals: vec![],
            }));
        }
    };

    info!("session opened, consuming events...");

    if let Some(task) = &req.task
        && session.send(task.clone()).await.is_err()
    {
        let _ = session.shutdown(Shutdown::Abort).await;
        return Ok(Json(ChatResponse {
            status: ChatStatus::Error("failed to send task".into()),
            events: vec![],
            pending_approvals: vec![],
        }));
    }

    let mut events: Vec<WireEvent> = Vec::new();
    let mut pending_approvals: Vec<coda_agent::PendingApproval> = Vec::new();
    let mut status = ChatStatus::Done;
    let root_name = session.root_name().to_string();

    while let Some(event) = session.recv().await {
        let is_terminal = match &event.kind {
            AgentEvent::Suspended(pending) => {
                pending_approvals.push(pending.clone());
                status = ChatStatus::PendingApproval;
                true
            }
            AgentEvent::LLMEnd(msg) if event.origin.is_root() && msg.tool_calls.is_empty() => true,
            _ => false,
        };
        events.push(WireEvent::from_session_event(event, &root_name));
        if is_terminal {
            break;
        }
    }

    session
        .shutdown(Shutdown::graceful(Duration::from_secs(5)))
        .await;

    info!(
        events = events.len(),
        pending = pending_approvals.len(),
        ?status,
        "request complete"
    );

    Ok(Json(ChatResponse {
        status,
        events,
        pending_approvals,
    }))
}

async fn history_handler(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<HistoryResponse>, String> {
    let checkpoint = state
        .storage
        .load_checkpoint(&session_id)
        .await
        .map_err(|e| format!("storage error: {e}"))?;

    let messages = checkpoint.as_ref().map_or(vec![], |c| c.messages.clone());

    let mut pending_approvals = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(session_id.clone());

    if let Some(ref ckpt) = checkpoint
        && let coda_agent::persist::StoredResumePoint::PendingApproval {
            ref pending_approval_calls,
            ..
        } = ckpt.resume_point
        && !pending_approval_calls.is_empty()
    {
        pending_approvals.push(coda_agent::PendingApproval {
            thread_id: ckpt.thread_id.clone(),
            agent_name: ckpt.agent_name.clone(),
            calls: pending_approval_calls.clone(),
            suspended_at: ckpt.suspended_at,
        });
    }

    if let Some(snapshot) = state
        .storage
        .load_session_snapshot(&session_id)
        .await
        .map_err(|e| format!("storage error: {e}"))?
    {
        for tid in snapshot
            .active_threads
            .values()
            .filter(|tid| seen.insert((*tid).clone()))
        {
            if let Some(ckpt) = state
                .storage
                .load_checkpoint(tid)
                .await
                .map_err(|e| format!("storage error: {e}"))?
                && let coda_agent::persist::StoredResumePoint::PendingApproval {
                    ref pending_approval_calls,
                    ..
                } = ckpt.resume_point
                && !pending_approval_calls.is_empty()
            {
                pending_approvals.push(coda_agent::PendingApproval {
                    thread_id: ckpt.thread_id.clone(),
                    agent_name: ckpt.agent_name.clone(),
                    calls: pending_approval_calls.clone(),
                    suspended_at: ckpt.suspended_at,
                });
            }
        }
    }

    Ok(Json(HistoryResponse {
        messages,
        pending_approvals,
    }))
}

async fn add_allow_pattern_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddAllowPatternRequest>,
) -> Result<(), (StatusCode, String)> {
    let config = state.approval_config.clone();
    let pattern = req.pattern;
    tokio::task::spawn_blocking(move || {
        config
            .add_allow_pattern(&pattern)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
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

    let mcp_servers = coda_examples::mcp::load_mcp_servers(&workspace_dir)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("failed to load MCP servers: {e}");
            McpServers::empty()
        });

    let approval_config = ToolApprovalConfig::load(&workspace_dir).unwrap_or_else(|e| {
        tracing::warn!("failed to load approval config: {e}");
        ToolApprovalConfig::default_for(&workspace_dir)
    });

    let state = Arc::new(AppState {
        storage,
        system_prompt,
        workspace_str,
        provider,
        model,
        mcp_servers,
        approval_config,
    });

    let app = Router::new()
        .route("/chat", post(chat_handler))
        .route("/history/{session_id}", get(history_handler))
        .route("/permissions/allow", post(add_allow_pattern_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    info!("coda_server listening on http://127.0.0.1:3000");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            info!("shutdown signal received");
        })
        .await
        .unwrap();

    match Arc::try_unwrap(state) {
        Ok(app_state) => app_state.mcp_servers.shutdown().await,
        Err(_) => warn!("cannot shutdown MCP servers: outstanding references"),
    }

    info!("server stopped");
}
