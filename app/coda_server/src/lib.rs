pub mod agents;
pub mod ask_user;
pub mod config;
pub mod mcp;
pub mod storage;
pub mod transport;
pub mod wire;

use coda_agent::EnvRenderer;
use coda_skills::Skills;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

pub static SYSTEM_PROMPT: &str = include_str!("system-prompt.md");
pub static AGENT_SKILLS_PROMPT: &str = include_str!("agent-skills-prompt.md");

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

/// A selectable field of the per-turn environment context block. Agents declare
/// the set they want via the `env:` frontmatter list; omitting it defaults to
/// just [`EnvField::Date`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvField {
    /// Today's date — the one truly volatile field, recomputed every turn.
    Date,
    /// OS, architecture, and (best-effort) OS version.
    System,
    /// The current shell.
    Shell,
    /// The agent's workspace directory.
    Workspace,
}

/// The default env field set when an agent omits `env:` — date only.
pub fn default_env_fields() -> Vec<EnvField> {
    vec![EnvField::Date]
}

/// Build the workspace-knowledge segment: the workspace's skills followed by its
/// custom instructions (`AGENTS.md`). Returns an empty string when neither is
/// present, so callers can treat "no knowledge" uniformly.
///
/// This is pure and quiet: it is re-run on every watcher poll (a few seconds
/// apart), so it must not log per call. A missing skills directory — the common
/// case — is silently "no skills"; the watcher logs once when the rendered text
/// actually changes.
pub fn build_workspace_knowledge(workspace_dir: &str) -> String {
    let mut out = String::new();

    let skills_dir = Path::new(workspace_dir).join(".coda").join("skills");
    if skills_dir.exists() {
        match Skills::from_dir(&skills_dir) {
            Ok(skills) if !skills.0.is_empty() => {
                out.push_str(AGENT_SKILLS_PROMPT);
                out.push('\n');
                out.push_str(&skills.to_xml());
            }
            Ok(_) => {}
            Err(err) => {
                warn!("failed to load skills, proceeding without them: {err}");
            }
        }
    }

    if let Some(instructions) = read_custom_instructions(workspace_dir) {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("<custom_instructions>\n");
        out.push_str(&instructions);
        out.push_str("\n</custom_instructions>");
    }

    out
}

/// Build a per-turn environment-context renderer for an agent rooted at
/// `workspace_dir` and wanting `fields`. The static values (OS, shell, workspace
/// path) are computed once, here, and only for the fields actually requested —
/// so a `[date]`-only agent never spawns the OS-version probe. Only the
/// date is recomputed on each call, so it never goes stale in a long session.
/// Returns `None` when `fields` is empty (no env block).
///
/// The renderer captures its `workspace_dir`, so a per-agent workspace in Phase
/// 2 only changes the captured value — not the shape of this seam.
pub fn make_env_renderer(workspace_dir: String, fields: Vec<EnvField>) -> Option<EnvRenderer> {
    if fields.is_empty() {
        return None;
    }

    let system = fields.contains(&EnvField::System).then(|| {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        format!(
            "  <os>{os}({arch})</os>{}",
            get_os_version()
                .map(|v| format!("\n  <os_version>{v}</os_version>"))
                .unwrap_or_default()
        )
    });
    // The `shell` tool always executes via `bash -c`, regardless of the host's
    // login shell, so the advertised shell is fixed to its concrete backend.
    let shell = fields
        .contains(&EnvField::Shell)
        .then(|| "  <shell>bash</shell>".to_string());
    let workspace = fields
        .contains(&EnvField::Workspace)
        .then(|| format!("  <workspace>{workspace_dir}</workspace>"));

    Some(Arc::new(move || {
        let mut lines = Vec::with_capacity(fields.len());
        for field in &fields {
            // The static fields are `Some` exactly when requested, and we only
            // reach their arm while iterating a requested field.
            let line = match field {
                EnvField::Date => format!("  <date>{}</date>", jiff::Zoned::now().date()),
                EnvField::System => system.clone().expect("system computed when requested"),
                EnvField::Shell => shell.clone().expect("shell computed when requested"),
                EnvField::Workspace => workspace
                    .clone()
                    .expect("workspace computed when requested"),
            };
            lines.push(line);
        }
        format!(
            "<environment_context>\n{}\n</environment_context>",
            lines.join("\n")
        )
    }))
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
    fn build_workspace_knowledge_includes_custom_instructions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CUSTOM_INSTRUCTIONS_FILE),
            "always write tests.",
        )
        .unwrap();
        let knowledge = build_workspace_knowledge(&dir.path().to_string_lossy());
        assert!(knowledge.contains("<custom_instructions>"));
        assert!(knowledge.contains("always write tests."));
    }

    #[test]
    fn build_workspace_knowledge_empty_without_skills_or_instructions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(build_workspace_knowledge(&dir.path().to_string_lossy()).is_empty());
    }

    #[test]
    fn build_workspace_knowledge_empty_for_present_but_empty_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".coda").join("skills")).unwrap();
        assert!(build_workspace_knowledge(&dir.path().to_string_lossy()).is_empty());
    }

    #[test]
    fn env_renderer_recomputes_date_and_honors_fields() {
        let renderer =
            make_env_renderer("/ws".to_string(), vec![EnvField::Date, EnvField::Workspace])
                .expect("non-empty fields yield a renderer");
        let rendered = renderer();
        assert!(rendered.contains("<date>"));
        assert!(rendered.contains("<workspace>/ws</workspace>"));
        assert!(!rendered.contains("<shell>"));
        assert!(!rendered.contains("<os>"));
    }

    #[test]
    fn env_renderer_none_for_empty_fields() {
        assert!(make_env_renderer("/ws".to_string(), vec![]).is_none());
    }
}
