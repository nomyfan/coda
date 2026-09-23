use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Channel, OutputChannelRef, OutputId, OutputRef, StorageFailure};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: u64 = 1024 * MIB as u64;
const TIB: u64 = 1024 * GIB;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceLimits {
    pub output: OutputLimits,
    pub ptc: PtcResourceLimits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputLimits {
    pub root: PathBuf,
    pub result_disk_bytes: u64,
    pub session_disk_bytes: u64,
    pub total_disk_bytes: u64,
    pub retention_secs: u64,
    pub model: ModelOutputLimits,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            root: std::env::temp_dir().join("coda-output"),
            result_disk_bytes: 64 * MIB as u64,
            session_disk_bytes: 512 * MIB as u64,
            total_disk_bytes: 4 * GIB,
            retention_secs: 86400,
            model: ModelOutputLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOutputLimits {
    pub single_call_bytes: usize,
    pub batch_call_bytes: usize,
}

impl Default for ModelOutputLimits {
    fn default() -> Self {
        Self {
            single_call_bytes: 16 * KIB,
            batch_call_bytes: 64 * KIB,
        }
    }
}

impl ModelOutputLimits {
    /// The largest page any model may be configured for; capture memory is
    /// derived from it.
    pub const MAX_SINGLE_CALL_BYTES: usize = 256 * KIB;

    pub fn validate(&self, output_root: &Path) -> Result<(), String> {
        bounded(
            "single_call_bytes",
            self.single_call_bytes as u64,
            KIB as u64,
            Self::MAX_SINGLE_CALL_BYTES as u64,
        )?;
        bounded(
            "batch_call_bytes",
            self.batch_call_bytes as u64,
            self.single_call_bytes as u64,
            MIB as u64,
        )?;
        let minimum = Self::minimum_response_bytes(output_root)?;
        if self.single_call_bytes < minimum {
            return Err(format!(
                "single_call_bytes must be at least {minimum} to show complete output paths and metadata, got {}",
                self.single_call_bytes
            ));
        }
        Ok(())
    }

    /// Conservative body allowance shared by configuration checks and batch dispatch.
    /// Includes one fully described reference with the most channels an output
    /// may hold (the longest-named ones) plus 512 bytes for status,
    /// continuation information and the response envelope; excludes preview text.
    pub fn minimum_response_bytes(output_root: &Path) -> Result<usize, String> {
        let id = OutputId(uuid::Uuid::nil());
        let directory = output_root.join("objects").join(id.to_string());
        let timestamp = [jiff::Timestamp::MIN, jiff::Timestamp::MAX]
            .into_iter()
            .max_by_key(|value| value.to_string().len())
            .expect("timestamp bounds are nonempty");
        // The worst case is the channels with the longest file names, as many
        // as one output may hold.
        let mut channels = Channel::ALL;
        channels.sort_by_key(|channel| std::cmp::Reverse(channel.file_name().len()));
        let reference = OutputRef {
            id,
            channels: channels[..Channel::MAX_PER_OUTPUT]
                .iter()
                .map(|&channel| OutputChannelRef {
                    channel,
                    path: directory.join(channel.file_name()),
                    captured_bytes: u64::MAX,
                    saved_bytes: u64::MAX,
                })
                .collect(),
            complete: false,
            failure: Some(StorageFailure::FinalizeTimeout),
            sealed_at: timestamp,
            expires_at: timestamp,
        };
        super::describe_saved(&[reference], None)
            .len()
            .checked_add(512)
            .ok_or_else(|| "minimum output response size overflowed".into())
    }

    /// Stable dispatch-order shares, computed before any call can have side effects.
    pub fn allocate(&self, calls: usize, minimum: usize) -> Result<Vec<usize>, &'static str> {
        if calls == 0 {
            return Ok(Vec::new());
        }
        let share = (self.batch_call_bytes / calls).min(self.single_call_bytes);
        if share < minimum {
            return Err("tool batch cannot fit the minimum output metadata");
        }
        let remainder = if share < self.single_call_bytes {
            self.batch_call_bytes % calls
        } else {
            0
        };
        Ok((0..calls)
            .map(|index| share + usize::from(index < remainder))
            .collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtcResourceLimits {
    pub timeout_secs: u64,
    pub heap_bytes: usize,
    pub host_buffer_bytes: usize,
    pub max_calls: usize,
    pub max_concurrent_calls: usize,
    pub final_report_bytes: usize,
}

impl Default for PtcResourceLimits {
    fn default() -> Self {
        Self {
            timeout_secs: 120,
            heap_bytes: 64 * MIB,
            host_buffer_bytes: 64 * MIB,
            max_calls: 128,
            max_concurrent_calls: 16,
            final_report_bytes: MIB,
        }
    }
}

impl PtcResourceLimits {
    /// The smallest configurable host-buffer budget: enough to clear the
    /// capture reservation at [`ModelOutputLimits::MAX_SINGLE_CALL_BYTES`]
    /// *and* [`super::PTC_REPORT_SEALING_BYTES`], which is what keeps this
    /// table independent of the model's.
    pub const MIN_HOST_BUFFER_BYTES: usize = 8 * MIB;
}

// No legal pair of settings may leave a script unable to seal a report.
const _: () = assert!(
    PtcResourceLimits::MIN_HOST_BUFFER_BYTES
        >= super::ptc_capture_reserve_bytes(ModelOutputLimits::MAX_SINGLE_CALL_BYTES)
            + super::PTC_REPORT_SEALING_BYTES
            + MIB
);

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), String> {
        let output = &self.output;
        let ptc = &self.ptc;
        if !output.root.is_absolute() {
            return Err("resources.output.root must resolve to an absolute path".into());
        }
        bounded(
            "resources.output.result_disk_bytes",
            output.result_disk_bytes,
            MIB as u64,
            GIB,
        )?;
        bounded(
            "resources.output.session_disk_bytes",
            output.session_disk_bytes,
            output.result_disk_bytes,
            TIB,
        )?;
        bounded(
            "resources.output.total_disk_bytes",
            output.total_disk_bytes,
            output.session_disk_bytes,
            16 * TIB,
        )?;
        bounded(
            "resources.output.retention_secs",
            output.retention_secs,
            60,
            2592000,
        )?;
        output
            .model
            .validate(&output.root)
            .map_err(|e| format!("resources.output.model.{e}"))?;
        bounded("resources.ptc.timeout_secs", ptc.timeout_secs, 1, 600)?;
        bounded(
            "resources.ptc.heap_bytes",
            ptc.heap_bytes as u64,
            (8 * MIB) as u64,
            (512 * MIB) as u64,
        )?;
        bounded(
            "resources.ptc.host_buffer_bytes",
            ptc.host_buffer_bytes as u64,
            PtcResourceLimits::MIN_HOST_BUFFER_BYTES as u64,
            (512 * MIB) as u64,
        )?;
        bounded("resources.ptc.max_calls", ptc.max_calls as u64, 1, 1024)?;
        bounded(
            "resources.ptc.max_concurrent_calls",
            ptc.max_concurrent_calls as u64,
            1,
            ptc.max_calls.min(64) as u64,
        )?;
        bounded(
            "resources.ptc.final_report_bytes",
            ptc.final_report_bytes as u64,
            KIB as u64,
            output.result_disk_bytes.min((16 * MIB) as u64),
        )?;
        Ok(())
    }
}

fn bounded(field: &str, value: u64, min: u64, max: u64) -> Result<(), String> {
    if !(min..=max).contains(&value) {
        Err(format!(
            "{field} must be between {min} and {max}, got {value}"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
