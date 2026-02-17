mod core;
mod provider;

use core::session::Session;
use core::tool::{Tool, ToolError, ToolManager, ToolResult};
use dotenvy::dotenv;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Write};
use tokio::process::Command;
use tracing::{debug, info};

use crate::core::llm::{
    ChatCompletionRequest, LLM, Message, SystemMessage, ToolMessage, UserMessage,
};
use crate::provider::openai::OpenAI;

static SYSTEM_PROMPT: &str = include_str!("./system-prompt.md");

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

struct ShellTool {
    schema: Schema,
    description: String,
}

impl ShellTool {
    fn new() -> Self {
        let description = format!(
            "Execute shell commands and return stdout and stderr. Your are in a Unix environment, cat, ls, echo, pwd, date, and other basic commands are available."
        );

        ShellTool {
            schema: schemars::schema_for!(ShellToolParams),
            description,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ShellToolParams {
    command: String,
}

impl Tool for ShellTool {
    type Parameters = ShellToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    fn execute(
        &self,
        params: Self::Parameters,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move {
            info!("Executing shell command: {}", params.command);
            let output = Command::new("sh")
                .arg("-c")
                .arg(&params.command)
                .output()
                .await
                .map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to execute command: {}", e))
                })?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                Ok(format!("{}", stdout))
            } else {
                Ok(format!(
                    "exit code: {}\nstdout: {}\nstderr: {}",
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
    }
}

struct Agent {
    name: String,
    session: Session,
    tools: ToolManager,
}

impl Agent {
    fn new(name: String) -> Self {
        Agent {
            name,
            session: Session::new(),
            tools: ToolManager::new(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv()?;

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();

    let mut agent = Agent::new("coda".to_string());

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    agent.session.add_message(Message::System(SystemMessage(
        SYSTEM_PROMPT.replace("{{OS}}", &format!("{}({})", os, arch)),
    )));

    agent.tools.register(ShellTool::new());

    let provider = OpenAI::new(LLM {
        api_key: env::var("OPENAI_API_KEY").unwrap(),
        base_url: env::var("OPENAI_BASE_URL").unwrap(),
        stream: true,
    });

    print_logo();
    println!("Type 'quit', 'exit', or 'q' to stop");

    // Main conversation loop
    loop {
        // Get user input
        print!("\nYou: ");
        io::stdout().flush()?;

        let mut user_input = String::new();
        io::stdin().read_line(&mut user_input)?;
        let user_input = user_input.trim();

        // Check for exit commands
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

        // Add user message to session
        agent
            .session
            .add_message(Message::User(UserMessage(user_input.to_string())));
        debug!("messages: {:?}\n", agent.session.messages());

        print!("Assistant: ");
        io::stdout().flush()?;

        // AI processing loop
        loop {
            let request = ChatCompletionRequest {
                model: env::var("OPENAI_MODEL").unwrap(),
                messages: agent.session.messages().to_vec(),
                tools: agent.tools.descriptors(),
                max_completion_tokens: Some(8000),
                temperature: Some(0.7),
            };
            let on_content = |content: String| async move {
                print!("{}", content);
            };
            let assistant_message = provider.stream(request, on_content).await?;
            if !assistant_message.content.is_empty() {
                println!();
            }
            let stop = assistant_message.tool_calls.is_empty();
            agent
                .session
                .add_message(Message::Assistant(assistant_message.clone()));

            if !assistant_message.tool_calls.is_empty() {
                let futures: Vec<_> = assistant_message
                    .tool_calls
                    .into_iter()
                    .map(|call| {
                        let tool = agent.tools.get(&call.name).expect("TODO: should exist");
                        async move {
                            let result = tool
                                .execute(call.arguments.unwrap_or_default())
                                .await
                                .expect("TODO: error handling");
                            ToolMessage {
                                id: call.id,
                                name: call.name,
                                result,
                            }
                        }
                    })
                    .collect();

                let results = futures::future::join_all(futures).await;
                for tool_message in results {
                    agent.session.add_message(Message::Tool(tool_message));
                }
            }
            debug!("messages: {:?}\n", agent.session.messages());
            if stop {
                break;
            }
        }
    }

    Ok(())
}
