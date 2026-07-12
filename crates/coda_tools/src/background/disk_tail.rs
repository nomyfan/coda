//! Fixed-capacity ring file presenting a logically continuous byte stream.
//!
//! `DiskTail` is the storage primitive behind a background task's stdout/stderr
//! spool (design: `docs/design/background-task-output-spool.md`). It hides the
//! physical ring layout — wrap-around, split reads/writes, and the mapping from
//! an absolute logical offset to a physical file position — behind an append /
//! read-from-cursor / tail interface addressed entirely in absolute logical
//! offsets, so a reader's cursor stays valid even after the head is overwritten.
//!
//! Physical position of logical offset `o` is `o % capacity`. The retained
//! window is `start_offset..total_written`, where
//! `start_offset == total_written.saturating_sub(capacity)`; bytes below
//! `start_offset` have been overwritten and are unrecoverable, but that loss is
//! always observable via [`OutputChunk::lost`].
//!
//! This module is deliberately standalone (Roadmap phase 1): it owns an already
//! opened file and never touches paths — safe, fd-relative file creation is the
//! `ArchiveDir` layer's job. All blocking file I/O runs on the blocking pool via
//! positioned reads/writes (`pread`/`pwrite`), which need no shared file cursor;
//! a per-stream async mutex serializes appends and reads so a reader always sees
//! a consistent logical range.

use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

#[cfg(test)]
use std::sync::Mutex as StdMutex;

use tokio::sync::Mutex;

/// Physical layout format of a ring file. Persisted per stream; an unknown
/// version is rejected rather than reinterpreted under the current layout.
// Consumed by the manifest layer (roadmap phase 2, `StreamManifest`), which is
// where a persisted `layout_version` is checked against this value.
#[allow(dead_code)]
pub const LAYOUT_VERSION: u32 = 1;

/// Supported per-stream capacity bounds. The lower bound makes the retained
/// index provably bounded under the session quota; the upper bound caps a
/// single stream's disk footprint. A manifest capacity outside this range is
/// treated as corrupt.
pub const MIN_CAPACITY: u64 = 64 * 1024;
pub const MAX_CAPACITY: u64 = 64 * 1024 * 1024;

/// One incremental read: bytes from the requested cursor plus the accounting a
/// caller needs to advance its persisted cursor and report loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub bytes: Vec<u8>,
    /// Bytes between the requested cursor and the retained window start that
    /// were overwritten before this read could observe them.
    pub lost: u64,
    /// Advances only to the end of the bytes actually returned; when data
    /// remains the next call continues from here.
    pub next_cursor: u64,
    pub has_more: bool,
}

/// Mutable ring state guarded by the per-stream lock.
struct Offsets {
    /// Earliest logical offset still retained.
    start_offset: u64,
    /// Next logical offset to be written (i.e. total bytes ever appended).
    total_written: u64,
    /// First append failure, retained so a cancelled pump cannot hide it from
    /// the terminal flush barrier.
    failure: Option<String>,
}

/// A fixed-capacity ring file exposing a logically continuous byte stream.
pub struct DiskTail {
    /// Positioned I/O needs only `&File`, so the handle is shared with the
    /// blocking worker without moving it out of the guarded state. `None` for a
    /// *detached* tail whose ring file was deleted (Consumed/Expired): it keeps
    /// the recorded logical range for manifest consistency but has no bytes.
    file: Option<Arc<std::fs::File>>,
    capacity: u64,
    state: Arc<Mutex<Offsets>>,
    #[cfg(test)]
    append_pause: Arc<StdMutex<Option<AppendPause>>>,
}

impl DiskTail {
    /// Wrap a freshly created, empty ring file. The file must be zero-length;
    /// a non-empty file here means a create/truncate step was skipped.
    pub fn create(file: std::fs::File, capacity: u64) -> io::Result<Self> {
        check_capacity(capacity)?;
        Self::create_inner(file, capacity)
    }

    /// Reopen a persisted ring at its recorded logical range. Validates the
    /// invariants that keep the `logical_offset % capacity` mapping sound;
    /// any inconsistency is reported as corrupt rather than guessed at.
    pub fn reopen(
        file: std::fs::File,
        capacity: u64,
        start_offset: u64,
        total_written: u64,
    ) -> io::Result<Self> {
        check_capacity(capacity)?;
        Self::reopen_inner(file, capacity, start_offset, total_written)
    }

