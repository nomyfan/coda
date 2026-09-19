use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use coda_core::output::*;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::archive_dir::{ArchiveDir, ArchiveFileName as FileName, ArchiveRootLock, EntryKind};
use crate::preview::Preview;

const OBJECT_OVERHEAD: u64 = 32 * 1024;
const MAX_MANIFEST: u64 = 8192;
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_OBJECTS: usize = 65536;
const MAX_SESSION_OBJECTS: usize = 4096;
/// Every file an output object may hold.
fn object_files() -> impl Iterator<Item = FileName> {
    [FileName::OutputOwner, FileName::Meta, FileName::MetaTmp]
        .into_iter()
        .chain(Channel::ALL.map(FileName::Channel))
}

pub struct Store {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    limits: OutputLimits,
    _lock: ArchiveRootLock,
    objects: ArchiveDir,
    ledger: Mutex<HashMap<OutputId, Entry>>,
    history_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    hook: Mutex<Option<TestHook>>,
}

struct Entry {
    owner: OutputOwner,
    charged: u64,
    sealed_at: Option<jiff::Timestamp>,
    expires_at: Option<jiff::Timestamp>,
    pin: Weak<()>,
    deleting: bool,
    reference: Option<OutputRef>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    owner: OutputOwner,
    reference: OutputRef,
}

impl Store {
    pub fn standalone() -> Arc<Self> {
        static STORE: std::sync::OnceLock<Arc<Store>> = std::sync::OnceLock::new();
        STORE
            .get_or_init(|| {
                Arc::new(
                    Store::open(OutputLimits {
                        root: std::env::temp_dir().join(format!(
                            "coda-output-{}-{}",
                            std::process::id(),
                            OutputId::new()
                        )),
                        ..OutputLimits::default()
                    })
                    .expect("standalone output store"),
                )
            })
            .clone()
    }

    /// The session's output store and the owner its output is charged to, or
    /// the standalone store outside a session.
    pub fn session_or_standalone(
        outputs: Option<&OutputRuntime>,
    ) -> (Arc<dyn OutputStore>, OutputOwner) {
        match outputs {
            Some(outputs) => (outputs.store.clone(), outputs.owner.clone()),
            None => (Self::standalone(), OutputOwner::default()),
        }
    }

