mod ask_user;
mod storage;

use ask_user::{AskUserParams, AskUserToolSpec};
use coda_agent::{
    AbortedTarget, AgentCheckpoint, AgentEvent, AgentSpec, BuildContext, OpenError, ResumeDecision,
    RunConfig, Session, SessionEvent, Shutdown, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    agent::ResumePoint, builtin_specs,
};
use coda_core::llm::{
    CompletionUsage, LLMProviderConfig, Message, ToolCall, ToolCallOutcome, ToolOutput,
};
use coda_openai::OpenAI;
use coda_skills::Skills;
use dotenvy::dotenv;
use either::Either;
use rustyline::error::ReadlineError;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::{env, time::Duration};
use storage::JsonFileStorage;
use tracing::{info, warn};
use uuid::Uuid;

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
    rl: &mut rustyline::DefaultEditor,
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
        let input = rl.readline("> ")?;
        let input = input.trim().to_string();

        if input == "0" {
            let custom = rl.readline("Your response: ")?;
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

#[allow(clippy::type_complexity)]
fn prompt_approval(
    rl: &mut rustyline::DefaultEditor,
    call: &ToolCall,
) -> Result<Either<String, (String, Option<String>)>, Box<dyn std::error::Error>> {
    println!(
        "\n  {}: {}",
        call.name,
        call.arguments.as_deref().unwrap_or("{}")
    );

    loop {
        let input = rl.readline("     Approve? [y/n]: ")?;

        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                return Ok(Either::Left(call.id.clone()));
            }
            "n" | "no" => {
                let reason = rl.readline("     Reason (optional, Enter to skip): ")?;
                let reason = reason.trim().to_string();
                return Ok(Either::Right((
                    call.id.clone(),
                    if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    },
                )));
            }
            _ => println!("     Please enter 'y' or 'n'."),
        }
    }
}

fn resolve_pending_calls(
    rl: &mut rustyline::DefaultEditor,
    checkpoint: &AgentCheckpoint,
) -> Result<Vec<(String, ToolCallResolution)>, Box<dyn std::error::Error>> {
    let ResumePoint::PendingApproval {
        pending_approval_calls,
        ..
    } = &checkpoint.resume_point
    else {
        return Ok(vec![]);
    };

    let mut resolutions = vec![];
    for pending_call in pending_approval_calls {
        if pending_call.name == "ask_user" {
            let output = match serde_json::from_str::<AskUserParams>(
                pending_call.arguments.as_deref().unwrap_or("{}"),
            ) {
                Ok(params) => {
                    ToolOutput::Ok(prompt_ask_user(rl, &params.question, &params.options)?)
                }
                Err(err) => ToolOutput::Err(format!("Invalid ask_user arguments: {err}")),
            };
            resolutions.push((
                pending_call.id.clone(),
                ToolCallResolution::Resolved(output),
            ));
        } else {
            match prompt_approval(rl, pending_call)? {
                Either::Left(id) => {
                    resolutions.push((id, ToolCallResolution::Execute));
                }
                Either::Right((id, reason)) => {
                    resolutions.push((id, ToolCallResolution::Rejected { reason }));
                }
            }
        }
    }
    Ok(resolutions)
}

fn render_message(message: &Message) {
    match message {
        Message::System(_) => {}
        Message::User(msg) => {
            println!("You: {}", msg.0);
        }
        Message::Assistant(msg) => {
            if !msg.content.is_empty() {
                println!("Assistant: {}", msg.content);
            }
            for tool_call in &msg.tool_calls {
                println!(
                    "[Tool: {}]: {}",
                    tool_call.name,
                    tool_call.arguments.as_deref().unwrap_or("{}")
                );
            }
            if let Some(usage) = &msg.usage {
                println!("{}", token_usage_line(None, usage));
            }
            if msg.aborted {
                println!("[Assistant generation was interrupted]");
            }
        }
        Message::Tool(msg) => {
            let status = match &msg.outcome {
                ToolCallOutcome::Auto => "auto",
                ToolCallOutcome::Approved => "approved",
                ToolCallOutcome::Resolved => "resolved",
                ToolCallOutcome::Rejected { .. } => "rejected",
                ToolCallOutcome::Aborted => "aborted",
            };
            match &msg.output {
                ToolOutput::Ok(output) => {
                    println!("[Tool Result: {}][{}] {}", msg.name, status, output)
                }
                ToolOutput::Err(output) => {
                    println!("[Tool Error: {}][{}] {}", msg.name, status, output)
                }
            }
        }
    }
}

