use dotenvy::dotenv;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::debug;
use tracing::info;
use tracing::warn;

use coda_agent::{Agent, AgentEvent, ApprovalDecision, RejectedCall, RunConfig, ToolApprovalMode};
use coda_core::llm::{
    LLMProviderConfig, Message, StreamError, SystemMessage, ToolCall, UserMessage,
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

fn prompt_approval(calls: &[ToolCall]) -> Result<ApprovalDecision, Box<dyn std::error::Error>> {
    println!(
        "\n[Approval Required] The agent wants to run {} tool call(s):",
        calls.len()
    );

    let mut approved = vec![];
    let mut rejected = vec![];

    for (i, call) in calls.iter().enumerate() {
        println!(
            "\n  {}. {}: {}",
            i + 1,
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
                    approved.push(call.id.clone());
                    break;
                }
                "n" | "no" => {
                    print!("     Reason (optional, Enter to skip): ");
                    io::stdout().flush()?;
                    let mut reason = String::new();
                    io::stdin().read_line(&mut reason)?;
                    let reason = reason.trim().to_string();
                    rejected.push(RejectedCall {
                        id: call.id.clone(),
                        reason: if reason.is_empty() {
                            None
                        } else {
                            Some(reason)
                        },
                    });
                    break;
                }
                _ => println!("     Please enter 'y' or 'n'."),
            }
        }
    }

    Ok(ApprovalDecision { approved, rejected })
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
    let workspace_dir = std::env::current_dir()?.to_string_lossy().into_owned();
    let mut system_prompt = SYSTEM_PROMPT
        .replace("{{OS}}", &format!("{}({})", os, arch))
        .replace("{{WORKSPACE_DIR}}", &workspace_dir);

    match Skills::from_dir(&PathBuf::from_str("./skills").unwrap()) {
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

    let agent = Agent::new_with_default_tools(
        OpenAI::new(LLMProviderConfig {
            api_key,
            base_url,
            stream: true,
        }),
        workspace_dir,
    );

    agent
        .add_message(Message::System(SystemMessage(system_prompt)))
        .await;

    print_logo();
    println!("Type 'quit', 'exit', or 'q' to stop");

    // Main conversation loop
    loop {
        print!("\nYou: ");
        io::stdout().flush()?;

        let mut user_input = String::new();
        io::stdin().read_line(&mut user_input)?;
        let user_input = user_input.trim();

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

        print!("Assistant: ");
        io::stdout().flush()?;

        let thread_id = uuid::Uuid::new_v4().to_string();
        let make_config = |thread_id: String| RunConfig {
            model: model.clone(),
            max_completion_tokens: Some(5000),
            temperature: Some(0.5),
            thread_id,
            tool_approval: ToolApprovalMode::RequireWhen(std::sync::Arc::new(|call| {
                call.name == "shell"
            })),
        };

        // Explicit dyn Stream type so run() and resume() can be assigned to the same variable.
        let mut stream: std::pin::Pin<
            Box<dyn Stream<Item = Result<AgentEvent, StreamError>> + '_>,
        > = Box::pin(agent.run(UserMessage(user_input.to_string()), make_config(thread_id)));

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
                            "tool result: id={} name={} result={}",
                            m.id, m.name, m.result
                        );
                    }
                    AgentEvent::Suspended(checkpoint) => {
                        suspended_checkpoint = Some(checkpoint);
                        break;
                    }
                    _ => {}
                }
            }

            match suspended_checkpoint {
                None => break, // stream completed normally
                Some(checkpoint) => {
                    let decision = prompt_approval(&checkpoint.pending_calls)?;
                    let thread_id = checkpoint.thread_id.clone();
                    stream = Box::pin(agent.resume(checkpoint, decision, make_config(thread_id)));
                }
            }
        }
    }

    Ok(())
}
