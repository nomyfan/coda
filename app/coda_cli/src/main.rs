mod ask_user;

use ask_user::{AskUserParams, AskUserToolSpec};
use coda_agent::{
    AgentCheckpoint, AgentEvent, AgentSpec, BuildContext, Envelope, ResumeDecision, RunConfig,
    Sender, SubAgentMode, SubAgentSpec, ToolApprovalMode, ToolCallResolution, builtin_specs,
};
use coda_core::llm::{LLMProviderConfig, ToolCall, ToolOutput};
use coda_openai::OpenAI;
use coda_runtime::{AgentControl, AgentRuntime};
use coda_skills::Skills;
use dotenvy::dotenv;
use either::Either;
use rustyline::error::ReadlineError;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{info, warn};

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
    let mut resolutions = vec![];
    for pending_call in &checkpoint.pending_calls {
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

    let ctx = BuildContext {
        workspace_dir: workspace_str.clone(),
    };

    let spec = AgentSpec {
        name: "coda".into(),
        system_prompt,
        tools: {
            let mut t = builtin_specs();
            t.push(Box::new(AskUserToolSpec));
            t
        },
        subagents: vec![
            SubAgentSpec {
                name: "researcher".into(),
                description: "A research sub-agent that can read files, search code, and explore the codebase. Delegate research tasks to it.".into(),
                mode: SubAgentMode::Stateless,
                agent: AgentSpec {
                    name: "researcher".into(),
                    system_prompt:
                        "You are a research assistant. You can read files, search code, and list directories. \
                         Summarize your findings concisely."
                            .to_string(),
                    tools: builtin_specs(),
                    subagents: vec![],
                },
            },
            SubAgentSpec {
                name: "memo".into(),
                description: "A stateful memo agent that remembers information across calls. \
                              Use it to store and recall facts across turns."
                    .into(),
                mode: SubAgentMode::Stateful,
                agent: AgentSpec {
                    name: "memo".into(),
                    system_prompt: (
                        "You are a simple memo agent. Your only job is to remember what the user tells you and \
                         answer questions about it. Keep your replies very brief."
                            .to_string()
                    ),
                    tools: vec![],
                    subagents: vec![],
                },
            },
        ],
    };

    let agent = spec.build(&ctx);

    let runtime = AgentRuntime::new();
    let handle = runtime
        .spawn_agent(
            agent,
            RunConfig {
                provider,
                model,
                max_completion_tokens: Some(5000),
                temperature: Some(0.7),
                thread_id: uuid::Uuid::new_v4().to_string(),
                tool_approval: ToolApprovalMode::RequireWhen(std::sync::Arc::new(|call| {
                    call.name == "shell" || call.name == "ask_user"
                })),
            },
        )
        .await;

    print_logo();

    let mut rl = rustyline::DefaultEditor::new()?;
    println!("Type 'quit', 'exit', or 'q' to stop\n");

    loop {
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

        // Subscribe to all agents (including subagents spawned later)
        let mut event_rx = runtime.subscribe();

        handle
            .send_message(Envelope::new(|id| Envelope {
                id,
                from: Sender::User,
                to: Sender::Agent(handle.agent_id().clone()),
                reply_to: None,
                body: user_input.to_string(),
            }))
            .await?;

        print!("Assistant: ");
        io::stdout().flush()?;

        let mut abort_requested = false;

        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    if abort_requested {
                        println!("\nGoodbye!");
                        std::process::exit(0);
                    }
                    abort_requested = true;
                    runtime.broadcast_command(AgentControl::Abort).await;
                }
                event = event_rx.recv() => {
                    match event {
                        Err(_) => break,
                        Ok((aid, AgentEvent::LLMContentChunk(s))) => {
                            if aid == *handle.agent_id() {
                                print!("{s}");
                            } else {
                                print!("\x1b[36m{s}\x1b[0m");
                            }
                            io::stdout().flush()?;
                        }
                        Ok((aid, AgentEvent::LLMEnd(msg))) => {
                            if !msg.content.is_empty() {
                                println!();
                            }
                            // Only the main agent finishing means the turn is done.
                            if aid == *handle.agent_id() && msg.tool_calls.is_empty() {
                                break;
                            }
                        }
                        Ok((aid, AgentEvent::ToolCallStart(c))) => {
                            let is_main = aid == *handle.agent_id();
                            if is_main {
                                println!("\n[Tool: {}]: {:?}", c.name, c.arguments);
                            } else {
                                println!("\n\x1b[36m[Sub-agent Tool: {}]: {:?}\x1b[0m", c.name, c.arguments);
                            }
                        }
                        Ok((_, AgentEvent::ToolCallEnd(_))) => {}
                        Ok((aid, AgentEvent::Suspended(checkpoint))) => {
                            let is_main = aid == *handle.agent_id();
                            let label = if is_main { "" } else { " (sub-agent)" };
                            println!("\n[{} tool call(s) require approval{}]", checkpoint.pending_calls.len(), label);
                            let resolutions = match resolve_pending_calls(&mut rl, &checkpoint) {
                                Ok(r) => r,
                                Err(_) => {
                                    // Ctrl+C during approval prompt — abort all agents
                                    // (the suspended one and any parent waiting on it).
                                    runtime.broadcast_command(AgentControl::Abort).await;
                                    println!("\n[Aborted: approval interrupted]");
                                    // Drain until the main agent's Aborted event so the
                                    // outer loop doesn't see stale events.
                                    while let Ok((a, ev)) = event_rx.try_recv() {
                                        if a == *handle.agent_id() && matches!(ev, AgentEvent::Aborted(_)) {
                                            break;
                                        }
                                    }
                                    break;
                                }
                            };
                            let decision = ResumeDecision { resolutions };
                            if let Err(e) = runtime.send_command(&aid, AgentControl::Resume(decision)).await {
                                warn!("{}", e);
                                println!("\n[Error: failed to resume agent: {}]", e);
                                break;
                            }
                        }
                        Ok((aid, AgentEvent::Aborted(target))) if aid == *handle.agent_id() => {
                            match &target {
                                coda_agent::AbortedTarget::Generation => {
                                    println!("\n\n[Aborted: generation interrupted]");
                                }
                                coda_agent::AbortedTarget::ToolCalls(ids) => {
                                    println!("\n[Aborted: {} tool call(s) interrupted]", ids.len());
                                }
                            }
                            break;
                        }
                        Ok((aid, AgentEvent::Error(err))) if aid == *handle.agent_id() => {
                            warn!("{}", err);
                            println!("\n[Error: {}]", err);
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }

        println!();
    }

    Ok(())
}
