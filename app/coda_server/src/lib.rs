pub mod agents;
pub mod ask_user;
pub mod config;
pub mod files;
pub mod hub;
pub mod jsonb;
pub mod mcp;
pub mod rpc;
pub mod schema;
pub mod storage;
pub mod transport;
pub mod wire;

use coda_agent::{SharedSystemPrompt, VarsProvider};
use coda_skills::Skills;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

pub static SYSTEM_PROMPT: &str = include_str!("system-prompt.md");
pub static AGENT_SKILLS_PROMPT: &str = include_str!("agent-skills-prompt.md");

/// Git HEAD short SHA captured at build time by `build.rs` (or "unknown" when the
/// source wasn't a git checkout).
pub const GIT_SHA: &str = env!("CODA_GIT_SHA");

/// Build version: the crate version paired with the build-time git SHA, e.g.
/// `0.1.0 (1bc5b49)`. Used as the `--version` string and in the startup log.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CODA_GIT_SHA"), ")");

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

/// One skill as the composer's `/` picker offers it — the same name and
/// description the model reads inside `<available_skills>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// The workspace's skills, read once and rendered two ways: the
/// `<available_skills>` XML the model sees, and the name/description pairs the
/// composer's `/` picker lists. One read serves both, so the menu can never
/// name a skill the prompt omits (or vice versa).
///
/// Pure and quiet: it is re-run on every watcher poll (a few seconds apart), so
/// it must not log per call. A missing skills directory — the common case — is
/// silently "no skills"; the watcher logs once when the rendered text changes.
/// A `SKILL.md` that fails to parse takes the whole set down, for the prompt and
/// the picker alike.
pub fn load_workspace_skills(workspace_dir: &str) -> (String, Vec<SkillSummary>) {
    let skills_dir = Path::new(workspace_dir).join(".coda").join("skills");
    if !skills_dir.exists() {
        return (String::new(), Vec::new());
    }
    match Skills::from_dir(&skills_dir) {
        Ok(skills) if !skills.0.is_empty() => {
            let xml = skills.to_xml();
            let mut summaries: Vec<SkillSummary> = skills
                .0
                .into_iter()
                .map(|skill| SkillSummary {
                    name: skill.properties.name,
                    description: skill.properties.description,
                })
                .collect();
            summaries.sort_by(|a, b| a.name.cmp(&b.name));
            (xml, summaries)
        }
        Ok(_) => (String::new(), Vec::new()),
        Err(err) => {
            warn!("failed to load skills, proceeding without them: {err}");
            (String::new(), Vec::new())
        }
    }
}

/// The parsed skills list behind a shared handle, refreshed in place by the
/// workspace watcher exactly like the prompt handles beside it.
#[derive(Clone, Default)]
pub struct SharedSkills(Arc<std::sync::Mutex<Arc<Vec<SkillSummary>>>>);

impl SharedSkills {
    pub fn new(skills: Vec<SkillSummary>) -> Self {
        SharedSkills(Arc::new(std::sync::Mutex::new(Arc::new(skills))))
    }

    /// The current list. Cloning the inner `Arc` keeps the lock uncontended
    /// while a caller reads.
    pub fn get(&self) -> Arc<Vec<SkillSummary>> {
        Arc::clone(&self.lock())
    }

    pub fn set(&self, skills: Vec<SkillSummary>) {
        *self.lock() = Arc::new(skills);
    }

    /// The guarded sections only clone an `Arc`, so a poisoned lock means an
    /// unrelated panic and the list behind it is still good data.
    fn lock(&self) -> std::sync::MutexGuard<'_, Arc<Vec<SkillSummary>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The workspace's `AGENTS.md` wrapped in a `<custom_instructions>` block, or an
/// empty string when absent/blank. This is the value of the
/// `{{workspace_custom_instructions}}` template variable. Like
/// [`load_workspace_skills`], this is re-run on every watcher poll and must
/// stay quiet.
pub fn build_workspace_custom_instructions(workspace_dir: &str) -> String {
    match read_custom_instructions(workspace_dir) {
        Some(instructions) => {
            format!("<custom_instructions>\n{instructions}\n</custom_instructions>")
        }
        None => String::new(),
    }
}

/// The per-workspace, hot-reloaded knowledge handles feeding the dynamic
/// template variables. A per-workspace watcher refreshes each in place; the vars
/// provider reads them on every turn, so edits reach the prompt without
/// rebuilding agents. The skills *guide* needs no handle — it is a compile-time
/// constant ([`AGENT_SKILLS_PROMPT`]).
#[derive(Clone)]
pub struct WorkspaceKnowledge {
    /// `{{workspace_available_skills}}` — the `<available_skills>` XML, or empty.
    pub available_skills: SharedSystemPrompt,
    /// The same skills, parsed, for the composer's `/` picker. Refreshed from
    /// the same read as `available_skills` so the two never disagree.
    pub skills: SharedSkills,
    /// `{{workspace_custom_instructions}}` — `AGENTS.md` wrapped in
    /// `<custom_instructions>`, or empty.
    pub custom_instructions: SharedSystemPrompt,
}

impl WorkspaceKnowledge {
    /// Handles seeded from the workspace's current on-disk state.
    pub fn load(workspace_dir: &str) -> Self {
        let (available_skills, skills) = load_workspace_skills(workspace_dir);
        WorkspaceKnowledge {
            available_skills: SharedSystemPrompt::new(available_skills),
            skills: SharedSkills::new(skills),
            custom_instructions: SharedSystemPrompt::new(build_workspace_custom_instructions(
                workspace_dir,
            )),
        }
    }

