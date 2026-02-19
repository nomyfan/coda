mod ask_user;

use dotenvy::dotenv;
use either::Either;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::debug;
use tracing::info;
use tracing::warn;

use ask_user::{AskUserParams, AskUserTool};
use coda_agent::{
    Agent, AgentCheckpoint, AgentEvent, ApprovalDecision, RejectedCall, RunConfig, SessionStore,
    ToolApprovalMode,
};
use coda_core::llm::{
    LLMProviderConfig, Message, StreamError, SystemMessage, ToolCall, ToolCallOutcome, ToolMessage,
    ToolOutput, UserMessage,
};
use coda_openai::OpenAI;
use coda_skills::Skills;
use futures::{Stream, StreamExt};

static SYSTEM_PROMPT: &str = include_str!("system-prompt.md");
static AGENT_SKILLS_PROMPT: &str = include_str!("agent-skills-prompt.md");

const LOGO: &str = r#"
  ██████╗ ██████╗ ██████╗  █████╗
 ██╔════╝██╔═══██╗██╔══██╗██╔══██╗
 ██║     ██║   ██║██║  ██║███████║
 ██║     ██║   ██║██║  ██║██╔══██║
 ╚██████╗╚██████╔╝██████╔╝██║  ██║
  ╚═════╝ ╚═════╝ ╚═════╝ ╚═╝  ╚═╝
"#;

fn print_logo() {
    // ANSI 真彩色: \x1b[38;2;<R>;<G>;<B>m
    // #f27b73 = RGB(242, 123, 115) - 浅珊瑚红
    println!("\x1b[1;38;2;242;123;115m{}\x1b[0m", LOGO);
    println!("\x1b[2;37m  An AI Agent\x1b[0m");
    println!();
}

fn prompt_ask_user(
    question: &str,
    options: &[String],
) -> Result<String, Box<dyn std::error::Error>> {
    println!("\n[User Input Required]\n");
    println!("{}\n", question);
    for (i, opt) in options.iter().enumerate() {
        println!("  {}. {}", i + 1, opt);
    }
    println!("  0. Other (type your own response)\n");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input == "0" {
            print!("Your response: ");
            io::stdout().flush()?;
            let mut custom = String::new();
            io::stdin().read_line(&mut custom)?;
            return Ok(custom.trim().to_string());
        }
        if let Ok(idx) = input.parse::<usize>()
            && idx >= 1
            && idx <= options.len()
        {
            return Ok(options[idx - 1].clone());
        }
        println!("  Please enter a number between 0 and {}.", options.len());
    }
}

fn prompt_approval(
    call: &ToolCall,
) -> Result<Either<String, RejectedCall>, Box<dyn std::error::Error>> {
    println!(
        "\n  {}: {}",
        call.name,
        call.arguments.as_deref().unwrap_or("{}")
    );

    loop {
        print!("     Approve? [y/n]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                return Ok(Either::Left(call.id.clone()));
            }
            "n" | "no" => {
                print!("     Reason (optional, Enter to skip): ");
                io::stdout().flush()?;
                let mut reason = String::new();
                io::stdin().read_line(&mut reason)?;
                let reason = reason.trim().to_string();
                return Ok(Either::Right(RejectedCall {
                    id: call.id.clone(),
                    reason: if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    },
                }));
            }
            _ => println!("     Please enter 'y' or 'n'."),
        }
    }
}

