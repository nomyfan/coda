//! Measures what previews really allocate: a preview stays within its
//! capacity whatever the input's shape, and rendering allocates in proportion
//! to the text rendered, not to the lines retained.
//!
//! It counts every allocation in the process, so this file holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use coda_core::output::Preview;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
    PEAK.fetch_max(LIVE.fetch_add(bytes, SeqCst) + bytes, SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), SeqCst);
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Counted as a fresh allocation freed after the copy, its worst case.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grow(new_size);
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        LIVE.fetch_sub(layout.size(), SeqCst);
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Most bytes `work` held at once beyond what was live when it started.
fn peak_of<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(SeqCst);
    PEAK.store(base, SeqCst);
    let value = work();
    (value, PEAK.load(SeqCst) - base)
}

#[test]
fn previews_stay_within_capacity_and_render_in_proportion_to_their_text() {
    let lines = |count: usize, width: usize| -> Vec<u8> {
        (0..count)
            .flat_map(|_| std::iter::repeat_n(b'x', width).chain([b'\n']))
            .collect()
    };
    let inputs = [
        ("empty lines", lines(200_000, 0)),
        ("one-byte lines", lines(100_000, 1)),
        ("short lines", lines(20_000, 40)),
        ("near-limit lines", lines(500, 1999)),
        ("one huge line", vec![b'y'; 1 << 20]),
        (
            "mixed",
            [lines(3000, 7), lines(20, 5000), lines(3000, 0)].concat(),
        ),
    ];
    for capacity in [1024, 28 * 1024, 256 * 1024] {
        for (shape, input) in &inputs {
            for chunk in [7, 16 * 1024] {
                let (preview, peak) = peak_of(|| {
                    let mut preview = Preview::new(capacity);
                    for piece in input.chunks(chunk) {
                        preview.append(piece);
                    }
                    preview
                });
                assert!(
                    peak <= capacity,
                    "{shape} in {chunk}-byte chunks peaked at {peak} bytes over a {capacity}-byte preview"
                );
                let (_, rendered) = peak_of(|| preview.render(1024));
                assert!(
                    rendered <= 8 * 1024,
                    "{shape}: rendering 1 KiB of a {capacity}-byte preview took {rendered} bytes"
                );
            }
        }
    }
}
