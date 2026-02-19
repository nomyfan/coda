use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use coda_agent::TodoItem;
use coda_core::llm::{Message, ToolCall};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Conversation messages excluding the System message (regenerated on startup).
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
    /// Tool calls awaiting user approval at the time the process exited.
    /// When present, the session resumes directly into the approval flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_calls: Vec<ToolCall>,
}

/// Lightweight metadata used for the session picker list.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub message_count: usize,
}

fn sessions_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(".coda").join("sessions")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn derive_title(messages: &[Message]) -> String {
    for msg in messages {
        if let Message::User(u) = msg {
            let text = u.0.trim();
            if !text.is_empty() {
                let truncated: String = text.chars().take(60).collect();
                return if text.chars().count() > 60 {
                    format!("{}…", truncated)
                } else {
                    truncated
                };
            }
        }
    }
    "New session".to_string()
}

/// Return all sessions sorted by `updated_at` descending (most recent first).
pub fn list_sessions(workspace_dir: &Path) -> Vec<SessionMeta> {
    let dir = sessions_dir(workspace_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };

    let mut metas: Vec<SessionMeta> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let bytes = std::fs::read(e.path()).ok()?;
            let data: SessionData = serde_json::from_slice(&bytes).ok()?;
            Some(SessionMeta {
                id: data.id,
                title: data.title,
                updated_at: data.updated_at,
                message_count: data.messages.len(),
            })
        })
        .collect();

    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    metas
}

/// Save the current session to `{workspace_dir}/.coda/sessions/{id}.json`.
///
/// Only non-System messages are stored; the system prompt is regenerated on startup.
/// If `id` is `None` a new UUID is generated. Returns the session id.
pub fn save_session(
    workspace_dir: &Path,
    session_id: Option<&str>,
    messages: &[Message],
    todos: &[TodoItem],
    pending_calls: &[ToolCall],
    auto_calls: &[ToolCall],
) -> Result<String, Box<dyn std::error::Error>> {
    let dir = sessions_dir(workspace_dir);
    std::fs::create_dir_all(&dir)?;

    // Filter out System messages.
    let messages: Vec<Message> = messages
        .iter()
        .filter(|m| !matches!(m, Message::System(_)))
        .cloned()
        .collect();

    let now = now_secs();
    let id = session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // If updating an existing session preserve created_at.
    let created_at = if let Some(sid) = session_id {
        load_session(workspace_dir, sid)
            .map(|d| d.created_at)
            .unwrap_or(now)
    } else {
        now
    };

    let data = SessionData {
        title: derive_title(&messages),
        id: id.clone(),
        created_at,
        updated_at: now,
        messages,
        todos: todos.to_vec(),
        pending_calls: pending_calls.to_vec(),
        auto_calls: auto_calls.to_vec(),
    };

    let path = dir.join(format!("{}.json", id));
    let bytes = serde_json::to_vec_pretty(&data)?;
    std::fs::write(path, bytes)?;
    Ok(id)
}

pub fn load_session(
    workspace_dir: &Path,
    id: &str,
) -> Result<SessionData, Box<dyn std::error::Error>> {
    let path = sessions_dir(workspace_dir).join(format!("{}.json", id));
    let bytes = std::fs::read(path)?;
    let data = serde_json::from_slice(&bytes)?;
    Ok(data)
}