    /// Locks and scans the whole root before accepting any writes.
    pub fn open(limits: OutputLimits) -> Result<Self, String> {
        let lock = ArchiveRootLock::acquire(&limits.root).map_err(|e| e.to_string())?;
        let root = lock.directory();
        let objects = match root.open_dir("objects") {
            Ok(dir) => dir,
            Err(crate::archive_dir::ArchiveError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                root.create_dir("objects").map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        let inner = Arc::new(StoreInner {
            limits,
            _lock: lock,
            objects,
            ledger: Mutex::default(),
            history_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            hook: Mutex::new(None),
        });
        inner.recover()?;
        inner.cleanup(None, false);
        Ok(Self { inner })
    }

    /// The task owns only a weak reference; shutting down the service releases the lock.
    pub fn start_cleanup(&self) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(inner) = weak.upgrade() else { break };
                let _ = tokio::task::spawn_blocking(move || inner.cleanup(None, false)).await;
            }
        });
    }

    pub fn charged_bytes(&self) -> u64 {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.charged)
            .sum()
    }

    pub fn reader(&self, snapshot: OutputSnapshot) -> Arc<dyn OutputReader> {
        Arc::new(Reader {
            snapshot: Arc::new(Mutex::new(snapshot)),
            store: self.inner.clone(),
        })
    }

    /// Start a capture. A programmatic capture's `memory` lease is held by the
    /// writer until it finishes.
    fn start_capture(
        &self,
        owner: OutputOwner,
        channels: Vec<Channel>,
        purpose: &CapturePurpose,
        memory: Option<BufferLease>,
        id: Option<OutputId>,
    ) -> Capture {
        // Every producer passes a fixed channel list, so a bad one is a bug.
        assert!(
            !channels.is_empty()
                && channels.len() <= Channel::MAX_PER_OUTPUT
                && !channels
                    .iter()
                    .enumerate()
                    .any(|(i, c)| channels[..i].contains(c)),
            "invalid output channels: {channels:?}"
        );
        let limit = self.inner.limits.capture_memory_bytes;
        // Background output is read while it is still being written, so
        // every byte goes straight to disk and even an empty capture
        // leaves files to read. A script's capture holds budget only
        // until the writer finishes, so it also ends on disk, as a
        // temporary file nothing references.
        let inline_limit = (limit - 2 * IO_BLOCK_BYTES) / 8;
        let (inline_limit, force_disk, temporary) = match purpose {
            CapturePurpose::Foreground => (inline_limit, false, false),
            CapturePurpose::Background => (0, true, false),
            CapturePurpose::Programmatic(_) => (inline_limit, true, true),
        };
        let previews = channels
            .iter()
            .map(|c| {
                (
                    *c,
                    Preview::new((limit - 2 * IO_BLOCK_BYTES) / 32 / channels.len()),
                )
            })
            .collect();
        let abandoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, mut receiver) = mpsc::channel(1);
        let id = id.unwrap_or_default();
        let snapshot = Arc::new(Mutex::new(OutputSnapshot {
            preview: String::new(),
            sealed: false,
            id,
            channels: channels
                .iter()
                .map(|channel| ChannelBytes {
                    channel: *channel,
                    captured: 0,
                    saved: 0,
                })
                .collect(),
            reference: None,
            failure: None,
        }));
        let reader = Arc::new(Reader {
            snapshot: snapshot.clone(),
            store: self.inner.clone(),
        });
        let mut writer = Writer {
            store: self.inner.clone(),
            owner,
            id,
            snapshot,
            pin: Arc::new(()),
            temporary,
            inline: channels.into_iter().map(|c| (c, Vec::new())).collect(),
            inline_limit,
            directory: None,
            files: Vec::new(),
            failure: None,
            abandoned: abandoned.clone(),
        };
        tokio::task::spawn_blocking(move || {
            let _memory = memory;
            while let Some(request) = receiver.blocking_recv() {
                match request {
                    Request::Append(channel, bytes, reply) => {
                        writer.append(channel, &bytes);
                        let _ = reply.send(writer.failure.clone());
                    }
                    Request::Finish(force_disk, captured, failure, reply) => {
                        let result = if force_disk && writer.directory.is_none() {
                            writer
                                .spill()
                                .and_then(|()| writer.finish(captured, failure))
                        } else {
                            writer.finish(captured, failure)
                        };
                        if !writer.abandoned.load(std::sync::atomic::Ordering::Acquire) {
                            if let Ok(finished) = &result
                                && let Some(reference) = &finished.reference
                            {
                                let mut ledger = writer.store.ledger.lock().unwrap();
                                if let Some(entry) = ledger.get_mut(&writer.id) {
                                    entry.reference = Some(reference.clone());
                                    entry.sealed_at = Some(reference.sealed_at);
                                    entry.expires_at = Some(reference.expires_at);
                                }
                            }
                            let _ = reply.send(result.map(|finished| {
                                (finished, writer.pin.clone(), writer.store.clone())
                            }));
                        }
                        break;
                    }
                }
            }
            // No new IO is submitted after a timed-out operation. The writer
            // retains its pin and quota until the blocked syscall returns.
            if writer.abandoned.load(std::sync::atomic::Ordering::Acquire)
                && let Some(entry) = writer.store.ledger.lock().unwrap().get_mut(&writer.id)
            {
                entry.reference = None;
                entry.sealed_at = None;
                entry.expires_at = None;
            }
            let store = writer.store.clone();
            drop(writer);
            store.cleanup(None, false);
        });
        Capture {
            sender,
            previews,
            failure: None,
            abandoned,
            completed: false,
            reader,
            force_disk,
            deadline: None,
        }
    }

    /// Save `text` as a sealed result file, giving up on storage at `deadline`.
    async fn save_result(
        &self,
        owner: OutputOwner,
        id: Option<OutputId>,
        text: &str,
        deadline: tokio::time::Instant,
    ) -> OutputData {
        let mut capture = self.start_capture(
            owner,
            vec![Channel::Result],
            &CapturePurpose::Foreground,
            None,
            id,
        );
        capture.force_disk = true;
        capture.set_deadline(deadline);
        capture.append(Channel::Result, text.as_bytes()).await;
        Box::new(capture).finish(deadline).await
    }
}

impl OutputStore for Store {
    fn limits(&self) -> &OutputLimits {
        &self.inner.limits
    }