    /// Empty handles, for an agent with no workspace knowledge (e.g. tests).
    pub fn empty() -> Self {
        WorkspaceKnowledge {
            available_skills: SharedSystemPrompt::new(String::new()),
            skills: SharedSkills::default(),
            custom_instructions: SharedSystemPrompt::new(String::new()),
        }
    }
}

/// Build the per-turn template-variable provider for an agent rooted at
/// `workspace_dir`. Every binding a prompt can reference is exposed here:
///
/// - `date` — today's date, recomputed on each call so it never goes stale.
/// - `os` — OS, architecture, and (best-effort) version, e.g. `macos(aarch64) 15.6`.
/// - `shell` — the interpreter the `shell` tool runs commands through (`bash`).
/// - `workspace` — the agent's workspace directory.
/// - `skills_guide` — the constant skills usage guide ([`AGENT_SKILLS_PROMPT`]).
/// - `workspace_available_skills` — the workspace's `<available_skills>` XML (or empty).
/// - `workspace_custom_instructions` — the workspace's `AGENTS.md` (or empty).
///
/// The static values (os, shell, workspace, skills guide) are fixed once, here;
/// the date is produced fresh per call, and the two knowledge bindings are read
/// from their hot-reloaded handles per call. The base body (`AGENT.md` /
/// built-in template) is the only thing scanned for `{{name}}` placeholders —
/// a binding's value is never re-scanned, so authored content (`AGENTS.md`, a
/// skill description) is never treated as a template. Unreferenced bindings
/// simply don't appear.
pub fn make_vars_provider(workspace_dir: String, knowledge: WorkspaceKnowledge) -> VarsProvider {
    let os = {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        match get_os_version() {
            Some(version) => format!("{os}({arch}) {version}"),
            None => format!("{os}({arch})"),
        }
    };
    // The `shell` tool always executes via `bash -c`, regardless of the host's
    // login shell, so the advertised shell is fixed to its concrete backend.
    let shell = "bash".to_string();

    Arc::new(move || {
        vec![
            ("date".to_string(), jiff::Zoned::now().date().to_string()),
            ("os".to_string(), os.clone()),
            ("shell".to_string(), shell.clone()),
            ("workspace".to_string(), workspace_dir.clone()),
            ("skills_guide".to_string(), AGENT_SKILLS_PROMPT.to_string()),
            (
                "workspace_available_skills".to_string(),
                knowledge.available_skills.get(),
            ),
            (
                "workspace_custom_instructions".to_string(),
                knowledge.custom_instructions.get(),
            ),
        ]
    })
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
    fn build_workspace_custom_instructions_wraps_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CUSTOM_INSTRUCTIONS_FILE),
            "always write tests.",
        )
        .unwrap();
        let ci = build_workspace_custom_instructions(&dir.path().to_string_lossy());
        assert!(ci.starts_with("<custom_instructions>"));
        assert!(ci.contains("always write tests."));
        assert!(ci.ends_with("</custom_instructions>"));
    }

    #[test]
    fn build_workspace_custom_instructions_empty_without_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        assert!(build_workspace_custom_instructions(&dir.path().to_string_lossy()).is_empty());
    }

    #[test]
    fn load_workspace_skills_empty_without_or_for_empty_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        // No `.coda/skills` at all.
        let (xml, skills) = load_workspace_skills(&dir.path().to_string_lossy());
        assert!(xml.is_empty());
        assert!(skills.is_empty());
        // Present but empty.
        std::fs::create_dir_all(dir.path().join(".coda").join("skills")).unwrap();
        let (xml, skills) = load_workspace_skills(&dir.path().to_string_lossy());
        assert!(xml.is_empty());
        assert!(skills.is_empty());
    }

    #[test]
    fn vars_provider_exposes_all_bindings() {
        let knowledge = WorkspaceKnowledge {
            available_skills: SharedSystemPrompt::new("<available_skills>x</available_skills>"),
            skills: SharedSkills::default(),
            custom_instructions: SharedSystemPrompt::new(
                "<custom_instructions>y</custom_instructions>",
            ),
        };
        let vars = make_vars_provider("/ws".to_string(), knowledge)();
        let lookup = |name: &str| {
            vars.iter()
                .find(|(var, _)| var == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(lookup("workspace"), Some("/ws"));
        assert_eq!(lookup("shell"), Some("bash"));
        assert!(lookup("date").is_some_and(|d| !d.is_empty()));
        assert!(lookup("os").is_some_and(|os| os.contains(std::env::consts::OS)));
        assert_eq!(lookup("skills_guide"), Some(AGENT_SKILLS_PROMPT));
        assert_eq!(
            lookup("workspace_available_skills"),
            Some("<available_skills>x</available_skills>")
        );
        assert_eq!(
            lookup("workspace_custom_instructions"),
            Some("<custom_instructions>y</custom_instructions>")
        );
    }

    #[test]
    fn vars_provider_recomputes_date_and_rereads_handles_per_call() {
        // The date is fresh each call, and the knowledge bindings reflect
        // in-place handle updates (hot-reload) on the next call.
        let knowledge = WorkspaceKnowledge::empty();
        let provider = make_vars_provider("/ws".to_string(), knowledge.clone());
        let lookup = |vars: &[(String, String)], name: &str| {
            vars.iter()
                .find(|(var, _)| var == name)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        let first = provider();
        assert_eq!(
            lookup(&first, "date"),
            jiff::Zoned::now().date().to_string()
        );
        assert_eq!(lookup(&first, "workspace_available_skills"), "");

        knowledge.available_skills.set("<available_skills/>");
        assert_eq!(
            lookup(&provider(), "workspace_available_skills"),
            "<available_skills/>"
        );
    }
}
