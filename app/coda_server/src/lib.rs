pub mod ask_user;
pub mod config;
pub mod mcp;
pub mod storage;
pub mod transport;
pub mod wire;

use coda_agent::{AgentSpec, SubAgentMode, SystemPrompt};
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

/// Name of the custom-instructions file read from the workspace root.
pub const CUSTOM_INSTRUCTIONS_FILE: &str = "AGENTS.md";

/// Read the workspace's custom-instructions file (`AGENTS.md`), returning its
/// trimmed contents. Returns `None` when the file is absent, unreadable, or
/// blank so callers can simply skip the section.
pub fn read_custom_instructions(workspace_dir: &str) -> Option<String> {
    let path = Path::new(workspace_dir).join(CUSTOM_INSTRUCTIONS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!("failed to read {CUSTOM_INSTRUCTIONS_FILE}: {err}");
            None
        }
    }
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

    if let Some(instructions) = read_custom_instructions(workspace_dir) {
        info!("loaded custom instructions from {CUSTOM_INSTRUCTIONS_FILE}");
        prompt.push_str("\n\n<custom_instructions>\n");
        prompt.push_str(&instructions);
        prompt.push_str("\n</custom_instructions>");
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

pub fn build_agent_spec(
    system_prompt: impl Into<SystemPrompt>,
    extra_tools: Vec<Box<dyn ToolSpec>>,
) -> AgentSpec {
    let mut tools = builtin_specs();
    tools.extend(extra_tools);

    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: system_prompt.into(),
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
                        .into(),
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
                        .into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_custom_instructions_reads_trimmed_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CUSTOM_INSTRUCTIONS_FILE),
            "\n  be concise.\n\n",
        )
        .unwrap();
        let workspace = dir.path().to_string_lossy();
        assert_eq!(
            read_custom_instructions(&workspace),
            Some("be concise.".to_string())
        );
    }

    #[test]
    fn read_custom_instructions_missing_or_blank_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_string_lossy().into_owned();
        assert_eq!(read_custom_instructions(&workspace), None);

        std::fs::write(dir.path().join(CUSTOM_INSTRUCTIONS_FILE), "   \n\t\n").unwrap();
        assert_eq!(read_custom_instructions(&workspace), None);
    }

    #[test]
    fn build_system_prompt_includes_custom_instructions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CUSTOM_INSTRUCTIONS_FILE),
            "always write tests.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir.path().to_string_lossy());
        assert!(prompt.contains("<custom_instructions>"));
        assert!(prompt.contains("always write tests."));
    }
}
