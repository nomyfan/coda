//! A synchronous log producer with its own independently drained queue.
use crate::preview::Preview;
use coda_core::output::{Channel, FINALIZE_TIMEOUT, IO_BLOCK_BYTES, OutputCapture, OutputData};
use coda_core::tool::CancellationToken;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Queue {
    chunks: VecDeque<Vec<u8>>,
    closed: bool,
    preview: Preview,
}

#[derive(Clone)]
pub struct LogWriter {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    cancel: CancellationToken,
    deadline: Instant,
}

pub struct LogCollector {
    writer: LogWriter,
    completion: Option<tokio::sync::oneshot::Receiver<Box<dyn OutputCapture>>>,
}

impl LogCollector {
    pub fn start(
        mut capture: Box<dyn OutputCapture>,
        preview_bytes: usize,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> std::io::Result<Self> {
        let writer = LogWriter {
            queue: Arc::new((
                Mutex::new(Queue {
                    chunks: VecDeque::new(),
                    closed: false,
                    preview: Preview::new(preview_bytes),
                }),
                Condvar::new(),
            )),
            cancel,
            deadline,
        };
        let queue = writer.queue.clone();
        let (complete, completion) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("coda-output-log".into())
            .spawn(move || {
                loop {
                    let chunk = {
                        let (lock, wake) = &*queue;
                        let mut queue = lock.lock().unwrap();
                        while queue.chunks.is_empty() && !queue.closed {
                            queue = wake.wait(queue).unwrap();
                        }
                        let chunk = queue.chunks.pop_front();
                        wake.notify_all();
                        chunk
                    };
                    let Some(chunk) = chunk else { break };
                    runtime.block_on(capture.append(Channel::Log, chunk));
                }
                let _ = complete.send(capture);
            })?;
        Ok(Self {
            writer,
            completion: Some(completion),
        })
    }

    pub fn writer(&self) -> LogWriter {
        self.writer.clone()
    }

    pub fn snapshot(&self) -> (String, bool) {
        let queue = self.writer.queue.0.lock().unwrap();
        (queue.preview.text(), !queue.preview.complete())
    }

    pub async fn finish(mut self, report: &str) -> OutputData {
        {
            let mut queue = self.writer.queue.0.lock().unwrap();
            queue.closed = true;
            self.writer.queue.1.notify_all();
        }
        let deadline = tokio::time::Instant::now() + FINALIZE_TIMEOUT;
        match tokio::time::timeout_at(deadline, self.completion.take().unwrap()).await {
            Ok(Ok(mut capture)) => {
                capture.set_deadline(deadline);
                for chunk in report.as_bytes().chunks(IO_BLOCK_BYTES) {
                    if tokio::time::Instant::now() >= deadline {
                        capture.fail(coda_core::output::StorageFailure::FinalizeTimeout);
                    }
                    capture.append(Channel::ResultJson, chunk.to_vec()).await;
                }
                capture.finish(deadline).await
            }
            _ => OutputData::unavailable(
                {
                    let (log, _) = self.snapshot();
                    match (report.is_empty(), log.is_empty()) {
                        (_, true) => report.to_owned(),
                        (true, false) => format!("log:\n{log}"),
                        (false, false) => format!("{report}\nlog:\n{log}"),
                    }
                },
                coda_core::output::StorageFailure::FinalizeTimeout,
            ),
        }
    }
}

impl LogWriter {
    /// JavaScript splits on scalar boundaries before crossing the native bridge.
    pub fn append(&self, chunk: String) {
        assert!(chunk.len() <= IO_BLOCK_BYTES);
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        while queue.chunks.len() == 1 && !queue.closed {
            if self.cancel.is_cancelled() || Instant::now() >= self.deadline {
                return;
            }
            queue = wake
                .wait_timeout(queue, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        if queue.closed {
            return;
        }
        queue.preview.append(chunk.as_bytes());
        queue.chunks.push_back(chunk.into_bytes());
        wake.notify_all();
    }
}

impl Drop for LogCollector {
    fn drop(&mut self) {
        self.writer.queue.0.lock().unwrap().closed = true;
        self.writer.queue.1.notify_all();
    }
}
