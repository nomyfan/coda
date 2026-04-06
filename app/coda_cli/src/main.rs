mod ask_user;

use ask_user::{AskUserParams, AskUserToolSpec};
use coda_agent::{
    AbortedTarget, AgentCheckpoint, AgentEvent, AgentSpec, BuildContext, Envelope, ResumeDecision,
    RunConfig, Sender, SubAgentMode, ThreadId, ToolApprovalMode, ToolCallResolution,
    agent::{EnvelopeBody, Receiver, ResumePoint},
    builtin_specs,
    runtime::{AgentControl, AgentRuntime, MemoryStorage},
};
use coda_core::llm::{LLMProviderConfig, ToolCall, ToolOutput};
use coda_openai::OpenAI;
use coda_skills::Skills;
use dotenvy::dotenv;
use either::Either;
use rustyline::error::ReadlineError;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::{env, time::Duration};
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
    };

    let agents = spec.build(&ctx)?;

    let config = RunConfig {
        provider,
        model,
        max_completion_tokens: Some(5000),
        temperature: Some(0.7),
        tool_approval: ToolApprovalMode::RequireWhen(std::sync::Arc::new(|call| {
            call.name == "shell" || call.name == "ask_user"
        })),
    };
    let mut runtime = AgentRuntime::new(MemoryStorage::default());
    runtime.bootstrap(agents, config).await;

    print_logo();

    let mut rl = rustyline::DefaultEditor::new()?;
    println!("Type 'quit', 'exit', or 'q' to stop\n");

    let thread_id = ThreadId::new();

    loop {
        let raw_input = match rl.readline("You: ") {
            Ok(line) => line,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => {
                println!("Goodbye!");
                runtime.broadcast_command(AgentControl::Exit).await;
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
            runtime.broadcast_command(AgentControl::Exit).await;
            break;
        }

        let mut event_rx = runtime.subscribe();

        runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: "coda".into(),
                    thread_id: thread_id.clone(),
                },
                reply_to: None,
                body: EnvelopeBody::Task(user_input.to_string()),
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
                        runtime.broadcast_command(AgentControl::Exit).await;
                        break;
                    }
                    abort_requested = true;
                    runtime.broadcast_command(AgentControl::Abort).await;
                }
                event = event_rx.recv() => {
                    match event {
                        Err(_) => break,
                        Ok((agent_name, _, AgentEvent::LLMContentChunk(s))) => {
                            if agent_name == "coda" {
                                print!("{s}");
                            } else {
                                print!("\x1b[36m{s}\x1b[0m");
                            }
                            io::stdout().flush()?;
                        }
                        Ok((agent_name, _, AgentEvent::LLMEnd(msg))) => {
                            if !msg.content.is_empty() {
                                println!();
                            }
                            if agent_name == "coda" && msg.tool_calls.is_empty() {
                                break;
                            }
                        }
                        Ok((agent_name, _, AgentEvent::ToolCallStart(c))) => {
                            if agent_name == "coda" {
                                println!("\n[Tool: {}]: {:?}", c.name, c.arguments);
                            } else {
                                println!("\n\x1b[36m[Sub-agent {}: {}]: {:?}\x1b[0m", agent_name,c.name, c.arguments);
                            }
                        }
                        Ok((_, _, AgentEvent::ToolCallEnd(_))) => {}
                        Ok((_, _, AgentEvent::Suspended(checkpoint))) => {
                            let is_main = checkpoint.agent_name == "coda";
                            let label = if is_main { "" } else { " (sub-agent)" };
                            let ResumePoint::PendingApproval { pending_approval_calls, .. } = &checkpoint.resume_point else {
                                continue;
                            };
                            println!("\n[{} tool call(s) require approval{}]", pending_approval_calls.len(), label);
                            let resolutions = match resolve_pending_calls(&mut rl, &checkpoint) {
                                Ok(r) => r,
                                Err(_) => {
                                    runtime.broadcast_command(AgentControl::Abort).await;
                                    println!("\n[Aborted: approval interrupted]");
                                    while let Ok((name, _, ev)) = event_rx.try_recv() {
                                        if name == "coda" && matches!(ev, AgentEvent::Aborted(_)) {
                                            break;
                                        }
                                    }
                                    break;
                                }
                            };
                            let decision = ResumeDecision { resolutions };
                            if let Err(e) = runtime
                                .send_message(Envelope::with_id(|id| Envelope {
                                    id,
                                    from: Sender::User,
                                    to: Receiver {
                                        name: checkpoint.agent_name.clone(),
                                        thread_id: ThreadId::from(checkpoint.thread_id.clone()),
                                    },
                                    reply_to: None,
                                    body: EnvelopeBody::Resume(decision),
                                }))
                                .await
                            {
                                warn!("{}", e);
                                println!("\n[Error: failed to resume agent: {}]", e);
                                break;
                            }
                        }
                        Ok((agent_name, _, AgentEvent::Aborted(target))) if agent_name == "coda" => {
                            match &target {
                                AbortedTarget::Generation => {
                                    println!("\n\n[Aborted: generation interrupted]");
                                }
                                AbortedTarget::ToolCalls(ids) => {
                                    println!("\n[Aborted: {} tool call(s) interrupted]", ids.len());
                                }
                            }
                            break;
                        }
                        Ok((agent_name, _, AgentEvent::Error(err))) if agent_name == "coda" => {
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

    // Wait a moment for agents to process the exit gracefully before shutting down the runtime.
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}