    fn begin(
        &self,
        owner: OutputOwner,
        channels: Vec<Channel>,
        purpose: CapturePurpose,
    ) -> OutputFuture<'_, Result<Box<dyn OutputCapture>, OutputError>> {
        Box::pin(async move {
            let memory = match &purpose {
                CapturePurpose::Programmatic(budget) => Some(
                    budget
                        .reserve(
                            self.inner.limits.capture_memory_bytes,
                            &coda_core::tool::CancellationToken::new(),
                        )
                        .await?,
                ),
                CapturePurpose::Foreground | CapturePurpose::Background => None,
            };
            Ok(
                Box::new(self.start_capture(owner, channels, &purpose, memory, None))
                    as Box<dyn OutputCapture>,
            )
        })
    }

    fn retain_source(
        &self,
        owner: OutputOwner,
        source: coda_core::llm::MessageId,
        text: String,
        deadline: tokio::time::Instant,
    ) -> OutputFuture<'_, OutputData> {
        Box::pin(async move {
            let Ok(_gate) = tokio::time::timeout_at(deadline, self.inner.history_gate.lock()).await
            else {
                return OutputData::unavailable(text, StorageFailure::FinalizeTimeout);
            };
            let id = OutputId::for_source(&owner, source);
            let retained = {
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger
                    .get_mut(&id)
                    .filter(|entry| !entry.deleting)
                    .and_then(|entry| {
                        let reference = entry.reference.clone()?;
                        let pin = entry.pin.upgrade().unwrap_or_else(|| Arc::new(()));
                        entry.pin = Arc::downgrade(&pin);
                        Some((reference, pin))
                    })
            };
            if let Some((reference, pin)) = retained {
                let store = self.inner.clone();
                let reference_for_read = reference.clone();
                let opened = tokio::task::spawn_blocking(move || {
                    let dir = store
                        .objects
                        .open_dir(id.to_string())
                        .map_err(|e| e.to_string())?;
                    let files = reference_for_read
                        .channels
                        .iter()
                        .map(|channel| {
                            dir.open_file(FileName::Channel(channel.channel), false)
                                .map(|file| (channel.channel, file, channel.saved_bytes))
                                .map_err(|e| e.to_string())
                        })
                        .collect::<Result<_, _>>()?;
                    Ok::<_, String>(Arc::new(Buffer {
                        source: Arc::new(Mutex::new(Source::Files(files))),
                        failure: reference_for_read.failure,
                        _pin: pin,
                        store,
                    }))
                });
                return match tokio::time::timeout_at(deadline, opened).await {
                    Ok(Ok(Ok(buffer))) => OutputData::Captured(CapturedOutput {
                        report_ok: None,
                        preview: text,
                        reference: Some(reference.clone()),
                        failure: reference.failure,
                        buffer,
                    }),
                    _ => OutputData::unavailable(text, StorageFailure::Io),
                };
            }
            if self.inner.ledger.lock().unwrap().contains_key(&id) {
                return OutputData::unavailable(text, StorageFailure::Io);
            }
            self.save_result(owner, Some(id), &text, deadline).await
        })
    }

    fn retain(
        &self,
        owner: OutputOwner,
        text: String,
        deadline: tokio::time::Instant,
    ) -> OutputFuture<'_, OutputData> {
        Box::pin(async move { self.save_result(owner, None, &text, deadline).await })
    }
}

