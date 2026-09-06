//! Strong task identity — the only type allowed to name an archive path.
//!
//! A `TaskId` is `bg_` followed by exactly 32 lowercase ASCII hex digits. Model
//! input reaches the archive only after parsing into this type at the tool
//! boundary, so a raw `../`, an absolute path, or any non-canonical string can
//! never reach `PathBuf::join`/`openat`. Manifest-decoded ids are re-validated
//! through the same `FromStr` and must equal their parent directory name.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A validated background task id. `Serialize` emits the canonical string;
/// `Deserialize` re-parses it, so a manifest can never inject an invalid,
/// path-bearing value.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct TaskId(String);

/// Returned when a string is not a canonical task id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTaskId;

impl fmt::Display for InvalidTaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid task id (expected `bg_` + 32 lowercase hex digits)")
    }
}

impl std::error::Error for InvalidTaskId {}

impl TaskId {
    /// A delegation replay uses the same id in its session and calling thread.
    pub fn for_call(session_id: &str, thread_id: &str, origin: &crate::llm::MessageOrigin) -> Self {
        let identity = serde_json::to_vec(&(session_id, thread_id, origin))
            .expect("call identity contains only strings and message ids");
        let namespace = uuid::Uuid::from_u128(0xb385bbce_ac91_471e_b039_556afcbf8701);
        TaskId(format!(
            "bg_{}",
            uuid::Uuid::new_v5(&namespace, &identity).simple()
        ))
    }

    /// Mint a fresh id from a UUID v4, encoded canonically.
    pub fn new() -> Self {
        TaskId(format!("bg_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn notice_message_id(&self) -> crate::llm::MessageId {
        uuid::Uuid::new_v5(
            &uuid::Uuid::from_u128(0x605b8944_882a_489f_9abe_7601767537aa),
            self.as_str().as_bytes(),
        )
        .into()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TaskId {
    type Err = InvalidTaskId;

    /// Accepts only `bg_` + 32 lowercase hex digits. No trimming, no case
    /// folding: a non-canonical form is rejected outright.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let hex = raw.strip_prefix("bg_").ok_or(InvalidTaskId)?;
        let canonical = hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if canonical {
            Ok(TaskId(raw.to_owned()))
        } else {
            Err(InvalidTaskId)
        }
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// A thread invocation that must be cleaned before the thread can be reused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeMember {
    pub thread_id: String,
    pub invocation_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskOrigin {
    pub thread_id: String,
    pub message_origin: Option<crate::llm::MessageOrigin>,
    pub agent_path: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_identity_is_stable_and_includes_session_thread_and_message() {
        let origin = crate::llm::MessageOrigin {
            message_id: crate::llm::MessageId::new(),
            call_id: "call_1".into(),
        };
        let id = TaskId::for_call("session", "parent", &origin);
        assert_eq!(id, TaskId::for_call("session", "parent", &origin));
        assert_ne!(id, TaskId::for_call("other", "parent", &origin));
        assert_ne!(id, TaskId::for_call("session", "other", &origin));
        assert_ne!(
            id,
            TaskId::for_call(
                "session",
                "parent",
                &crate::llm::MessageOrigin {
                    message_id: crate::llm::MessageId::new(),
                    call_id: origin.call_id.clone(),
                }
            )
        );
        assert_ne!(
            id,
            TaskId::for_call(
                "session",
                "parent",
                &crate::llm::MessageOrigin {
                    message_id: origin.message_id,
                    call_id: "call_2".into(),
                }
            )
        );
    }

    #[test]
    fn new_is_canonical() {
        let id = TaskId::new();
        assert!(id.as_str().starts_with("bg_"));
        assert_eq!(id.as_str().len(), 35);
        assert_eq!(id.as_str(), &id.as_str().parse::<TaskId>().unwrap().0);
    }

    #[test]
    fn rejects_path_traversal_and_noncanonical() {
        for bad in [
            "../etc",
            "bg_../../etc",
            "/etc/passwd",
            "bg_",
            "bg_ABCDEF0123456789ABCDEF0123456789",  // uppercase
            "bg_0123",                              // too short
            "bg_0123456789abcdef0123456789abcdef0", // too long (33)
            "bg_0123456789abcdef0123456789abcdeg",  // non-hex 'g'
            "task_0123456789abcdef0123456789abcd",  // wrong prefix
            " bg_0123456789abcdef0123456789abcdef", // leading space
        ] {
            assert!(bad.parse::<TaskId>().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn accepts_canonical() {
        let ok = "bg_0123456789abcdef0123456789abcdef";
        assert_eq!(ok.parse::<TaskId>().unwrap().as_str(), ok);
    }

    #[test]
    fn serde_roundtrip_and_validation() {
        let id = TaskId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        // A malicious manifest string is rejected at deserialize time.
        assert!(serde_json::from_str::<TaskId>("\"../escape\"").is_err());
    }
}