/// Show the session picker. Returns the session id to resume, or None for a new session.
/// Returns Err if the user chose to quit.
fn prompt_session_choice(
    store: &SessionStore,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let sessions = store.list();
    if sessions.is_empty() {
        return Ok(None);
    }

    println!("Available sessions (most recent first):\n");
    for (i, s) in sessions.iter().enumerate() {
        // message_count counts User+Assistant+Tool messages; approximate "turns" as User msgs.
        println!(
            "  {}. [{}] {}  ({} messages)",
            i + 1,
            jiff::Timestamp::from_second(s.updated_at as i64)
                .map(|ts| ts
                    .to_zoned(jiff::tz::TimeZone::UTC)
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string())
                .unwrap_or_default(),
            s.title,
            s.message_count,
        );
    }
    println!("\n  n. New session");
    println!("  q. Quit");

    loop {
        print!("\n> ");
        io::stdout().flush()?;

        let mut input = String::new();
        let n = io::stdin().read_line(&mut input)?;
        if n == 0 {
            // EOF
            std::process::exit(0);
        }
        let input = input.trim().to_lowercase();

        if input == "q" {
            std::process::exit(0);
        }
        if input == "n" {
            return Ok(None);
        }
        if let Ok(idx) = input.parse::<usize>()
            && idx >= 1
            && idx <= sessions.len()
        {
            return Ok(Some(sessions[idx - 1].id.clone()));
        }
        println!(
            "  Please enter a number between 1 and {}, 'n', or 'q'.",
            sessions.len()
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv()?;

    let api_key = env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let base_url = env::var("OPENAI_BASE_URL").expect("OPENAI_BASE_URL must be set");
    let model = env::var("OPENAI_MODEL").expect("OPENAI_MODEL must be set");

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let workspace_dir = std::env::current_dir()?;
    let workspace_str = workspace_dir.to_string_lossy().into_owned();
    let session_store = SessionStore::new(workspace_dir.join(".coda").join("sessions"));
    let mut system_prompt = SYSTEM_PROMPT
        .replace("{{OS}}", &format!("{}({})", os, arch))
        .replace("{{WORKSPACE_DIR}}", &workspace_str);

    match Skills::from_dir(&PathBuf::from_str(".coda/skills").unwrap()) {
        Ok(skills) => {
            info!("Loaded skills {:?}", skills.0);
            system_prompt.push_str("\n---\n");
            system_prompt.push_str(AGENT_SKILLS_PROMPT);
            system_prompt.push('\n');
            system_prompt.push_str(&skills.to_xml());
        }
        Err(err) => {
            warn!("Failed to load skills, proceeding without them: {}", err);
        }
    }

    let mut agent = Agent::new_with_default_tools(
        OpenAI::new(LLMProviderConfig {
            api_key,
            base_url,
            stream: true,
        }),
        workspace_str.clone(),
    );
    agent.tools.register(AskUserTool::new());

    agent
        .add_message(Message::System(SystemMessage(system_prompt)))
        .await;

    print_logo();

    // Session picker: let the user choose an existing session or start new.
    let mut initial_checkpoint: Option<AgentCheckpoint> = None;
    let session_id: Option<String> = match prompt_session_choice(&session_store)? {
        Some(id) => match session_store.load(&id) {
            Ok(data) => {
                println!("Resuming: {}\n", data.title);
                print_history(&data.messages);
                let pending_calls = data.pending_calls.clone();
                let auto_calls = data.auto_calls.clone();
                agent.restore_history(data.messages, data.todos).await;
                if !pending_calls.is_empty() {
                    let todos = agent.state.lock().await.todos.clone();
                    initial_checkpoint = Some(AgentCheckpoint {
                        thread_id: id.clone(),
                        messages: agent.messages().await,
                        pending_calls,
                        auto_calls,
                        todos,
                    });
                }
                Some(id)
            }
            Err(e) => {
                warn!("Failed to load session {}: {}", id, e);
                println!("Could not load session, starting new.\n");
                None
            }
        },
        None => None,
    };

    // session_id tracks the id of the current session for saving on exit.
    let mut current_session_id: Option<String> = session_id;

    println!("Type 'quit', 'exit', or 'q' to stop");

    let make_config = |thread_id: String| RunConfig {
        model: model.clone(),
        max_completion_tokens: Some(5000),
        temperature: Some(0.5),
        thread_id,
        tool_approval: ToolApprovalMode::RequireWhen(std::sync::Arc::new(|call| {
            info!(
                "deciding whether to require approval for tool call: {}",
                call.name
            );
            call.name == "shell" || call.name == "ask_user"
        })),
    };

    // Main conversation loop
    loop {
        // Explicit dyn Stream type so run() and resume() can be assigned to the same variable.
        let mut stream: std::pin::Pin<
            Box<dyn Stream<Item = Result<AgentEvent, StreamError>> + '_>,
        > = if let Some(cp) = initial_checkpoint.take() {
            // Restored from a saved session: surface the checkpoint immediately without
            // starting a new LLM turn or reading user input.
            Box::pin(futures::stream::once(async move {
                Ok::<AgentEvent, StreamError>(AgentEvent::Suspended(cp))
            }))
        } else {
            print!("\nYou: ");
            io::stdout().flush()?;

            let mut user_input = String::new();
            let n = io::stdin().read_line(&mut user_input)?;

            // EOF
            if n == 0 {
                save_and_exit(&agent, &session_store, current_session_id.as_deref()).await;
            }

            let user_input = user_input.trim();

            if user_input.is_empty() {
                continue;
            }
            if user_input.eq_ignore_ascii_case("quit")
                || user_input.eq_ignore_ascii_case("exit")
                || user_input.eq_ignore_ascii_case("q")
            {
                save_and_exit(&agent, &session_store, current_session_id.as_deref()).await;
            }

            print!("Assistant: ");
            io::stdout().flush()?;

            let thread_id = uuid::Uuid::new_v4().to_string();
            Box::pin(agent.run(UserMessage(user_input.to_string()), make_config(thread_id)))
        };

        loop {
            let mut suspended_checkpoint = None;

            while let Some(event) = stream.next().await {
                match event.map_err(|e: StreamError| Box::new(e) as Box<dyn std::error::Error>)? {
                    AgentEvent::LLMContentChunk(s) => {
                        print!("{s}");
                        io::stdout().flush()?;
                    }
                    AgentEvent::LLMEnd(msg) if !msg.content.is_empty() => {
                        println!();
                    }
                    AgentEvent::ToolCallStart(c) => {
                        debug!(
                            "tool call: id={} name={} arguments={:?}",
                            c.id, c.name, c.arguments
                        );
                        println!("\n[Tool: {}]: {:?}", c.name, c.arguments);
                    }
                    AgentEvent::ToolCallEnd(m) => {
                        debug!(
                            "tool result: id={} name={} output={:?}",
                            m.id, m.name, m.output
                        );
                    }
                    AgentEvent::Suspended(checkpoint) => {
                        // Persist the checkpoint so it survives process restarts.
                        match session_store
                            .save_checkpoint(current_session_id.as_deref(), &checkpoint)
                        {
                            Ok(id) => current_session_id = Some(id),
                            Err(e) => warn!("Failed to save checkpoint: {}", e),
                        }
                        suspended_checkpoint = Some(checkpoint);
                        break;
                    }
                    _ => {}
                }
            }

            match suspended_checkpoint {
                None => break, // stream completed normally
                Some(checkpoint) => {
                    let mut approved = vec![];
                    let mut rejected = vec![];
                    let mut handled = vec![];
                    for pending_call in &checkpoint.pending_calls {
                        if pending_call.name == "ask_user" {
                            let output = match serde_json::from_str::<AskUserParams>(
                                pending_call.arguments.as_deref().unwrap_or("{}"),
                            ) {
                                Ok(params) => ToolOutput::Ok(prompt_ask_user(
                                    &params.question,
                                    &params.options,
                                )?),
                                Err(err) => {
                                    ToolOutput::Err(format!("Invalid ask_user arguments: {err}"))
                                }
                            };
                            handled.push(ToolMessage {
                                id: pending_call.id.clone(),
                                name: pending_call.name.clone(),
                                output,
                                outcome: ToolCallOutcome::Auto,
                            });
                        } else {
                            match prompt_approval(pending_call)? {
                                Either::Left(id) => approved.push(id),
                                Either::Right(reject) => rejected.push(reject),
                            }
                        }
                    }

                    let thread_id = checkpoint.thread_id.clone();
                    stream = Box::pin(agent.resume(
                        checkpoint,
                        ApprovalDecision {
                            approved,
                            rejected,
                            handled,
                        },
                        make_config(thread_id),
                    ));
                }
            }
        }

        // After each turn, persist the session (without checkpoint — turn completed normally).
        let messages = agent.messages().await;
        let todos = {
            let state = agent.state.lock().await;
            state.todos.clone()
        };
        match session_store.save(current_session_id.as_deref(), &messages, &todos) {
            Ok(id) => current_session_id = Some(id),
            Err(e) => warn!("Failed to save session: {}", e),
        }
    }
}

fn print_history(messages: &[Message]) {
    for msg in messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                println!("\nYou: {}", u.0);
            }
            Message::Assistant(a) => {
                if !a.content.is_empty() {
                    println!("Assistant: {}", a.content);
                }
                for call in &a.tool_calls {
                    println!("\n[Tool: {}]: {:?}", call.name, call.arguments);
                }
            }
            Message::Tool(t) => {
                let status = match &t.outcome {
                    ToolCallOutcome::Rejected { reason } => match reason {
                        Some(r) => format!("rejected: {r}"),
                        None => "rejected".to_string(),
                    },
                    _ => match &t.output {
                        ToolOutput::Ok(_) => "ok".to_string(),
                        ToolOutput::Err(e) => format!("err: {e}"),
                    },
                };
                let approved_tag = match &t.outcome {
                    ToolCallOutcome::Approved => " (approved)",
                    _ => "",
                };
                println!("  -> {status}{approved_tag}");
            }
        }
    }
    println!();
}

async fn save_and_exit(agent: &Agent<OpenAI>, store: &SessionStore, session_id: Option<&str>) -> ! {
    let messages = agent.messages().await;
    let has_user_msg = messages.iter().any(|m| matches!(m, Message::User(_)));
    if has_user_msg {
        let todos = agent.state.lock().await.todos.clone();
        match store.save(session_id, &messages, &todos) {
            Ok(_) => println!("\nSession saved."),
            Err(e) => eprintln!("\nFailed to save session: {}", e),
        }
    }
    println!("Goodbye!");
    std::process::exit(0);
}