impl StoreInner {
    fn recover(&self) -> Result<(), String> {
        for entry in self.objects.entries().map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !matches!(entry.kind, EntryKind::Dir | EntryKind::Unknown) {
                return Err(format!("unexpected output object {}", entry.name));
            }
            let id: OutputId =
                serde_json::from_value(serde_json::Value::String(entry.name.clone()))
                    .map_err(|_| "invalid output object ID")?;
            let dir = self
                .objects
                .open_dir(&entry.name)
                .map_err(|e| e.to_string())?;
            let mut charged = OBJECT_OVERHEAD;
            for name in object_files() {
                match dir.open_file(name, false) {
                    Ok(file) => {
                        if matches!(name, FileName::Channel(_)) {
                            charged = charged
                                .checked_add(file.metadata().map_err(|e| e.to_string())?.len())
                                .ok_or("output size overflow")?;
                        }
                    }
                    Err(crate::archive_dir::ArchiveError::Io(e))
                        if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            let manifest = dir.open_file(FileName::Meta, false).ok().and_then(|file| {
                let mut bytes = Vec::new();
                file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes).ok()?;
                (bytes.len() <= MAX_MANIFEST as usize)
                    .then(|| serde_json::from_slice::<Manifest>(&bytes).ok())
                    .flatten()
            });
            let manifest = manifest.filter(|m| {
                m.version == 1
                    && m.reference.id == id
                    && m.reference.channels.len() <= Channel::MAX_PER_OUTPUT
                    && m.reference.channels.iter().all(|channel| {
                        channel.path
                            == self
                                .limits
                                .root
                                .join("objects")
                                .join(id.to_string())
                                .join(channel.channel.file_name())
                            && channel.saved_bytes <= channel.captured_bytes
                            && dir
                                .open_file(FileName::Channel(channel.channel), false)
                                .and_then(|f| Ok(f.metadata()?.len()))
                                .is_ok_and(|len| len == channel.saved_bytes)
                    })
            });
            let recovered_owner =
                dir.open_file(FileName::OutputOwner, false)
                    .ok()
                    .and_then(|file| {
                        let mut bytes = Vec::new();
                        file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes).ok()?;
                        if bytes.len() > MAX_MANIFEST as usize {
                            return None;
                        }
                        serde_json::from_slice::<OutputOwner>(&bytes).ok()
                    });
            let reference = manifest.as_ref().map(|m| m.reference.clone());
            let (owner, sealed_at, expires_at) = match manifest {
                Some(m) => (
                    m.owner,
                    Some(m.reference.sealed_at),
                    Some(m.reference.expires_at),
                ),
                None => (
                    recovered_owner.unwrap_or(OutputOwner {
                        workspace_id: "<orphan>".into(),
                        session_id: "<orphan>".into(),
                    }),
                    None,
                    None,
                ),
            };
            self.ledger.lock().unwrap().insert(
                id,
                Entry {
                    owner,
                    charged,
                    sealed_at,
                    expires_at,
                    pin: Weak::new(),
                    deleting: false,
                    reference,
                },
            );
        }
        Ok(())
    }

    fn reserve(
        &self,
        id: OutputId,
        owner: &OutputOwner,
        bytes: u64,
        pin: &Arc<()>,
    ) -> Result<(), StorageFailure> {
        for attempt in 0..65 {
            let failure = {
                let mut ledger = self.ledger.lock().unwrap();
                let session_bytes: u64 = ledger
                    .values()
                    .filter(|e| &e.owner == owner)
                    .map(|e| e.charged)
                    .sum();
                let total_bytes: u64 = ledger.values().map(|e| e.charged).sum();
                let new = !ledger.contains_key(&id);
                let failure = if new
                    && (ledger.len() >= MAX_OBJECTS
                        || ledger.values().filter(|e| &e.owner == owner).count()
                            >= MAX_SESSION_OBJECTS)
                {
                    Some(StorageFailure::ObjectLimit)
                } else if session_bytes.saturating_add(bytes) > self.limits.session_disk_bytes {
                    Some(StorageFailure::SessionQuota)
                } else if total_bytes.saturating_add(bytes) > self.limits.total_disk_bytes {
                    Some(StorageFailure::ServiceQuota)
                } else {
                    None
                };
                if failure.is_none() {
                    ledger
                        .entry(id)
                        .or_insert_with(|| Entry {
                            owner: owner.clone(),
                            charged: 0,
                            sealed_at: None,
                            expires_at: None,
                            pin: Arc::downgrade(pin),
                            deleting: false,
                            reference: None,
                        })
                        .charged += bytes;
                    return Ok(());
                }
                failure.unwrap()
            };
            if attempt == 64 {
                return Err(failure);
            }
            let owner = (failure == StorageFailure::SessionQuota).then_some(owner);
            self.cleanup(owner, true);
        }
        unreachable!()
    }

    fn cleanup(&self, owner: Option<&OutputOwner>, pressure: bool) {
        // Mark under the ledger lock, then perform filesystem IO without holding it.
        let candidates = {
            let mut ledger = self.ledger.lock().unwrap();
            let now = jiff::Timestamp::now();
            let mut candidates: Vec<_> = ledger
                .iter()
                .filter(|(_, e)| {
                    !e.deleting
                        && e.pin.strong_count() == 0
                        && owner.is_none_or(|owner| &e.owner == owner)
                        && (pressure || e.expires_at.is_none_or(|at| at <= now))
                })
                .map(|(id, e)| (*id, e.sealed_at))
                .collect();
            candidates.sort_by_key(|(_, at)| *at);
            candidates.truncate(if pressure { 1 } else { 64 });
            for (id, _) in &candidates {
                ledger.get_mut(id).unwrap().deleting = true;
            }
            candidates
        };
        for (id, _) in candidates {
            let removed = (|| {
                #[cfg(test)]
                self.test_step("delete").map_err(|_| {
                    crate::archive_dir::ArchiveError::corrupt("injected delete failure")
                })?;
                let dir = match self.objects.open_dir(id.to_string()) {
                    Ok(dir) => dir,
                    Err(crate::archive_dir::ArchiveError::Io(e))
                        if e.kind() == std::io::ErrorKind::NotFound =>
                    {
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                };
                for name in object_files() {
                    dir.unlink(name)?;
                }
                self.objects.remove_dir(id.to_string())?;
                Ok::<_, crate::archive_dir::ArchiveError>(())
            })();
            let mut ledger = self.ledger.lock().unwrap();
            if removed.is_ok() {
                ledger.remove(&id);
            } else if let Some(entry) = ledger.get_mut(&id) {
                entry.deleting = false;
            }
        }
    }
}