fn token_usage_summary(usage: &CompletionUsage) -> String {
    let total_tokens = usage.prompt_tokens + usage.completion_tokens;
    format!(
        "prompt: {} | completion: {} | total: {}",
        usage.prompt_tokens, usage.completion_tokens, total_tokens
    )
}

fn token_usage_line(agent_name: Option<&str>, usage: &CompletionUsage) -> String {
    match agent_name {
        Some(name) if name != "coda" => {
            format!("[Token Usage: {name}] {}", token_usage_summary(usage))
        }
        _ => format!("[Token Usage] {}", token_usage_summary(usage)),
    }
}

fn render_checkpoint_history(checkpoint: &AgentCheckpoint) {
    if checkpoint.messages.is_empty() {
        return;
    }

    println!("Resumed conversation:\n");
    for message in &checkpoint.messages {
        render_message(message);
    }
    println!();
}

fn build_agent_spec(system_prompt: String) -> AgentSpec {
    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt,
        mode: SubAgentMode::Stateful,
        tools: {
            let mut t = builtin_specs();
            t.push(Box::new(AskUserToolSpec));
            t
        },
        subagents: vec![
            AgentSpec {
                name: "explore".into(),
                description: "An explore sub-agent that can read files, search code, and explore the codebase. Delegate exploration and research tasks to it.".into(),
                system_prompt:
                    "You are an exploration assistant. You can read files, search code, and list directories. \
                     Summarize your findings concisely."
                        .to_string(),
                mode: SubAgentMode::Stateless,
                tools: builtin_specs(),
                subagents: vec![],
            },
            AgentSpec {
                name: "memo".into(),
                description: "A stateful memo agent that remembers information across calls. \
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

fn print_usage(program: &str) {
    println!("Usage: {program} [--resume <uuid>]");
}

fn parse_session_id_arg(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    let mut session_id = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resume" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --resume".to_string())?;
                Uuid::parse_str(&value)
                    .map_err(|err| format!("invalid thread id '{value}': {err}"))?;
                if session_id.replace(value).is_some() {
                    return Err("session id can only be provided once".to_string());
                }
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(session_id)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let program = env::args().next().unwrap_or_else(|| "coda".into());
    // session id is also the thread id for the main agent
    let resumed_session_id = match parse_session_id_arg(env::args().skip(1)) {
        Ok(Some(thread_id)) => Some(thread_id),
        Ok(None) => None,
        Err(err) if err.is_empty() => {
            print_usage(&program);
            return Ok(());
        }
        Err(err) => {
            eprintln!("Error: {err}");
            print_usage(&program);
            return Err(err.into());
        }
    };
    let session_id = resumed_session_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

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

    let provider = std::sync::Arc::new(OpenAI::new(LLMProviderConfig {
        api_key,
        base_url,
        stream: true,
    }));

    let checkpoint_dir = workspace_dir.join(".coda").join("sessions");
    let storage = JsonFileStorage::new(checkpoint_dir);

    print_logo();
    let mut rl = rustyline::DefaultEditor::new()?;

    let mut resume_decisions: HashMap<String, ResumeDecision> = HashMap::new();
    let session = loop {
        let ctx = BuildContext {
            workspace_dir: workspace_str.clone(),
        };
        let spec = build_agent_spec(system_prompt.clone());
        let config = RunConfig {
            provider: provider.clone(),
            model: model.clone(),
            max_completion_tokens: Some(5000),
            temperature: Some(0.7),
            tool_approval: ToolApprovalMode::RequireWhen(std::sync::Arc::new(|call| {
                call.name == "shell" || call.name == "ask_user"
            })),
        };
        match Session::builder()
            .storage(storage.clone())
            .root(spec)
            .build_context(ctx)
            .run_config(config)
            .session_id(session_id.clone())
            .resume_decisions(std::mem::take(&mut resume_decisions))
            .open()
            .await
        {
            Ok(s) => break s,
            Err(OpenError::PendingApprovalsRequired(ckpts)) => {
                println!(
                    "\n[Resuming session — {} pending approval(s) to resolve]",
                    ckpts.len()
                );
                for ckpt in ckpts {
                    let resolutions = match resolve_pending_calls(&mut rl, &ckpt) {
                        Ok(r) => r,
                        Err(e) => {
                            println!("\n[Aborted: approval interrupted: {e}]");
                            return Err(e);
                        }
                    };
                    resume_decisions.insert(ckpt.thread_id.clone(), ResumeDecision { resolutions });
                }
            }
            Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error>),
        }
    };

    println!("Type 'quit', 'exit', or 'q' to stop\n");
    println!("Session id: {}\n", session_id);
    if resumed_session_id.is_some()
        && let Some(checkpoint) = session.resumed_checkpoint()
    {
        render_checkpoint_history(checkpoint)
    }

    // Skip the first readline if agents are already running after resume.
    let mut skip_input = session.has_resuming_agents();

    loop {
        if !skip_input {
            let raw_input = match rl.readline("You: ") {
                Ok(line) => line,
                Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => {
                    println!("Goodbye!");
                    break;
                }
                Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error>),
            };

            let user_input = raw_input.trim();
            if user_input.is_empty() {
                continue;
            }
            if user_input.eq_ignore_ascii_case("quit")
                || user_input.eq_ignore_ascii_case("exit")
                || user_input.eq_ignore_ascii_case("q")
            {
                println!("Goodbye!");
                break;
            }

            session.send(user_input.to_string()).await?;

            print!("Assistant: ");
            io::stdout().flush()?;
        }
        skip_input = false;

        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    session.abort().await;
                }
                event = session.recv() => {
                    let Some(SessionEvent { origin, kind, .. }) = event else { break };
                    match kind {
                        AgentEvent::LLMContentChunk(s) => {
                            if origin.is_root() {
                                print!("{s}");
                            } else {
                                print!("\x1b[36m{s}\x1b[0m");
                            }
                            io::stdout().flush()?;
                        }
                        AgentEvent::LLMEnd(msg) => {
                            if !msg.content.is_empty() || msg.usage.is_some() {
                                println!();
                            }
                            if let Some(usage) = &msg.usage {
                                let usage_line =
                                    token_usage_line(origin.subagent_name(), usage);
                                if origin.is_root() {
                                    println!("{usage_line}");
                                } else {
                                    println!("\x1b[36m{usage_line}\x1b[0m");
                                }
                            }
                            if origin.is_root() && msg.tool_calls.is_empty() {
                                break;
                            }
                        }
                        AgentEvent::ToolCallStart(c) => {
                            if origin.is_root() {
                                println!("\n[Tool: {}]: {:?}", c.name, c.arguments);
                            } else {
                                let name = origin.subagent_name().unwrap_or_default();
                                println!(
                                    "\n\x1b[36m[Sub-agent {}: {}]: {:?}\x1b[0m",
                                    name, c.name, c.arguments
                                );
                            }
                        }
                        AgentEvent::ToolCallEnd(_) => {}
                        AgentEvent::Suspended(checkpoint) => {
                            let label = if origin.is_root() { "" } else { " (sub-agent)" };
                            let ResumePoint::PendingApproval {
                                pending_approval_calls,
                                ..
                            } = &checkpoint.resume_point
                            else {
                                continue;
                            };
                            println!(
                                "\n[{} tool call(s) require approval{}]",
                                pending_approval_calls.len(),
                                label
                            );
                            let resolutions = match resolve_pending_calls(&mut rl, &checkpoint) {
                                Ok(r) => r,
                                Err(_) => {
                                    session.abort().await;
                                    println!("\n[Aborted: approval interrupted]");
                                    break;
                                }
                            };
                            if let Err(e) =
                                session.resume(&checkpoint, ResumeDecision { resolutions }).await
                            {
                                warn!("{}", e);
                                println!("\n[Error: failed to resume agent: {}]", e);
                                break;
                            }
                        }
                        AgentEvent::Aborted(target) if origin.is_root() => match &target {
                            AbortedTarget::Generation => {
                                println!("\n\n[Aborted: generation interrupted]");
                                break;
                            }
                            AbortedTarget::ToolCalls(ids) => {
                                println!("\n[Aborted: {} tool call(s) interrupted]", ids.len());
                                break;
                            }
                        },
                        AgentEvent::Error(err) if origin.is_root() => {
                            warn!("{}", err);
                            println!("\n[Error: {}]", err);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        println!();
    }

    println!("Current session id: {}", session_id);
    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
    println!("Session ended. You can resume this session later with `--resume {session_id}`");

    Ok(())
}
