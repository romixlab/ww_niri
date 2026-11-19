//! Sets `GIT_SHA` (short, `-dirty` from the index) and `BUILD_TIME` (the commit's own time, not the clock) for
//! `wwn --version`. Both fall back to `unknown` without git. Worktree-safe: resolves the real `.git` paths with
//! `git rev-parse` instead of assuming `.git/HEAD` is a directory, and takes `BUILD_TIME` from the commit so a
//! rebuild of an unchanged commit reruns build.rs (harmless) without changing its output.

use std::process::Command;

fn main() {
    let git_dir = git(&["rev-parse", "--git-dir"]).unwrap_or_else(|| ".git".into());
    let common_dir = git(&["rev-parse", "--git-common-dir"]).unwrap_or_else(|| git_dir.clone());
    println!("cargo:rerun-if-changed={git_dir}/HEAD");
    println!("cargo:rerun-if-changed={git_dir}/index");
    if let Ok(head) = std::fs::read_to_string(format!("{git_dir}/HEAD"))
        && let Some(r) = head.trim().strip_prefix("ref: ")
    {
        watch_if_there(&format!("{common_dir}/{r}"));
    }
    watch_if_there(&format!("{common_dir}/packed-refs"));
    println!("cargo:rustc-env=GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=BUILD_TIME={}", build_time());
}

/// `rerun-if-changed` on a path that does not exist makes cargo treat build.rs as stale on every build.
fn watch_if_there(path: &str) {
    if std::path::Path::new(path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_sha() -> String {
    let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["diff-index", "--quiet", "HEAD", "--"]) {
        None => format!("{sha}-dirty"),
        Some(_) => sha,
    }
}

/// "3 Oct 2026 18:20 UTC", without a date crate; from the commit's own time (`git log -1 --format=%ct`).
fn build_time() -> String {
    let Some(secs) = git(&["log", "-1", "--format=%ct"]).and_then(|s| s.parse::<i64>().ok()) else {
        return "unknown".into();
    };
    let (days, rest) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{day} {} {year} {:02}:{:02} UTC",
        MONTHS[(month - 1) as usize],
        rest / 3600,
        rest % 3600 / 60
    )
}
