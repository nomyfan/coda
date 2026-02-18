use dotenvy::dotenv;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{debug, info, warn};

use coda_agent::agent::Agent;
use coda_agent::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};
use coda_core::llm::{
    ChatCompletionRequest, LLMProvider, LLMProviderConfig, Message, SystemMessage, ToolMessage,
    UserMessage,
};
use coda_openai::OpenAI;
use coda_skills::Skills;

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
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let mut system_prompt = format!(
        "{}",
        SYSTEM_PROMPT
            .replace("{{OS}}", &format!("{}({})", os, arch))
            .replace("{{CWD}}", &cwd),
    );

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

    let mut agent = Agent::new(OpenAI::new(LLMProviderConfig {
        api_key,
        base_url,
        stream: true,
    }));

    agent
        .add_message(Message::System(SystemMessage(system_prompt)))
        .await;

    let state = agent.state();
    agent.tools.register(ShellTool::new());
    agent.tools.register(ReadFileTool::new());
    agent.tools.register(WriteFileTool::new());
    agent.tools.register(ListDirectoryTool::new());
    agent.tools.register(GrepTool::new(cwd.clone()));
    agent.tools.register(GlobTool::new(cwd));
    agent.tools.register(ReadTodosTool::new(state.clone()));
    agent.tools.register(WriteTodosTool::new(state));

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

        // Add user message
        agent
            .add_message(Message::User(UserMessage(user_input.to_string())))
            .await;
        debug!("messages: {:?}", agent.messages().await);

        print!("Assistant: ");
        io::stdout().flush()?;

        // AI processing loop
        loop {
            let request = ChatCompletionRequest {
                model: model.clone(),
                messages: agent.messages().await,
                tools: agent.tools.descriptors(),
                max_completion_tokens: Some(8000),
                temperature: Some(0.7),
            };
            let on_content = |content: String| async move {
                print!("{}", content);
            };
            let assistant_message = agent.provider.stream(request, on_content).await?;
            if !assistant_message.content.is_empty() {
                println!();
            }
            let stop = assistant_message.tool_calls.is_empty();
            agent
                .add_message(Message::Assistant(assistant_message.clone()))
                .await;
            debug!("messages: {:?}", agent.messages().await);

            if !assistant_message.tool_calls.is_empty() {
                let futures: Vec<_> = assistant_message
                    .tool_calls
                    .into_iter()
                    .map(|call| {
                        let tool = agent.tools.get(&call.name);
                        async move {
                            match tool {
                                Some(tool) => {
                                    let result =
                                        tool.execute(call.arguments.unwrap_or_default()).await;
                                    ToolMessage {
                                        id: call.id,
                                        name: call.name,
                                        result: match result {
                                            Ok(output) => output,
                                            Err(err) => format!("Error: {}", err),
                                        },
                                    }
                                }
                                None => ToolMessage {
                                    id: call.id,
                                    result: format!("Error: Tool '{}' not found", call.name),
                                    name: call.name,
                                },
                            }
                        }
                    })
                    .collect();

                let results = futures::future::join_all(futures).await;
                for tool_message in results {
                    agent.add_message(Message::Tool(tool_message)).await;
                }
            }
            debug!("messages: {:?}", agent.messages().await);
            if stop {
                break;
            }
        }
    }

    Ok(())
}
