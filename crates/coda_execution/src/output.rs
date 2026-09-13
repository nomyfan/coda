//! Task execution owns lifecycle; coda_output owns every payload byte.
use coda_core::output::{
    Channel, FINALIZE_TIMEOUT, IO_BLOCK_BYTES, OutputCapture, OutputData, OutputReader,
    OutputSnapshot,
};
use std::sync::Arc;
use tokio::sync::Mutex;

struct CaptureState {
    writer: Option<Box<dyn OutputCapture>>,
    pending: Option<OutputData>,
    deadline: Option<tokio::time::Instant>,
}

pub struct TaskOutputFiles {
    pub stdout: Arc<Stream>,
    pub stderr: Arc<Stream>,
    pub result: Arc<Stream>,
    state: Arc<Mutex<CaptureState>>,
    reader: Arc<dyn OutputReader>,
}

pub struct Stream {
    channel: Channel,
    state: Arc<Mutex<CaptureState>>,
    reader: Arc<dyn OutputReader>,
}

pub struct OutputChunk {
    pub bytes: Vec<u8>,
    pub lost: u64,
    pub next_cursor: u64,
    pub has_more: bool,
}

impl TaskOutputFiles {
    pub fn capturing(writer: Box<dyn OutputCapture>) -> Self {
        let reader = writer.reader();
        Self::new(Some(writer), reader)
    }

    pub fn retained(reader: Arc<dyn OutputReader>) -> Self {
        Self::new(None, reader)
    }

    fn new(writer: Option<Box<dyn OutputCapture>>, reader: Arc<dyn OutputReader>) -> Self {
        let state = Arc::new(Mutex::new(CaptureState {
            writer,
            pending: None,
            deadline: None,
        }));
        let stream = |channel| {
            Arc::new(Stream {
                channel,
                state: state.clone(),
                reader: reader.clone(),
            })
        };
        Self {
            stdout: stream(Channel::Stdout),
            stderr: stream(Channel::Stderr),
            result: stream(Channel::Result),
            state,
            reader,
        }
    }

    pub fn snapshot(&self) -> OutputSnapshot {
        self.reader.snapshot()
    }

    pub async fn flush(&self) -> std::io::Result<()> {
        self.stdout.flush().await
    }

    pub async fn begin_finalization(&self) {
        let mut state = self.state.lock().await;
        let deadline = *state
            .deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + FINALIZE_TIMEOUT);
        if let Some(writer) = &mut state.writer {
            writer.set_deadline(deadline);
        }
    }

    pub async fn mark_incomplete(&self) {
        if let Some(writer) = &mut self.state.lock().await.writer {
            writer.fail(coda_core::output::StorageFailure::Incomplete);
        }
    }

    pub async fn checkpointed(&self) {
        self.state.lock().await.pending = None;
    }
}

impl Stream {
    pub async fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut state = self.state.lock().await;
        let writer = state
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("task output is sealed"))?;
        for chunk in bytes.chunks(IO_BLOCK_BYTES) {
            writer.append(self.channel, chunk.to_vec()).await;
        }
        // Storage exhaustion is recorded by the capture. It must not stop
        // draining the command's pipe or change its execution outcome.
        Ok(())
    }

    pub async fn logical_range(&self) -> (u64, u64) {
        (
            0,
            self.reader
                .snapshot()
                .channels
                .iter()
                .find(|channel| channel.channel == self.channel)
                .map_or(0, |channel| channel.captured),
        )
    }

    pub async fn read_from(&self, offset: u64, limit: usize) -> std::io::Result<OutputChunk> {
        let snapshot = self.reader.snapshot();
        let channel = snapshot
            .channels
            .iter()
            .find(|channel| channel.channel == self.channel);
        let captured = channel.map_or(0, |channel| channel.captured);
        let saved = channel.map_or(0, |channel| channel.saved);
        let mut bytes = self
            .reader
            .read(self.channel, offset, limit)
            .await
            .map_err(std::io::Error::other)?;
        if offset + (bytes.len() as u64) < saved {
            bytes.truncate(coda_output::preview::page_boundary(&bytes));
        }
        let next_cursor = offset + bytes.len() as u64;
        Ok(OutputChunk {
            bytes,
            lost: if snapshot.failure.is_some() {
                captured.saturating_sub(saved)
            } else {
                0
            },
            next_cursor,
            has_more: next_cursor < saved,
        })
    }

    pub async fn tail(&self, limit: usize) -> std::io::Result<Vec<u8>> {
        let snapshot = self.reader.snapshot();
        let saved = snapshot
            .channels
            .iter()
            .find(|c| c.channel == self.channel)
            .map_or(0, |c| c.saved);
        Ok(self
            .read_from(saved.saturating_sub(limit as u64), limit)
            .await?
            .bytes)
    }

    pub async fn flush(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().await;
        if let Some(writer) = state.writer.take() {
            let deadline = state
                .deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + FINALIZE_TIMEOUT);
            state.pending = Some(writer.finish(deadline).await);
        }
        Ok(())
    }
}