    fn create_inner(file: std::fs::File, capacity: u64) -> io::Result<Self> {
        let len = file.metadata()?.len();
        if len != 0 {
            return Err(corrupt(format!(
                "new ring file must be empty, found {len} bytes"
            )));
        }
        Ok(DiskTail {
            file: Some(Arc::new(file)),
            capacity,
            state: Arc::new(Mutex::new(Offsets {
                start_offset: 0,
                total_written: 0,
                failure: None,
            })),
            #[cfg(test)]
            append_pause: Arc::new(StdMutex::new(None)),
        })
    }

    /// A tail with no backing file, for a Consumed/Expired task whose rings were
    /// deleted. It preserves the recorded logical range so a manifest re-save
    /// stays consistent; any attempt to read retained bytes errors, since reads
    /// are gated on disposition before they reach here.
    pub fn detached(capacity: u64, start_offset: u64, total_written: u64) -> io::Result<Self> {
        check_capacity(capacity)?;
        if start_offset > total_written
            || total_written - start_offset > capacity
            || start_offset != total_written.saturating_sub(capacity)
        {
            return Err(corrupt("detached range violates ring invariants"));
        }
        Ok(DiskTail {
            file: None,
            capacity,
            state: Arc::new(Mutex::new(Offsets {
                start_offset,
                total_written,
                failure: None,
            })),
            #[cfg(test)]
            append_pause: Arc::new(StdMutex::new(None)),
        })
    }