struct Capture {
    sender: mpsc::Sender<Request>,
    previews: Vec<(Channel, Preview)>,
    failure: Option<StorageFailure>,
    abandoned: Arc<std::sync::atomic::AtomicBool>,
    completed: bool,
    reader: Arc<Reader>,
    force_disk: bool,
    deadline: Option<tokio::time::Instant>,
}

impl Capture {
    /// Count and preview one block, then hand it to the writer unless storage
    /// has already failed or the deadline has passed.
    async fn append_block(&mut self, channel: Channel, block: &[u8]) {
        self.reader
            .snapshot
            .lock()
            .unwrap()
            .channels
            .iter_mut()
            .find(|c| c.channel == channel)
            .unwrap()
            .captured += block.len() as u64;
        self.previews
            .iter_mut()
            .find(|(c, _)| *c == channel)
            .expect("declared channel")
            .1
            .append(block);
        if self.failure.is_none()
            && self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            self.fail(StorageFailure::FinalizeTimeout);
        }
        if self.failure.is_some() {
            return;
        }
        let (reply, receive) = oneshot::channel();
        let operation = async {
            self.sender
                .send(Request::Append(channel, block.to_vec(), reply))
                .await
                .map_err(|_| StorageFailure::Io)?;
            receive.await.map_err(|_| StorageFailure::Io)
        };
        let deadline = self
            .deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + IO_TIMEOUT)
            .min(tokio::time::Instant::now() + IO_TIMEOUT);
        match tokio::time::timeout_at(deadline, operation).await {
            Ok(Ok(failure)) => self.failure = failure,
            Ok(Err(failure)) => self.failure = Some(failure),
            Err(_) => {
                self.failure = Some(StorageFailure::FinalizeTimeout);
                self.abandoned
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        self.reader.snapshot.lock().unwrap().failure = self.failure.clone();
    }
}

type CaptureResult = Result<(Finished, Arc<()>, Arc<StoreInner>), StorageFailure>;

enum Request {
    Append(Channel, Vec<u8>, oneshot::Sender<Option<StorageFailure>>),
    Finish(
        bool,
        Vec<(Channel, u64)>,
        Option<StorageFailure>,
        oneshot::Sender<CaptureResult>,
    ),
}

impl OutputCapture for Capture {
    fn set_deadline(&mut self, deadline: tokio::time::Instant) {
        self.deadline = Some(
            self.deadline
                .map_or(deadline, |current| current.min(deadline)),
        );
    }
    fn reader(&self) -> Arc<dyn OutputReader> {
        self.reader.clone()
    }
    fn fail(&mut self, failure: StorageFailure) {
        self.failure = Some(failure.clone());
        let mut snapshot = self.reader.snapshot.lock().unwrap();
        snapshot.failure = Some(failure);
        snapshot.reference = None;
    }
    fn append<'a>(&'a mut self, channel: Channel, bytes: &'a [u8]) -> OutputFuture<'a, ()> {
        Box::pin(async move {
            for block in bytes.chunks(IO_BLOCK_BYTES) {
                self.append_block(channel, block).await;
            }
        })
    }

