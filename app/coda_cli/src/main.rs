use coda_agent::{Agent, AgentEvent, Envelope, RunConfig, Sender};
use coda_core::llm::LLMProviderConfig;
use coda_openai::OpenAI;
use coda_runtime::{AgentCommand, AgentRuntime};
use coda_skills::Skills;
use dotenvy::dotenv;
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

    let provider = OpenAI::new(LLMProviderConfig {
        api_key,
        base_url,
        stream: true,
    });

    let mut agent = Agent::new("coda", ());
    agent.with_default_tools(workspace_str.clone());
    agent.system_prompt = Some(system_prompt);

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

        // Subscribe before sending to avoid missing events
        let mut event_rx = handle.subscribe();

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
                    let _ = handle.send_command(AgentCommand::Abort).await;
                }
                event = event_rx.recv() => {
                    match event {
                        Err(_) => break,
                        Ok(AgentEvent::LLMContentChunk(s)) => {
                            print!("{s}");
                            io::stdout().flush()?;
                        }
                        Ok(AgentEvent::LLMEnd(msg)) => {
                            if !msg.content.is_empty() {
                                println!();
                            }
                            if msg.tool_calls.is_empty() {
                                break;
                            }
                        }
                        Ok(AgentEvent::ToolCallStart(c)) => {
                            println!("\n[Tool: {}]: {:?}", c.name, c.arguments);
                        }
                        Ok(AgentEvent::ToolCallEnd(_)) => {}
                        Ok(AgentEvent::Aborted(target)) => {
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
                        Ok(AgentEvent::Error(err)) => {
                            eprintln!("\nError: {}", err);
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
