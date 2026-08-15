use super::*;
use std::fs;

/// A workspace holding `paths` (each relative, `dir/` for a directory), rooted at
/// a fresh temp dir that looks like a git checkout so `.gitignore` applies —
/// `ignore` only honours it inside a repository.
fn workspace(paths: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git").join("config"), "[core]\n").unwrap();
    for path in paths {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if path.ends_with('/') {
            fs::create_dir_all(&full).unwrap();
        } else {
            fs::write(&full, "x").unwrap();
        }
    }
    dir
}

fn paths(entries: &[FileEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.path.as_str()).collect()
}

// --- walk --------------------------------------------------------------------

#[test]
fn walk_lists_relative_paths_shallowest_first() {
    let dir = workspace(&["src/app/deep.rs", "src/main.rs", "README.md"]);
    let (entries, truncated) = walk(dir.path());

    assert!(!truncated);
    assert_eq!(
        paths(&entries),
        [
            "README.md",
            "src",
            "src/app",
            "src/main.rs",
            "src/app/deep.rs"
        ]
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.path == "src" && entry.is_dir)
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.path == "src/main.rs" && !entry.is_dir)
    );
}

#[test]
fn walk_prunes_the_git_directory() {
    let dir = workspace(&["src/main.rs"]);
    let (entries, _) = walk(dir.path());

    assert!(
        !paths(&entries).iter().any(|path| path.starts_with(".git")),
        "`.git` must never reach the picker: {:?}",
        paths(&entries)
    );
}

#[test]
fn walk_honours_ignore_rules() {
    let dir = workspace(&["target/debug/binary", "src/main.rs", "notes.txt"]);
    fs::write(dir.path().join(".gitignore"), "target/\nnotes.txt\n").unwrap();
    let (entries, _) = walk(dir.path());

    let listed = paths(&entries);
    assert!(listed.contains(&"src/main.rs"));
    assert!(
        listed.contains(&".gitignore"),
        "the rules file itself is a real file"
    );
    assert!(!listed.iter().any(|path| path.starts_with("target")));
    assert!(!listed.contains(&"notes.txt"));
}

/// Dot files stay listed: naming `.github/workflows/ci.yml` is exactly what the
/// picker is for. Only `.git` is special-cased.
#[test]
fn walk_lists_dot_files() {
    let dir = workspace(&[".github/workflows/ci.yml", ".env.example"]);
    let (entries, _) = walk(dir.path());

    let listed = paths(&entries);
    assert!(listed.contains(&".env.example"));
    assert!(listed.contains(&".github/workflows/ci.yml"));
}

// --- fuzzy_score -------------------------------------------------------------

#[test]
fn fuzzy_score_requires_the_query_as_a_subsequence() {
    assert!(fuzzy_score("src/composer.tsx", "compo").is_some());
    // The characters may be spread across the path, as long as they're in order.
    assert!(fuzzy_score("src/composer.tsx", "srccomp").is_some());
    // Same letters, wrong order.
    assert!(fuzzy_score("src/composer.tsx", "opmoc").is_none());
    // One character the path doesn't have is enough to rule it out.
    assert!(fuzzy_score("src/composer.tsx", "composerz").is_none());
}

#[test]
fn fuzzy_score_is_ascii_case_insensitive() {
    assert!(fuzzy_score("src/Composer.tsx", "composer").is_some());
    assert!(fuzzy_score("src/composer.tsx", "COMPOSER").is_some());
}

#[test]
fn fuzzy_score_prefers_a_name_match_over_a_scattered_one() {
    let name = fuzzy_score("app/composer.tsx", "composer").unwrap();
    let scattered = fuzzy_score("components/opinionated/some-render.ts", "composer").unwrap();
    assert!(
        name > scattered,
        "name match {name} should outrank scattered match {scattered}"
    );
}

#[test]
fn fuzzy_score_prefers_the_tightest_name_match() {
    // Both names start with the query; the one that is mostly the query wins.
    let tight = fuzzy_score("app/src/components/composer.tsx", "composer").unwrap();
    let padded = fuzzy_score("app/test/composer-mentions.test.ts", "composer").unwrap();
    assert!(tight > padded, "{tight} should outrank {padded}");
}

#[test]
fn fuzzy_score_prefers_segment_starts() {
    let boundary = fuzzy_score("src/hub.rs", "hub").unwrap();
    let buried = fuzzy_score("src/githubbed.rs", "hub").unwrap();
    assert!(boundary > buried, "{boundary} should outrank {buried}");
}

#[test]
fn fuzzy_score_prefers_the_shorter_of_two_equal_matches() {
    let short = fuzzy_score("src/hub.rs", "hub").unwrap();
    let long = fuzzy_score("src/hub.rs/nested/deeper/still.rs", "hub").unwrap();
    assert!(short > long);
}

#[test]
fn fuzzy_score_accepts_an_empty_query() {
    assert_eq!(fuzzy_score("anything", ""), Some(0));
}

// --- rank --------------------------------------------------------------------

fn entries(paths: &[&str]) -> Vec<FileEntry> {
    paths
        .iter()
        .map(|path| FileEntry {
            path: path.to_string(),
            is_dir: false,
        })
        .collect()
}

