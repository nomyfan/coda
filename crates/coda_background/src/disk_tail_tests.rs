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
