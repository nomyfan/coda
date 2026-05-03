use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use coda_agent::{
    AgentEvent, AgentSpec, BuildContext, OpenError, RunConfig, Session, Shutdown, SubAgentMode,
    ToolApprovalMode, builtin_specs, runtime::SessionStorage,
};
use coda_core::llm::LLMProviderConfig;
use coda_openai::OpenAI;
use coda_server::storage::JsonFileStorage;
use coda_server::{ChatRequest, ChatResponse, ChatStatus, HistoryResponse, WireEvent};
use coda_skills::Skills;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

static SYSTEM_PROMPT: &str = include_str!("../system-prompt.md");
static AGENT_SKILLS_PROMPT: &str = include_str!("../agent-skills-prompt.md");

struct AppState {
    storage: JsonFileStorage,
    system_prompt: String,
    workspace_str: String,
    provider: Arc<OpenAI>,
    model: String,
}

fn build_agent_spec(system_prompt: String) -> AgentSpec {
    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt,
        mode: SubAgentMode::Stateful,
        tools: builtin_specs(),
        subagents: vec![
            AgentSpec {
                name: "explore".into(),
                description:
                    "An explore sub-agent that can read files, search code, and explore the codebase."
                        .into(),
                system_prompt: "You are an exploration assistant. Summarize findings concisely."
                    .to_string(),
                mode: SubAgentMode::Stateless,
                tools: builtin_specs(),
                subagents: vec![],
            },
            AgentSpec {
                name: "memo".into(),
                description:
                    "A stateful memo agent that remembers information across calls. \
                     Use it to store and recall facts across turns."
                        .into(),
                system_prompt:
                    "You are a simple memo agent. Your only job is to remember what the user tells you and \
                     answer questions about it. Keep your replies very brief."
                        .to_string(),
                mode: SubAgentMode::Stateful,
                tools: vec![],
                subagents: vec![],
            },
        ],
    }
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
        tool_approval: ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "shell")),
        approval_timeout: Some(Duration::from_secs(300)),
    };

    let spec = build_agent_spec(state.system_prompt.clone());
    let ctx = BuildContext {
        workspace_dir: state.workspace_str.clone(),
    };

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

    if let Some(task) = &req.task {
        if session.send(task.clone()).await.is_err() {
            let _ = session.shutdown(Shutdown::Abort).await;
            return Ok(Json(ChatResponse {
                status: ChatStatus::Error("failed to send task".into()),
                events: vec![],
                pending_approvals: vec![],
            }));
        }
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

    // Check root checkpoint for pending approval.
    if let Some(ref ckpt) = checkpoint {
        if let coda_agent::agent::ResumePoint::PendingApproval {
            ref pending_approval_calls,
            ..
        } = ckpt.resume_point
        {
            if !pending_approval_calls.is_empty() {
                pending_approvals.push(coda_agent::PendingApproval {
                    thread_id: ckpt.thread_id.clone(),
                    agent_name: ckpt.agent_name.clone(),
                    calls: pending_approval_calls.iter().cloned().collect(),
                    suspended_at: ckpt.suspended_at,
                });
            }
        }
    }

    // Check snapshot for subagent pending approvals.
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
            {
                if let coda_agent::agent::ResumePoint::PendingApproval {
                    ref pending_approval_calls,
                    ..
                } = ckpt.resume_point
                {
                    if !pending_approval_calls.is_empty() {
                        pending_approvals.push(coda_agent::PendingApproval {
                            thread_id: ckpt.thread_id.clone(),
                            agent_name: ckpt.agent_name.clone(),
                            calls: pending_approval_calls.iter().cloned().collect(),
                            suspended_at: ckpt.suspended_at,
                        });
                    }
                }
            }
        }
    }

    Ok(Json(HistoryResponse {
        messages,
        pending_approvals,
    }))
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

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let workspace_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace_str = workspace_dir.to_string_lossy().into_owned();

    let mut system_prompt = SYSTEM_PROMPT
        .replace("{{OS}}", &format!("{os}({arch})"))
        .replace("{{WORKSPACE_DIR}}", &workspace_str);

    match Skills::from_dir(&PathBuf::from_str(".coda/skills").unwrap()) {
        Ok(skills) => {
            info!("loaded {} skills", skills.0.len());
            system_prompt.push_str("\n---\n");
            system_prompt.push_str(AGENT_SKILLS_PROMPT);
            system_prompt.push('\n');
            system_prompt.push_str(&skills.to_xml());
        }
        Err(err) => {
            tracing::warn!("failed to load skills, proceeding without them: {err}");
        }
    }

    let provider = Arc::new(OpenAI::new(LLMProviderConfig {
        api_key,
        base_url,
        stream: true,
    }));

    let checkpoint_dir = workspace_dir.join(".coda").join("sessions");
    let storage = JsonFileStorage::new(checkpoint_dir);

    let state = Arc::new(AppState {
        storage,
        system_prompt,
        workspace_str,
        provider,
        model,
    });

    let app = Router::new()
        .route("/chat", post(chat_handler))
        .route("/history/{session_id}", get(history_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    info!("coda_server listening on http://127.0.0.1:3000");
    axum::serve(listener, app).await.unwrap();
}
