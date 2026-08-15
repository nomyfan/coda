//! Workspace file listing behind the composer's `@` picker.
//!
//! The walk uses `ignore` — the crate `rg` and `fd` are themselves built on — so
//! the picker offers the same files the agent's own `glob`/`grep` tools would
//! find: `.gitignore`/`.ignore` rules are honoured, and `.git` is pruned. Dot
//! files are *not* hidden, because the whole point of the picker is to name a
//! file the model should look at and `.github/workflows/ci.yml` is as nameable
//! as any other path.
//!
//! One walk serves every query that lands within [`CACHE_TTL`] of it: a picker
//! searches once per keystroke, and re-walking a large workspace for each of
//! those would be pure waste.

use ignore::WalkBuilder;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One entry the picker can insert: a path relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
}

/// The ranked answer to one query.
#[derive(Debug, Clone)]
pub struct FileMatches {
    pub files: Vec<FileEntry>,
    /// Entries were left out — more matched than `limit`, or the walk itself hit
    /// [`MAX_ENTRIES`]. Either way the list is not the whole truth, and the
    /// picker says so rather than implying the workspace holds nothing else.
    pub truncated: bool,
}

/// Ceiling on one walk. A workspace bigger than this still searches, it just
/// searches the first `MAX_ENTRIES` paths the walker yields.
const MAX_ENTRIES: usize = 20_000;

/// How long a completed walk stays reusable. Long enough that typing a query
/// costs one walk, short enough that a file created mid-conversation shows up
/// without the user wondering why it doesn't.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Matches returned when the client doesn't ask for a specific count.
pub const DEFAULT_LIMIT: usize = 50;
/// Ceiling on what a client may ask for: the picker is a menu, not an export.
pub const MAX_LIMIT: usize = 200;

/// A workspace's file list, walked on demand and reused for [`CACHE_TTL`].
pub struct FileIndex {
    root: String,
    ttl: Duration,
    cache: Mutex<Option<Listing>>,
}

/// One completed walk.
#[derive(Clone)]
struct Listing {
    walked_at: Instant,
    /// Shared, never mutated: a query ranks against this snapshot while a later
    /// walk may already be replacing it.
    entries: Arc<Vec<FileEntry>>,
    truncated: bool,
}

impl FileIndex {
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            ttl: CACHE_TTL,
            cache: Mutex::new(None),
        }
    }

    /// An index whose walks expire after `ttl` — lets a test exercise both sides
    /// of the cache without sleeping through [`CACHE_TTL`].
    #[cfg(test)]
    fn with_ttl(root: impl Into<String>, ttl: Duration) -> Self {
        Self {
            ttl,
            ..Self::new(root)
        }
    }

    /// The best `limit` matches for `query`, ranked by [`fuzzy_score`]. An empty
    /// query is not an error — it's the state right after typing `@`, and it
    /// answers with the shallowest paths in the workspace.
    pub async fn search(&self, query: &str, limit: usize) -> Result<FileMatches, String> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let listing = self.listing().await?;
        let (files, over_limit) = rank(&listing.entries, query, limit);
        Ok(FileMatches {
            files,
            truncated: over_limit || listing.truncated,
        })
    }

    /// A walk from within the TTL, or a fresh one. Two concurrent misses both
    /// walk and the later one wins: holding a lock across the walk would either
    /// block the runtime or serialize every search behind the slowest one, and a
    /// duplicated walk costs nothing but time.
    async fn listing(&self) -> Result<Listing, String> {
        if let Some(fresh) = self.fresh() {
            return Ok(fresh);
        }
        let root = self.root.clone();
        let (entries, truncated) = tokio::task::spawn_blocking(move || walk(Path::new(&root)))
            .await
            .map_err(|err| format!("failed to walk the workspace: {err}"))?;
        let listing = Listing {
            walked_at: Instant::now(),
            entries: Arc::new(entries),
            truncated,
        };
        *lock(&self.cache) = Some(listing.clone());
        Ok(listing)
    }

    fn fresh(&self) -> Option<Listing> {
        lock(&self.cache)
            .clone()
            .filter(|listing| listing.walked_at.elapsed() < self.ttl)
    }
}

