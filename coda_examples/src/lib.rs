pub mod ask_user;
pub mod mcp;
pub mod storage;
pub mod wire;

use coda_agent::{AgentSpec, SubAgentMode};
use coda_skills::Skills;
use coda_tools::{ToolSpec, builtin_specs};
use std::path::Path;
use tracing::{info, warn};
use uuid::Uuid;

pub static SYSTEM_PROMPT: &str = include_str!("system-prompt.md");
pub static AGENT_SKILLS_PROMPT: &str = include_str!("agent-skills-prompt.md");

pub const LOGO: &str = r#"
  ██████╗ ██████╗ ██████╗  █████╗
 ██╔════╝██╔═══██╗██╔══██╗██╔══██╗
 ██║     ██║   ██║██║  ██║███████║
 ██║     ██║   ██║██║  ██║██╔══██║
 ╚██████╗╚██████╔╝██████╔╝██║  ██║
  ╚═════╝ ╚═════╝ ╚═════╝ ╚═╝  ╚═╝
"#;

pub fn print_logo(subtitle: &str) {
    println!("\x1b[1;38;2;242;123;115m{}\x1b[0m", LOGO);
    println!("\x1b[2;37m  {subtitle}\x1b[0m");
    println!();
}

pub fn build_system_prompt(workspace_dir: &str) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();

    match Skills::from_dir(&Path::new(workspace_dir).join(".coda").join("skills")) {
        Ok(skills) => {
            info!("loaded {} skills", skills.0.len());
            prompt.push_str("\n---\n");
            prompt.push_str(AGENT_SKILLS_PROMPT);
            prompt.push('\n');
            prompt.push_str(&skills.to_xml());
        }
        Err(err) => {
            warn!("failed to load skills, proceeding without them: {err}");
        }
    }

    prompt.push_str("\n\n");
    prompt.push_str(&build_environment_context(workspace_dir));

    prompt
}

fn build_environment_context(workspace_dir: &str) -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let os_version = get_os_version()
        .map(|v| format!("\n  <os_version>{v}</os_version>"))
        .unwrap_or_default();
    let today = jiff::Zoned::now().date();
    let shell = get_current_shell();

    format!(
        "\
<environment_context>
  <workspace>{workspace_dir}</workspace>
  <os>{os}({arch})</os>{os_version}
  <date>{today}</date>
  <shell>{shell}</shell>
</environment_context>"
    )
}

#[cfg(unix)]
fn get_current_shell() -> String {
    let ppid = std::os::unix::process::parent_id();
    std::process::Command::new("ps")
        .args(["-p", &ppid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().rsplit('/').next().unwrap_or("unknown").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn get_os_version() -> Option<String> {
    if cfg!(target_os = "macos") {
        std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    } else {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    }
}

pub fn build_agent_spec(system_prompt: String, extra_tools: Vec<Box<dyn ToolSpec>>) -> AgentSpec {
    let mut tools = builtin_specs();
    tools.extend(extra_tools);

    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt,
        mode: SubAgentMode::Stateful,
        tools,
        subagents: vec![
            AgentSpec {
                name: "explore".into(),
                description: "An explore sub-agent that can read files, search code, and explore \
                              the codebase. Delegate exploration and research tasks to it."
                    .into(),
                system_prompt:
                    "You are an exploration assistant. You can read files, search code, and list \
                     directories. Summarize your findings concisely."
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
                    "You are a simple memo agent. Your only job is to remember what the user \
                     tells you and answer questions about it. Keep your replies very brief."
                        .to_string(),
                mode: SubAgentMode::Stateful,
                tools: vec![],
                subagents: vec![],
            },
        ],
    }
}

pub fn parse_session_id_arg(
    args: impl IntoIterator<Item = String>,
) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    let mut session_id = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resume" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --resume".to_string())?;
                Uuid::parse_str(&value)
                    .map_err(|err| format!("invalid session id '{value}': {err}"))?;
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
