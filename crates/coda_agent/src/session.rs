use std::path::PathBuf;

use coda_core::llm::{Message, ToolCall};
use serde::{Deserialize, Serialize};

use crate::agent::{AgentCheckpoint, TodoItem};

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

/// Persistent session storage backed by a directory on the filesystem.
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Return all sessions sorted by `updated_at` descending (most recent first).
    pub fn list(&self) -> Vec<SessionMeta> {
        let dir = &self.dir;
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

    pub fn load(&self, id: &str) -> Result<SessionData, Box<dyn std::error::Error>> {
        let path = &self.dir.join(format!("{}.json", id));
        let bytes = std::fs::read(path)?;
        let data = serde_json::from_slice(&bytes)?;
        Ok(data)
    }

    /// Save a suspended checkpoint. Preserves `pending_calls` and `auto_calls` so the
    /// session can resume directly into the approval flow on next startup.
    pub fn save_checkpoint(
        &self,
        session_id: Option<&str>,
        checkpoint: &AgentCheckpoint,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.save_inner(
            session_id,
            &checkpoint.messages,
            &checkpoint.todos,
            &checkpoint.pending_calls,
            &checkpoint.auto_calls,
        )
    }

    /// Save a completed turn. No pending tool calls are stored.
    pub fn save(
        &self,
        session_id: Option<&str>,
        messages: &[Message],
        todos: &[TodoItem],
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.save_inner(session_id, messages, todos, &[], &[])
    }

    fn save_inner(
        &self,
        session_id: Option<&str>,
        messages: &[Message],
        todos: &[TodoItem],
        pending_calls: &[ToolCall],
        auto_calls: &[ToolCall],
    ) -> Result<String, Box<dyn std::error::Error>> {
        let dir = &self.dir;
        std::fs::create_dir_all(&dir)?;

        // Filter out System messages.
        let messages: Vec<Message> = messages
            .iter()
            .filter(|m| !matches!(m, Message::System(_)))
            .cloned()
            .collect();

        let now = jiff::Timestamp::now().as_second().max(0) as u64;
        let id = session_id.map(|s| s.to_string()).unwrap_or_else(|| {
            let now = jiff::Zoned::now();
            format!("{}_{}", now.strftime("%Y%m%d_%H%M%S"), uuid::Uuid::new_v4())
        });

        // If updating an existing session preserve created_at.
        let created_at = if let Some(sid) = session_id {
            self.load(sid).map(|d| d.created_at).unwrap_or(now)
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
}
