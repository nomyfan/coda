use std::process::Command;

/// Capture the git HEAD short SHA at build time and expose it as `CODA_GIT_SHA`
/// so the binary can report the exact commit it was built from. Falls back to
/// "unknown" when git is unavailable or the source isn't a checkout (e.g. built
/// from a tarball).
fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CODA_GIT_SHA={sha}");

    // Rebuild when HEAD moves so the baked SHA never goes stale. Resolve every
    // watched file through `git rev-parse --git-path`, which is worktree-aware: in
    // a linked worktree HEAD lives in the worktree's git dir, while the branch ref
    // and packed-refs live in the common dir.
    watch(&git(&["rev-parse", "--git-path", "HEAD"]));
    watch(&git(&["rev-parse", "--git-path", "packed-refs"]));
    if let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        watch(&git(&["rev-parse", "--git-path", &reference]));
    }
}

/// Emit a `rerun-if-changed` for `path` when it resolved. A missing file is fine:
/// Cargo watches the path and rebuilds once it appears (e.g. a branch's first
/// commit creating its ref).
fn watch(path: &Option<String>) {
    if let Some(path) = path {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// Run `git <args>` and return its trimmed stdout, or `None` if git fails.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}