/// Take the cache lock, recovering from poisoning: the guarded sections only
/// clone, so a poisoned lock means an unrelated panic and the cached list is
/// still perfectly good data.
fn lock(cache: &Mutex<Option<Listing>>) -> std::sync::MutexGuard<'_, Option<Listing>> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Walk `root`, returning workspace-relative paths and whether the walk was cut
/// short at [`MAX_ENTRIES`]. Sorted shallowest-first, then alphabetically, which
/// is both the order an empty query answers with and the tie-break within a
/// ranked search.
fn walk(root: &Path) -> (Vec<FileEntry>, bool) {
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut truncated = false;

    let walker = WalkBuilder::new(root)
        // Dot files are listed (see the module docs); `.git` never is.
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .follow_links(false)
        .build();

    for entry in walker {
        // An unreadable directory or a broken symlink is skipped, not fatal: a
        // picker that returns nothing because one subdirectory lost its read bit
        // would be worse than one that returns everything else.
        let Ok(entry) = entry else { continue };
        // Depth 0 is `root` itself.
        if entry.depth() == 0 {
            continue;
        }
        let Some(path) = entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|relative| relative.to_str())
        else {
            continue;
        };
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(FileEntry {
            path: path.to_string(),
            is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
        });
    }

    entries.sort_by(|a, b| {
        depth(&a.path)
            .cmp(&depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });
    (entries, truncated)
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

/// The best `limit` entries for `query`, and whether anything was left out.
fn rank(entries: &[FileEntry], query: &str, limit: usize) -> (Vec<FileEntry>, bool) {
    if query.is_empty() {
        return (
            entries.iter().take(limit).cloned().collect(),
            entries.len() > limit,
        );
    }

    let mut scored: Vec<(i32, &FileEntry)> = entries
        .iter()
        .filter_map(|entry| fuzzy_score(&entry.path, query).map(|score| (score, entry)))
        .collect();
    let over_limit = scored.len() > limit;
    // A stable sort on the score alone keeps the index's shallow-first,
    // alphabetical order as the tie-break.
    scored.sort_by(|(a, _), (b, _)| b.cmp(a));
    (
        scored
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry.clone())
            .collect(),
        over_limit,
    )
}

const MATCH_SCORE: i32 = 8;
/// The match starts a path segment or a word within one — `co` in `coda/…` or in
/// `my_coda`, not the `co` buried in `discover`.
const BOUNDARY_BONUS: i32 = 12;
const CONSECUTIVE_BONUS: i32 = 10;
/// The match reads against the file name rather than the directories leading to
/// it — what someone typing a few letters almost always means.
const BASENAME_BONUS: i32 = 24;
/// Divisor turning path length into a penalty, so that among equally good
/// matches the shortest path wins.
const LENGTH_PENALTY: i32 = 4;
/// Divisor turning the name's *unmatched* characters into a penalty: between two
/// names that both start with the query, the shorter one is the better answer.
const LEFTOVER_PENALTY: i32 = 2;

/// Score `query` against `path`, or `None` when the query's characters don't
/// appear in `path` in order. Matching is ASCII-case-insensitive; non-ASCII
/// characters have to match exactly, which for a file picker is a fair trade
/// against carrying a full case-folding table.
///
/// The query is scored twice — against the whole path and against the file name
/// alone — and the better reading wins. Scoring the name separately is what
/// keeps `composer` finding `…/components/composer.tsx` rather than whichever
/// path happens to scatter those letters most tidily, and the leftover-name
/// penalty then prefers the *tightest* such name.
pub fn fuzzy_score(path: &str, query: &str) -> Option<i32> {
    let needles: Vec<char> = query.chars().collect();
    if needles.is_empty() {
        return Some(0);
    }

    let basename = path.rsplit('/').next().unwrap_or(path);
    let as_name = subsequence_score(basename, &needles).map(|score| {
        let leftover = basename.chars().count().saturating_sub(needles.len()) as i32;
        score + BASENAME_BONUS - leftover / LEFTOVER_PENALTY
    });
    let best = subsequence_score(path, &needles).max(as_name)?;
    Some(best - path.chars().count() as i32 / LENGTH_PENALTY)
}

/// How well `needles` reads as a subsequence of `candidate`, or `None` when it
/// doesn't. Greedy: each needle takes the first position it can, which costs the
/// occasional ideal alignment and saves a full search per candidate.
fn subsequence_score(candidate: &str, needles: &[char]) -> Option<i32> {
    let mut next = 0usize;
    let mut score = 0i32;
    let mut previous: Option<char> = None;
    let mut previous_matched = false;

    for ch in candidate.chars() {
        if next == needles.len() {
            break;
        }
        if ch.eq_ignore_ascii_case(&needles[next]) {
            score += MATCH_SCORE;
            if previous_matched {
                score += CONSECUTIVE_BONUS;
            }
            if previous.is_none_or(is_boundary) {
                score += BOUNDARY_BONUS;
            }
            next += 1;
            previous_matched = true;
        } else {
            previous_matched = false;
        }
        previous = Some(ch);
    }

    (next == needles.len()).then_some(score)
}

fn is_boundary(ch: char) -> bool {
    matches!(ch, '/' | '_' | '-' | '.' | ' ')
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