    fn reopen_inner(
        file: std::fs::File,
        capacity: u64,
        start_offset: u64,
        total_written: u64,
    ) -> io::Result<Self> {
        if start_offset > total_written {
            return Err(corrupt(format!(
                "start_offset {start_offset} exceeds total_written {total_written}"
            )));
        }
        if total_written - start_offset > capacity {
            return Err(corrupt(format!(
                "retained window {} exceeds capacity {capacity}",
                total_written - start_offset
            )));
        }
        if start_offset != total_written.saturating_sub(capacity) {
            return Err(corrupt(format!(
                "start_offset {start_offset} is not the ring-implied {}",
                total_written.saturating_sub(capacity)
            )));
        }
        let expected = total_written.min(capacity);
        let len = file.metadata()?.len();
        if len != expected {
            return Err(corrupt(format!(
                "ring file length {len} does not match expected {expected}"
            )));
        }
        Ok(DiskTail {
            file: Some(Arc::new(file)),
            capacity,
            state: Arc::new(Mutex::new(Offsets {
                start_offset,
                total_written,
                failure: None,
            })),
            #[cfg(test)]
            append_pause: Arc::new(StdMutex::new(None)),
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Snapshot of the retained logical range `(start_offset, total_written)` —
    /// what a manifest snapshot persists after a flush barrier.
    pub async fn logical_range(&self) -> (u64, u64) {
        let st = self.state.lock().await;
        (st.start_offset, st.total_written)
    }

    /// Append `bytes`, overwriting the oldest retained content once capacity is
    /// exceeded while keeping logical offsets monotonically increasing. I/O
    /// errors surface unchanged; the caller must not silently degrade into lost
    /// output.
    pub async fn append(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let cap = self.capacity;
        let mut st = self.state.clone().lock_owned().await;
        if let Some(message) = &st.failure {
            return Err(io::Error::other(message.clone()));
        }
        let old_total = st.total_written;
        let new_total = old_total + bytes.len() as u64;
        // Only the last `capacity` bytes can survive, so never write more than
        // one ring's worth: earlier bytes of an over-capacity append would be
        // immediately overwritten at the same physical positions.
        let (logical_start, payload): (u64, Vec<u8>) = if bytes.len() as u64 >= cap {
            (
                new_total - cap,
                bytes[bytes.len() - cap as usize..].to_vec(),
            )
        } else {
            (old_total, bytes.to_vec())
        };
        let file = self
            .file
            .clone()
            .ok_or_else(|| corrupt("append on a detached ring"))?;
        #[cfg(test)]
        let pause = self.append_pause.lock().unwrap().take();
        let transaction = tokio::spawn(async move {
            let write = tokio::task::spawn_blocking(move || {
                write_ring(&file, cap, logical_start, &payload)?;
                #[cfg(test)]
                if let Some(pause) = pause {
                    let _ = pause.entered.send(());
                    let _ = pause.release.recv();
                }
                Ok(())
            })
            .await
            .map_err(join_err)
            .and_then(|result| result);
            if write.is_ok() {
                st.total_written = new_total;
                st.start_offset = new_total.saturating_sub(cap);
            } else if let Err(error) = &write {
                st.failure = Some(error.to_string());
            }
            write
        });
        transaction.await.map_err(join_err)?
    }

    #[cfg(test)]
    fn pause_next_append(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *self.append_pause.lock().unwrap() = Some(AppendPause {
            entered: entered_tx,
            release: release_rx,
        });
        (entered_rx, release_tx)
    }

    /// Read up to `limit` bytes from absolute logical `cursor`. A cursor below
    /// the retained window start yields the still-readable remainder and
    /// reports the overwritten unread bytes in [`OutputChunk::lost`].
    pub async fn read_from(&self, cursor: u64, limit: usize) -> io::Result<OutputChunk> {
        let cap = self.capacity;
        let st = self.state.lock().await;
        let (start, total) = (st.start_offset, st.total_written);
        let lost = start.saturating_sub(cursor);
        let eff = cursor.max(start);
        let avail = total.saturating_sub(eff);
        let n = avail.min(limit as u64);
        let next_cursor = eff + n;
        let has_more = next_cursor < total;
        let bytes = if n == 0 {
            Vec::new()
        } else {
            let file = self
                .file
                .clone()
                .ok_or_else(|| corrupt("read on a detached ring"))?;
            let len = n as usize;
            tokio::task::spawn_blocking(move || read_ring(&file, cap, eff, len))
                .await
                .map_err(join_err)??
        };
        Ok(OutputChunk {
            bytes,
            lost,
            next_cursor,
            has_more,
        })
    }

    /// Last `limit` bytes of the retained window, without moving any cursor.
    pub async fn tail(&self, limit: usize) -> io::Result<Vec<u8>> {
        let cap = self.capacity;
        let st = self.state.lock().await;
        let total = st.total_written;
        let retained = total.min(cap);
        let n = retained.min(limit as u64);
        if n == 0 {
            return Ok(Vec::new());
        }
        let logical_start = total - n;
        let file = self
            .file
            .clone()
            .ok_or_else(|| corrupt("tail on a detached ring"))?;
        let len = n as usize;
        tokio::task::spawn_blocking(move || read_ring(&file, cap, logical_start, len))
            .await
            .map_err(join_err)?
    }

    /// Durability barrier for a normal release: flush written data to disk. The
    /// stream lock is held so the flushed range stays consistent with the
    /// logical range a manifest snapshot records right after.
    pub async fn flush(&self) -> io::Result<()> {
        let st = self.state.lock().await;
        if let Some(message) = &st.failure {
            return Err(io::Error::other(message.clone()));
        }
        let Some(file) = self.file.clone() else {
            return Ok(()); // detached: nothing to flush
        };
        tokio::task::spawn_blocking(move || file.sync_data())
            .await
            .map_err(join_err)?
    }
}

#[cfg(test)]
struct AppendPause {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// Write `data` (at most one ring's worth) so byte `data[i]` lands at physical
/// `(logical_start + i) % capacity`, splitting at the wrap into two `pwrite`s.
fn write_ring(
    file: &std::fs::File,
    capacity: u64,
    logical_start: u64,
    data: &[u8],
) -> io::Result<()> {
    debug_assert!(data.len() as u64 <= capacity);
    let phys = (logical_start % capacity) as usize;
    let first = std::cmp::min(data.len(), capacity as usize - phys);
    file.write_all_at(&data[..first], phys as u64)?;
    if first < data.len() {
        file.write_all_at(&data[first..], 0)?;
    }
    Ok(())
}

/// Read `len` (at most one ring's worth) bytes starting at logical
/// `logical_start`, splitting at the wrap into two `pread`s.
fn read_ring(
    file: &std::fs::File,
    capacity: u64,
    logical_start: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    debug_assert!(len as u64 <= capacity);
    let phys = (logical_start % capacity) as usize;
    let first = std::cmp::min(len, capacity as usize - phys);
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf[..first], phys as u64)?;
    if first < len {
        file.read_exact_at(&mut buf[first..], 0)?;
    }
    Ok(buf)
}

fn check_capacity(capacity: u64) -> io::Result<()> {
    if !(MIN_CAPACITY..=MAX_CAPACITY).contains(&capacity) {
        return Err(corrupt(format!(
            "capacity {capacity} out of range [{MIN_CAPACITY}, {MAX_CAPACITY}]"
        )));
    }
    Ok(())
}

fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn join_err(err: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("disk tail worker failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte-exact reference model: keeps the whole logical stream and derives
    /// every answer from first principles, so `DiskTail` is checked against an
    /// obviously-correct oracle rather than a re-derivation of its own math.
    struct Model {
        data: Vec<u8>,
        cap: u64,
    }

    impl Model {
        fn new(cap: u64) -> Self {
            Model {
                data: Vec::new(),
                cap,
            }
        }

        fn append(&mut self, bytes: &[u8]) {
            self.data.extend_from_slice(bytes);
        }

        fn read_from(&self, cursor: u64, limit: usize) -> OutputChunk {
            let total = self.data.len() as u64;
            let start = total.saturating_sub(self.cap);
            let lost = start.saturating_sub(cursor);
            let eff = cursor.max(start);
            let avail = total.saturating_sub(eff);
            let n = avail.min(limit as u64);
            let bytes = self.data[eff as usize..(eff + n) as usize].to_vec();
            OutputChunk {
                bytes,
                lost,
                next_cursor: eff + n,
                has_more: eff + n < total,
            }
        }

        fn tail(&self, limit: usize) -> Vec<u8> {
            let total = self.data.len() as u64;
            let n = total.min(self.cap).min(limit as u64) as usize;
            self.data[self.data.len() - n..].to_vec()
        }
    }

    fn temp_file() -> std::fs::File {
        tempfile::tempfile().expect("create temp file")
    }

    /// A fresh empty ring: nothing to read, no loss, no tail.
    #[tokio::test]
    async fn empty_ring_reads_nothing() {
        let tail = DiskTail::create_inner(temp_file(), 16).unwrap();
        let chunk = tail.read_from(0, 64).await.unwrap();
        assert_eq!(
            chunk,
            OutputChunk {
                bytes: Vec::new(),
                lost: 0,
                next_cursor: 0,
                has_more: false,
            }
        );
        assert!(tail.tail(64).await.unwrap().is_empty());
        assert_eq!(tail.logical_range().await, (0, 0));
    }

    /// Below capacity: everything is retained and read back verbatim.
    #[tokio::test]
    async fn sub_capacity_reads_back_verbatim() {
        let tail = DiskTail::create_inner(temp_file(), 16).unwrap();
        tail.append(b"hello").await.unwrap();
        let chunk = tail.read_from(0, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"hello");
        assert_eq!(chunk.lost, 0);
        assert_eq!(chunk.next_cursor, 5);
        assert!(!chunk.has_more);
        assert_eq!(tail.logical_range().await, (0, 5));
    }

    /// A write filling exactly capacity keeps the whole content, start stays 0.
    #[tokio::test]
    async fn exactly_capacity_retains_all() {
        let cap = 8u64;
        let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
        tail.append(b"ABCDEFGH").await.unwrap();
        assert_eq!(tail.logical_range().await, (0, 8));
        let chunk = tail.read_from(0, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"ABCDEFGH");
        assert_eq!(chunk.lost, 0);
    }

    /// A single over-capacity write keeps only the last `capacity` bytes and
    /// reports the overwritten prefix as lost to a cursor at 0.
    #[tokio::test]
    async fn single_over_capacity_write_keeps_tail() {
        let cap = 8u64;
        let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
        tail.append(b"0123456789AB").await.unwrap(); // 12 bytes
        assert_eq!(tail.logical_range().await, (4, 12));
        let chunk = tail.read_from(0, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"456789AB");
        assert_eq!(chunk.lost, 4);
        assert_eq!(chunk.next_cursor, 12);
        assert!(!chunk.has_more);
    }

    /// Many small appends wrap the ring repeatedly; a cursor kept current never
    /// loses a byte, and once it falls behind the loss is exact.
    #[tokio::test]
    async fn repeated_wrap_tracks_cursor_and_loss() {
        let cap = 8u64;
        let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
        let mut model = Model::new(cap);
        let mut cursor = 0u64;
        let mut collected = Vec::new();
        for i in 0..40u8 {
            let chunk = [b'a' + (i % 26)];
            tail.append(&chunk).await.unwrap();
            model.append(&chunk);
            // Keep the cursor current every step: no loss, full fidelity.
            let got = tail.read_from(cursor, 64).await.unwrap();
            let want = model.read_from(cursor, 64);
            assert_eq!(got, want, "step {i}");
            collected.extend_from_slice(&got.bytes);
            cursor = got.next_cursor;
        }
        assert_eq!(collected.len(), 40);
        assert_eq!(tail.logical_range().await, (32, 40));

        // A stale cursor at 0 now: 32 bytes lost, last 8 readable.
        let stale = tail.read_from(0, 64).await.unwrap();
        assert_eq!(stale.lost, 32);
        assert_eq!(stale.bytes, model.read_from(0, 64).bytes);
    }

    /// A read crossing the physical wrap boundary reassembles both segments in
    /// logical order.
    #[tokio::test]
    async fn read_across_wrap_boundary() {
        let cap = 8u64;
        let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
        tail.append(b"01234567").await.unwrap(); // fills ring, phys 0..8
        tail.append(b"89A").await.unwrap(); // overwrites phys 0..3
        // Retained window is logical 3..11 → "3456789A", physically split as
        // [phys 3..8]="34567" then [phys 0..3]="89A".
        assert_eq!(tail.logical_range().await, (3, 11));
        let chunk = tail.read_from(3, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"3456789A");
    }

    /// `limit` splits a read into segments; `has_more` and `next_cursor` drive
    /// continuation without overlap or gaps.
    #[tokio::test]
    async fn segmented_read_has_more() {
        let tail = DiskTail::create_inner(temp_file(), 64).unwrap();
        tail.append(b"abcdefghij").await.unwrap();
        let first = tail.read_from(0, 4).await.unwrap();
        assert_eq!(first.bytes, b"abcd");
        assert_eq!(first.next_cursor, 4);
        assert!(first.has_more);
        let second = tail.read_from(first.next_cursor, 4).await.unwrap();
        assert_eq!(second.bytes, b"efgh");
        assert!(second.has_more);
        let third = tail.read_from(second.next_cursor, 4).await.unwrap();
        assert_eq!(third.bytes, b"ij");
        assert!(!third.has_more);
    }

    /// tail() returns the last bytes without disturbing the read cursor model.
    #[tokio::test]
    async fn tail_returns_last_bytes() {
        let cap = 8u64;
        let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
        tail.append(b"0123456789").await.unwrap();
        assert_eq!(tail.tail(3).await.unwrap(), b"789");
        assert_eq!(tail.tail(100).await.unwrap(), b"23456789"); // capped at retained window
    }

    /// Reopening at the persisted logical range reads back the same retained
    /// window and continues appending coherently.
    #[tokio::test]
    async fn reopen_at_persisted_range() {
        let file = temp_file();
        let dup = file.try_clone().unwrap();
        let tail = DiskTail::create_inner(file, 8).unwrap();
        tail.append(b"0123456789AB").await.unwrap(); // window 4..12
        tail.flush().await.unwrap();
        let (start, total) = tail.logical_range().await;
        assert_eq!((start, total), (4, 12));
        drop(tail);

        let reopened = DiskTail::reopen_inner(dup, 8, start, total).unwrap();
        let chunk = reopened.read_from(4, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"456789AB");
        reopened.append(b"CD").await.unwrap();
        assert_eq!(reopened.logical_range().await, (6, 14));
        assert_eq!(reopened.read_from(6, 64).await.unwrap().bytes, b"6789ABCD");
    }

    #[tokio::test]
    async fn cancelled_append_finishes_offsets_before_flush_and_reopen() {
        let file = temp_file();
        let reopened_file = file.try_clone().unwrap();
        let tail = Arc::new(DiskTail::create_inner(file, 16).unwrap());
        let (entered, release) = tail.pause_next_append();
        let append_tail = tail.clone();
        let append = tokio::spawn(async move { append_tail.append(b"hello").await });
        tokio::task::spawn_blocking(move || entered.recv().unwrap())
            .await
            .unwrap();

        append.abort();
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(100), append)
            .await
            .expect("cancelling append blocked the Tokio worker")
            .unwrap_err();
        assert!(cancelled.is_cancelled());

        let flush_tail = tail.clone();
        let mut flush = tokio::spawn(async move { flush_tail.flush().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut flush)
                .await
                .is_err(),
            "flush passed an in-flight append"
        );
        release.send(()).unwrap();
        flush.await.unwrap().unwrap();

        let range = tail.logical_range().await;
        assert_eq!(range, (0, 5));
        drop(tail);
        let reopened = DiskTail::reopen_inner(reopened_file, 16, range.0, range.1).unwrap();
        assert_eq!(reopened.read_from(0, 16).await.unwrap().bytes, b"hello");
    }

    /// Reopen rejects a file whose length disagrees with the recorded range,
    /// rather than silently reinterpreting stale bytes.
    #[tokio::test]
    async fn reopen_rejects_length_mismatch() {
        let file = temp_file();
        file.set_len(3).unwrap();
        // Claims window 4..12 (expected physical length 8) but file is 3 bytes.
        let err = DiskTail::reopen_inner(file, 8, 4, 12)
            .err()
            .expect("length mismatch rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Reopen rejects a start_offset that is not the ring-implied value.
    #[tokio::test]
    async fn reopen_rejects_inconsistent_start() {
        let file = temp_file();
        file.set_len(8).unwrap();
        // total 12, cap 8 → implied start 4, but claims 2.
        let err = DiskTail::reopen_inner(file, 8, 2, 12)
            .err()
            .expect("inconsistent start rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Public constructors enforce the supported capacity bounds.
    #[tokio::test]
    async fn capacity_bounds_enforced() {
        assert!(DiskTail::create(temp_file(), MIN_CAPACITY - 1).is_err());
        assert!(DiskTail::create(temp_file(), MAX_CAPACITY + 1).is_err());
        assert!(DiskTail::create(temp_file(), MIN_CAPACITY).is_ok());
    }

    /// create() rejects a non-empty file — a missing truncate is a bug, not a
    /// resumable state.
    #[tokio::test]
    async fn create_rejects_nonempty_file() {
        let file = temp_file();
        file.set_len(4).unwrap();
        assert!(DiskTail::create_inner(file, 16).is_err());
    }

    /// Deterministic differential test: a pseudo-random op stream against the
    /// reference model across capacities and payload sizes that straddle the
    /// wrap boundary. Any divergence in bytes, loss, cursor, or has_more fails.
    #[tokio::test]
    async fn differential_against_model() {
        // A small xorshift keeps the sequence reproducible without a dep.
        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for &cap in &[1u64, 2, 3, 7, 8, 16, 33] {
            let tail = DiskTail::create_inner(temp_file(), cap).unwrap();
            let mut model = Model::new(cap);
            let mut cursor = 0u64;
            let mut counter: u64 = 0;

            for _ in 0..300 {
                match next() % 5 {
                    0 | 1 => {
                        // Append a payload of length 0..=2*cap+1 with unique,
                        // position-revealing bytes.
                        let len = (next() % (2 * cap + 2)) as usize;
                        let mut payload = Vec::with_capacity(len);
                        for _ in 0..len {
                            payload.push((counter % 251) as u8);
                            counter += 1;
                        }
                        tail.append(&payload).await.unwrap();
                        model.append(&payload);
                    }
                    2 => {
                        // Read from the maintained cursor and advance it.
                        let limit = (next() % (cap + 3) + 1) as usize;
                        let got = tail.read_from(cursor, limit).await.unwrap();
                        let want = model.read_from(cursor, limit);
                        assert_eq!(got, want, "cap {cap} cursor read");
                        cursor = got.next_cursor;
                    }
                    3 => {
                        // Read from an arbitrary (possibly stale) cursor without
                        // advancing the maintained one — exercises the loss path.
                        let total = model.data.len() as u64;
                        let c = if total == 0 { 0 } else { next() % (total + 1) };
                        let limit = (next() % (cap + 3) + 1) as usize;
                        let got = tail.read_from(c, limit).await.unwrap();
                        let want = model.read_from(c, limit);
                        assert_eq!(got, want, "cap {cap} stale read at {c}");
                    }
                    _ => {
                        let limit = (next() % (cap + 3)) as usize;
                        assert_eq!(
                            tail.tail(limit).await.unwrap(),
                            model.tail(limit),
                            "cap {cap} tail"
                        );
                    }
                }
                let (start, total) = tail.logical_range().await;
                assert_eq!(total, model.data.len() as u64);
                assert_eq!(start, total.saturating_sub(cap));
            }
        }
    }
}