    fn finish(
        mut self: Box<Self>,
        deadline: tokio::time::Instant,
    ) -> OutputFuture<'static, OutputData> {
        Box::pin(async move {
            let deadline = self
                .deadline
                .map_or(deadline, |current| current.min(deadline));
            let mut preview = String::new();
            let captured = self
                .previews
                .iter()
                .map(|(channel, p)| (*channel, p.captured_bytes()))
                .collect();
            for (channel, p) in &self.previews {
                append_text(&mut preview, *channel, &p.text());
            }
            {
                let mut bounded = Preview::new(1024);
                bounded.append(preview.as_bytes());
                self.reader.snapshot.lock().unwrap().preview = bounded.text();
            }
            let (reply, receive) = oneshot::channel();
            let operation = async {
                self.sender
                    .send(Request::Finish(
                        self.force_disk,
                        captured,
                        self.failure.clone(),
                        reply,
                    ))
                    .await
                    .map_err(|_| StorageFailure::Io)?;
                receive.await.map_err(|_| StorageFailure::Io)?
            };
            let finished = match tokio::time::timeout_at(deadline, operation).await {
                Ok(result) => result,
                Err(_) => Err(StorageFailure::FinalizeTimeout),
            };
            match finished {
                Ok((finished, pin, store)) => {
                    self.completed = true;
                    {
                        let mut snapshot = self.reader.snapshot.lock().unwrap();
                        snapshot.sealed = true;
                        snapshot.reference = finished.reference.clone();
                        snapshot.failure = finished.failure.clone();
                    }
                    if let Source::Memory(channels) = &finished.source
                        && finished.failure.is_none()
                    {
                        let mut text = String::new();
                        for (channel, bytes) in channels {
                            append_text(&mut text, *channel, &String::from_utf8_lossy(bytes));
                        }
                        return OutputData::Inline(text);
                    }
                    let buffer = Arc::new(Buffer {
                        source: Arc::new(Mutex::new(finished.source)),
                        failure: finished.failure.clone(),
                        _pin: pin,
                        store,
                    });
                    OutputData::Captured(CapturedOutput {
                        report_ok: None,
                        preview,
                        reference: finished.reference,
                        failure: finished.failure,
                        buffer,
                    })
                }
                Err(failure) => {
                    self.reader.snapshot.lock().unwrap().sealed = true;
                    self.fail(failure.clone());
                    self.abandoned
                        .store(true, std::sync::atomic::Ordering::Release);
                    OutputData::unavailable(preview, failure)
                }
            }
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // The worker's own pin guards queued/in-flight IO after cancellation.
        if !self.completed {
            self.abandoned
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

struct Writer {
    store: Arc<StoreInner>,
    owner: OutputOwner,
    id: OutputId,
    pin: Arc<()>,
    temporary: bool,
    snapshot: Arc<Mutex<OutputSnapshot>>,
    inline: Vec<(Channel, Vec<u8>)>,
    inline_limit: usize,
    directory: Option<ArchiveDir>,
    files: Vec<(Channel, std::fs::File, u64)>,
    failure: Option<StorageFailure>,
    abandoned: Arc<std::sync::atomic::AtomicBool>,
}

impl Writer {
    fn spill(&mut self) -> Result<(), StorageFailure> {
        self.store
            .reserve(self.id, &self.owner, OBJECT_OVERHEAD, &self.pin)?;
        let dir = self
            .store
            .objects
            .create_dir(self.id.to_string())
            .map_err(|_| StorageFailure::Io)?;
        self.directory = Some(dir);
        let owner = serde_json::to_vec(&self.owner).map_err(|_| StorageFailure::Io)?;
        if owner.len() > MAX_MANIFEST as usize {
            return Err(StorageFailure::Io);
        }
        let dir = self.directory.as_ref().unwrap();
        let mut owner_file = dir
            .create_file(FileName::OutputOwner)
            .map_err(|_| StorageFailure::Io)?;
        owner_file
            .write_all(&owner)
            .map_err(|_| StorageFailure::Io)?;
        owner_file.sync_all().map_err(|_| StorageFailure::Io)?;
        dir.sync().map_err(|_| StorageFailure::Io)?;
        self.store.objects.sync().map_err(|_| StorageFailure::Io)?;
        let inline = std::mem::take(&mut self.inline);
        for (channel, bytes) in inline {
            let file = self
                .directory
                .as_ref()
                .unwrap()
                .create_file(FileName::Channel(channel))
                .map_err(|_| StorageFailure::Io)?;
            self.files.push((channel, file, 0));
            for chunk in bytes.chunks(IO_BLOCK_BYTES) {
                self.write(channel, chunk)?;
            }
        }
        Ok(())
    }

    fn write(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), StorageFailure> {
        let total: u64 = self.files.iter().map(|(_, _, n)| *n).sum();
        let allowed = (self.store.limits.result_max_bytes.saturating_sub(total))
            .min(bytes.len() as u64) as usize;
        if allowed > 0 {
            self.store
                .reserve(self.id, &self.owner, allowed as u64, &self.pin)?;
            let (_, file, saved) = self
                .files
                .iter_mut()
                .find(|(c, _, _)| *c == channel)
                .ok_or(StorageFailure::Io)?;
            let result = file.write_all(&bytes[..allowed]);
            *saved = file.metadata().map_err(|_| StorageFailure::Io)?.len();
            self.snapshot
                .lock()
                .unwrap()
                .channels
                .iter_mut()
                .find(|c| c.channel == channel)
                .unwrap()
                .saved = *saved;
            result.map_err(|_| StorageFailure::Io)?;
        }
        if allowed < bytes.len() {
            Err(StorageFailure::ResultLimit)
        } else {
            Ok(())
        }
    }

    fn append(&mut self, channel: Channel, bytes: &[u8]) {
        if self.failure.is_some() || self.abandoned.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if self.directory.is_none() {
            let size: usize = self.inline.iter().map(|(_, bytes)| bytes.len()).sum();
            if size + bytes.len() <= self.inline_limit {
                self.inline
                    .iter_mut()
                    .find(|(c, _)| *c == channel)
                    .unwrap()
                    .1
                    .extend_from_slice(bytes);
                return;
            }
            if let Err(failure) = self.spill() {
                self.failure = Some(failure);
                return;
            }
        }
        if let Err(failure) = self.write(channel, bytes) {
            self.failure = Some(failure);
        }
    }

    fn finish(
        &mut self,
        captured: Vec<(Channel, u64)>,
        failure: Option<StorageFailure>,
    ) -> Result<Finished, StorageFailure> {
        if self.failure.is_none() {
            self.failure = failure;
        }
        if self.directory.is_none() {
            return Ok(Finished {
                reference: None,
                source: Source::Memory(std::mem::take(&mut self.inline)),
                failure: self.failure.clone(),
            });
        }
        let reference = if self.temporary {
            None
        } else {
            let now = jiff::Timestamp::now();
            let reference = OutputRef {
                id: self.id,
                channels: self
                    .files
                    .iter()
                    .map(|(channel, _, saved)| OutputChannelRef {
                        channel: *channel,
                        path: self
                            .store
                            .limits
                            .root
                            .join("objects")
                            .join(self.id.to_string())
                            .join(channel.file_name()),
                        captured_bytes: captured
                            .iter()
                            .find(|(c, _)| c == channel)
                            .map_or(0, |(_, n)| *n),
                        saved_bytes: *saved,
                    })
                    .collect(),
                complete: self.failure.is_none(),
                failure: self.failure.clone(),
                sealed_at: now,
                expires_at: now
                    .checked_add(Duration::from_secs(self.store.limits.retention_secs))
                    .map_err(|_| StorageFailure::Io)?,
            };
            #[cfg(test)]
            self.store.test_step("file_sync")?;
            for (_, file, _) in &self.files {
                file.sync_all().map_err(|_| StorageFailure::Io)?;
            }
            let dir = self.directory.as_ref().unwrap();
            let bytes = serde_json::to_vec(&Manifest {
                version: 1,
                owner: self.owner.clone(),
                reference: reference.clone(),
            })
            .map_err(|_| StorageFailure::Io)?;
            if bytes.len() > MAX_MANIFEST as usize {
                return Err(StorageFailure::Io);
            }
            let mut file = dir
                .create_file(FileName::MetaTmp)
                .map_err(|_| StorageFailure::Io)?;
            file.write_all(&bytes).map_err(|_| StorageFailure::Io)?;
            file.sync_all().map_err(|_| StorageFailure::Io)?;
            #[cfg(test)]
            self.store.test_step("manifest_replace")?;
            dir.rename(FileName::MetaTmp, FileName::Meta)
                .map_err(|_| StorageFailure::Io)?;
            #[cfg(test)]
            self.store.test_step("directory_sync")?;
            dir.sync().map_err(|_| StorageFailure::Io)?;
            self.store.objects.sync().map_err(|_| StorageFailure::Io)?;
            Some(reference)
        };
        Ok(Finished {
            reference,
            source: Source::Files(std::mem::take(&mut self.files)),
            failure: self.failure.clone(),
        })
    }
}

struct Finished {
    reference: Option<OutputRef>,
    source: Source,
    failure: Option<StorageFailure>,
}

#[derive(Debug)]
enum Source {
    Memory(Vec<(Channel, Vec<u8>)>),
    Files(Vec<(Channel, std::fs::File, u64)>),
}

struct Buffer {
    source: Arc<Mutex<Source>>,
    failure: Option<StorageFailure>,
    _pin: Arc<()>,
    store: Arc<StoreInner>,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let store = self.store.clone();
        self._pin = Arc::new(());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || store.cleanup(None, false));
        }
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputBuffer")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl OutputBuffer for Buffer {
    fn materialize<'a>(
        &'a self,
        budget: &'a BufferBudget,
        cancel: &'a coda_core::tool::CancellationToken,
    ) -> OutputFuture<'a, Result<HostResultBuffer, OutputError>> {
        Box::pin(async move {
            if self.failure.is_some() {
                return Err(OutputError::Incomplete(
                    "complete output is unavailable".into(),
                ));
            }
            let length = {
                let source = self.source.lock().unwrap();
                match &*source {
                    Source::Memory(channels) => channels
                        .iter()
                        .map(|(_, bytes)| bytes.len() as u64 + 32)
                        .sum::<u64>(),
                    Source::Files(channels) => {
                        channels.iter().map(|(_, _, bytes)| bytes + 32).sum::<u64>()
                    }
                }
            };
            // Reserve for raw reads, lossy UTF-8 expansion, String capacity growth
            // and native conversion buffers that can coexist during delivery.
            let required = usize::try_from(length)
                .ok()
                .and_then(|n| n.checked_mul(12))
                .ok_or_else(|| OutputError::Limit("output size overflow".into()))?;
            let lease = budget.reserve(required, cancel).await?;
            let source = self.source.clone();
            let pin = self._pin.clone();
            let store = self.store.clone();
            let read = tokio::task::spawn_blocking(move || {
                let _held = (pin, store);
                let mut source = source.lock().unwrap();
                let mut text = String::new();
                match &mut *source {
                    Source::Memory(channels) => {
                        for (channel, bytes) in channels {
                            append_text(&mut text, *channel, &String::from_utf8_lossy(bytes));
                        }
                    }
                    Source::Files(channels) => {
                        use std::os::unix::fs::FileExt;
                        for (channel, file, length) in channels {
                            let mut bytes = vec![0; *length as usize];
                            file.read_exact_at(&mut bytes, 0)
                                .map_err(|e| OutputError::Incomplete(e.to_string()))?;
                            append_text(&mut text, *channel, &String::from_utf8_lossy(&bytes));
                        }
                    }
                }
                Ok(HostResultBuffer {
                    text,
                    lease: Some(lease),
                })
            });
            tokio::select! {
                _ = cancel.cancelled() => Err(OutputError::Aborted("delivery was cancelled".into())),
                result = read => result.map_err(|e| OutputError::Incomplete(e.to_string()))?,
            }
        })
    }
}

