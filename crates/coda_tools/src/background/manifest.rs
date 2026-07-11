//! Persisted per-task metadata — the small `meta.json` that indexes a task's
//! spool. Output *bytes* live only in the ring files; this manifest carries the
//! identity, timing, terminal status, per-stream ring layout, and cleanup
//! disposition needed to reopen or reclaim a task by id.

use serde::{Deserialize, Serialize};

use super::TaskStatus;
use super::disk_tail::{LAYOUT_VERSION, MAX_CAPACITY, MIN_CAPACITY};
use super::task_id::TaskId;

/// Current on-disk manifest format. A future breaking change bumps this;
/// an unknown version is treated as corrupt.
pub const MANIFEST_VERSION: u32 = 1;

/// Full manifest persisted as `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutputManifest {
    pub manifest_version: u32,
    pub id: TaskId,
    pub command: String,
    pub description: String,
    pub agent_name: String,
    pub started_at: jiff::Timestamp,
    pub terminal_at: Option<jiff::Timestamp>,
    pub status: TaskStatus,
    pub stdout: StreamManifest,
    pub stderr: StreamManifest,
    pub output: OutputDisposition,
}

/// One stream's persisted ring layout and read cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamManifest {
    /// Physical layout version of the ring file; an unknown value is rejected.
    pub layout_version: u32,
    /// Capacity the file was created with. Reopen uses exactly this — never the
    /// current default — so the `offset % capacity` mapping stays valid.
    pub capacity: u64,
    pub start_offset: u64,
    pub total_written: u64,
    /// Model read cursor (absolute logical offset).
    pub read_cursor: u64,
    /// 0..=3 bytes past `read_cursor` that were consumed as raw bytes but do
    /// not yet form a complete UTF-8 scalar; prepended to the next decode.
    pub utf8_carry: Vec<u8>,
}

impl StreamManifest {
    /// A brand-new empty stream at the given capacity.
    pub fn empty(capacity: u64) -> Self {
        StreamManifest {
            layout_version: LAYOUT_VERSION,
            capacity,
            start_offset: 0,
            total_written: 0,
            read_cursor: 0,
            utf8_carry: Vec::new(),
        }
    }

    pub fn fully_consumed(&self) -> bool {
        self.read_cursor == self.total_written
    }

    /// Structural checks independent of the ring file, per the design's reopen
    /// validation list. Ring-file length is checked separately by `DiskTail`.
    pub fn validate(&self) -> Result<(), String> {
        if self.layout_version != LAYOUT_VERSION {
            return Err(format!(
                "unsupported layout_version {}",
                self.layout_version
            ));
        }
        if !(MIN_CAPACITY..=MAX_CAPACITY).contains(&self.capacity) {
            return Err(format!("capacity {} out of range", self.capacity));
        }
        if self.start_offset > self.total_written {
            return Err("start_offset exceeds total_written".into());
        }
        if self.total_written - self.start_offset > self.capacity {
            return Err("retained window exceeds capacity".into());
        }
        if self.start_offset != self.total_written.saturating_sub(self.capacity) {
            return Err("start_offset is not the ring-implied value".into());
        }
        if self.read_cursor > self.total_written {
            return Err("read_cursor exceeds total_written".into());
        }
        if self.utf8_carry.len() > 3 {
            return Err("utf8_carry longer than 3 bytes".into());
        }
        if !is_utf8_prefix(&self.utf8_carry) {
            return Err("utf8_carry is not an incomplete UTF-8 prefix".into());
        }
        Ok(())
    }
}

/// Cleanup disposition of a terminal task's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OutputDisposition {
    /// Rings still present and readable.
    Retained,
    /// Fully read by the model, then rings deleted.
    Consumed { at: jiff::Timestamp },
    /// Evicted to reclaim quota before being fully read; rings deleted.
    Expired {
        at: jiff::Timestamp,
        reason: ExpireReason,
    },
}

impl OutputDisposition {
    /// Whether the ring files should still exist for this disposition.
    pub fn rings_present(&self) -> bool {
        matches!(self, OutputDisposition::Retained)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireReason {
    SessionQuota,
}

/// True if `bytes` is a strict prefix of some UTF-8 scalar's encoding — i.e. a
/// partial multi-byte sequence, never a complete or invalid one. Used to reject
/// a manifest whose carry could not have come from a real byte boundary.
fn is_utf8_prefix(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.len() > 3 {
        return false;
    }
    // Must not already decode to a complete string (that would not be a carry).
    if std::str::from_utf8(bytes).is_ok() {
        return false;
    }
    // Appending up to 3 continuation bytes must be able to complete it; if some
    // completion decodes, it is a valid incomplete prefix.
    for fill in 1..=3u8 {
        let mut probe = bytes.to_vec();
        probe.extend(std::iter::repeat_n(0x80, fill as usize));
        if std::str::from_utf8(&probe).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_stream() -> StreamManifest {
        StreamManifest {
            layout_version: LAYOUT_VERSION,
            capacity: MIN_CAPACITY,
            start_offset: 0,
            total_written: 10,
            read_cursor: 5,
            utf8_carry: Vec::new(),
        }
    }

    #[test]
    fn valid_stream_passes() {
        base_stream().validate().unwrap();
    }

    #[test]
    fn rejects_bad_layout_and_capacity() {
        let mut s = base_stream();
        s.layout_version = 99;
        assert!(s.validate().is_err());
        let mut s = base_stream();
        s.capacity = MIN_CAPACITY - 1;
        assert!(s.validate().is_err());
        let mut s = base_stream();
        s.capacity = MAX_CAPACITY + 1;
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_inconsistent_offsets() {
        let mut s = base_stream();
        s.read_cursor = 999;
        assert!(s.validate().is_err());
        // total 10 within one capacity → start must be 0.
        let mut s = base_stream();
        s.start_offset = 2;
        assert!(s.validate().is_err());
    }

    #[test]
    fn utf8_carry_prefix_rules() {
        // A lone lead byte of a 3-byte sequence is a valid carry.
        let mut s = base_stream();
        s.utf8_carry = vec![0xE2];
        s.validate().unwrap();
        // A complete scalar is not a carry.
        let mut s = base_stream();
        s.utf8_carry = b"a".to_vec();
        assert!(s.validate().is_err());
        // Too long.
        let mut s = base_stream();
        s.utf8_carry = vec![0xF0, 0x9F, 0x98, 0x80];
        assert!(s.validate().is_err());
        // Pure garbage that can never complete.
        let mut s = base_stream();
        s.utf8_carry = vec![0xFF];
        assert!(s.validate().is_err());
    }

    #[test]
    fn manifest_json_roundtrip() {
        let m = TaskOutputManifest {
            manifest_version: MANIFEST_VERSION,
            id: TaskId::new(),
            command: "echo hi".into(),
            description: "d".into(),
            agent_name: "coda".into(),
            started_at: jiff::Timestamp::now(),
            terminal_at: None,
            status: TaskStatus::Running,
            stdout: StreamManifest::empty(MIN_CAPACITY),
            stderr: StreamManifest::empty(MIN_CAPACITY),
            output: OutputDisposition::Retained,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: TaskOutputManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, m.id);
        assert_eq!(back.status, m.status);
        assert_eq!(back.output, m.output);
    }
}