#[test]
fn rank_without_a_query_lists_only_the_top_level() {
    let all = entries(&["README.md", "src", "src/main.rs", "src/app/deep.rs"]);
    let (files, truncated) = rank(&all, "", 10);

    assert_eq!(
        paths(&files),
        ["README.md", "src"],
        "browsing starts at the root; the menu descends when a directory is picked"
    );
    assert!(!truncated, "nothing at the top level was left out");
}

#[test]
fn rank_browses_one_level_when_a_query_ends_at_a_directory() {
    let all = entries(&["src", "src/main.rs", "src/app", "src/app/deep.rs"]);
    let (files, truncated) = rank(&all, "src/", 10);

    assert_eq!(
        paths(&files),
        ["src/main.rs", "src/app"],
        "picking a directory shows what's in it, not everything under it"
    );
    assert!(!truncated);
}

#[test]
fn rank_still_searches_a_query_that_only_contains_a_slash() {
    let all = entries(&["src/main.rs", "src/app/main.rs"]);
    let (files, _) = rank(&all, "src/ma", 10);

    assert_eq!(
        paths(&files),
        ["src/main.rs", "src/app/main.rs"],
        "a partial name after the slash is a search again, at any depth"
    );
}

#[test]
fn rank_without_a_query_reports_a_truncated_top_level() {
    let all = entries(&["README.md", "src", "src/main.rs"]);
    let (files, truncated) = rank(&all, "", 1);

    assert_eq!(paths(&files), ["README.md"]);
    assert!(truncated, "a second top-level entry was left out");
}

#[test]
fn rank_orders_by_score_and_reports_what_it_dropped() {
    let all = entries(&[
        "app/coda_web/src/components/composer.tsx",
        "composer.md",
        "docs/company/poster.md",
        "unrelated.rs",
    ]);

    let (files, truncated) = rank(&all, "composer", 10);
    assert_eq!(
        paths(&files),
        [
            "composer.md",
            "app/coda_web/src/components/composer.tsx",
            "docs/company/poster.md"
        ],
        "verbatim name matches first, then the scattered subsequence; non-matches drop out"
    );
    assert!(!truncated);

    let (files, truncated) = rank(&all, "composer", 1);
    assert_eq!(paths(&files), ["composer.md"]);
    assert!(truncated);
}

// --- FileIndex ---------------------------------------------------------------

#[tokio::test]
async fn search_ranks_workspace_files() {
    let dir = workspace(&["src/composer.tsx", "src/transcript.tsx"]);
    let index = FileIndex::new(dir.path().to_string_lossy().into_owned());

    let matches = index.search("composer", 10).await.unwrap();

    assert_eq!(paths(&matches.files), ["src/composer.tsx"]);
    assert!(!matches.truncated);
}

/// A picker sends a request per keystroke and each is answered from its own
/// task, so a cold cache sees them arrive together. They must share one walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_share_one_walk() {
    let dir = workspace(&["src/main.rs", "src/app/deep.rs", "README.md"]);
    let index = Arc::new(FileIndex::new(dir.path().to_string_lossy().into_owned()));

    // Collected, not lazily mapped: the spawns have to all be in flight before
    // the first one is awaited, or the test serializes itself and proves nothing.
    let searches: Vec<_> = (0..16)
        .map(|n| {
            let index = Arc::clone(&index);
            // Distinct queries, so nothing but the walk itself is shared.
            tokio::spawn(async move { index.search(&"m".repeat(n % 3), 10).await })
        })
        .collect();
    for search in searches {
        search.await.unwrap().unwrap();
    }

    assert_eq!(
        index.walks.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "16 simultaneous cold-cache queries must coalesce into a single traversal"
    );
}

#[tokio::test]
async fn search_reuses_a_recent_walk_and_rewalks_once_it_expires() {
    let dir = workspace(&["src/main.rs"]);
    let root = dir.path().to_string_lossy().into_owned();
    let index = FileIndex::new(root.clone());

    assert_eq!(index.search("", 10).await.unwrap().files.len(), 1);
    fs::write(dir.path().join("added.rs"), "x").unwrap();
    assert_eq!(
        index.search("", 10).await.unwrap().files.len(),
        1,
        "a walk taken moments ago is reused rather than repeated per keystroke"
    );

    // The expiry half only means something against a *populated* cache: a fresh
    // index misses for want of any walk at all, whatever the TTL says.
    let expiring = FileIndex::with_ttl(root, Duration::ZERO);
    assert_eq!(expiring.search("", 10).await.unwrap().files.len(), 2);
    fs::write(dir.path().join("later.rs"), "x").unwrap();
    assert_eq!(
        expiring.search("", 10).await.unwrap().files.len(),
        3,
        "a walk older than the TTL is discarded rather than reused"
    );
}

#[tokio::test]
async fn search_clamps_the_requested_limit() {
    let dir = workspace(&["a.rs", "b.rs", "c.rs"]);
    let index = FileIndex::new(dir.path().to_string_lossy().into_owned());

    assert_eq!(index.search("", 0).await.unwrap().files.len(), 1);
    assert_eq!(
        index
            .search("", MAX_LIMIT + 1_000)
            .await
            .unwrap()
            .files
            .len(),
        3
    );
}