struct Reader {
    snapshot: Arc<Mutex<OutputSnapshot>>,
    store: Arc<StoreInner>,
}

impl OutputReader for Reader {
    fn snapshot(&self) -> OutputSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    fn read(
        &self,
        channel: Channel,
        offset: u64,
        bytes: usize,
    ) -> OutputFuture<'_, Result<Vec<u8>, OutputError>> {
        Box::pin(async move {
            let snapshot = self.snapshot();
            if snapshot.sealed && snapshot.reference.is_none() {
                return Err(OutputError::Incomplete(
                    "complete output was not retained".into(),
                ));
            }
            let saved = snapshot
                .channels
                .iter()
                .find(|c| c.channel == channel)
                .map_or(0, |c| c.saved);
            if offset >= saved && !snapshot.sealed {
                return Ok(Vec::new());
            }
            let pin = {
                let mut ledger = self.store.ledger.lock().unwrap();
                let entry = ledger
                    .get_mut(&snapshot.id)
                    .filter(|entry| !entry.deleting)
                    .ok_or(OutputError::Expired)?;
                let pin = entry.pin.upgrade().unwrap_or_else(|| Arc::new(()));
                entry.pin = Arc::downgrade(&pin);
                pin
            };
            let store = self.store.clone();
            let read = tokio::task::spawn_blocking(move || {
                use std::os::unix::fs::FileExt;
                let _pin = pin;
                let dir = store
                    .objects
                    .open_dir(snapshot.id.to_string())
                    .map_err(|e| OutputError::Incomplete(e.to_string()))?;
                let file = dir
                    .open_file(FileName::Channel(channel), false)
                    .map_err(|e| OutputError::Incomplete(e.to_string()))?;
                let count = bytes.min(saved.saturating_sub(offset) as usize);
                let mut result = vec![0; count];
                file.read_exact_at(&mut result, offset)
                    .map_err(|e| OutputError::Incomplete(e.to_string()))?;
                Ok(result)
            });
            tokio::time::timeout(IO_TIMEOUT, read)
                .await
                .map_err(|_| OutputError::ReadTimeout)?
                .map_err(|e| OutputError::Incomplete(e.to_string()))?
        })
    }
}

fn append_text(text: &mut String, channel: Channel, content: &str) {
    if content.is_empty() {
        return;
    }
    if matches!(channel, Channel::Stderr | Channel::Log) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(if channel == Channel::Stderr {
            "stderr:\n"
        } else {
            "log:\n"
        });
    }
    text.push_str(content);
}

#[cfg(test)]
type TestHook = (
    &'static str,
    Box<dyn FnOnce() -> Result<(), StorageFailure> + Send>,
);
#[cfg(test)]
impl StoreInner {
    fn test_step(&self, step: &'static str) -> Result<(), StorageFailure> {
        let hook = {
            let mut pending = self.hook.lock().unwrap();
            if pending.as_ref().is_some_and(|(name, _)| *name == step) {
                pending.take()
            } else {
                None
            }
        };
        if let Some((_, hook)) = hook {
            hook()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
