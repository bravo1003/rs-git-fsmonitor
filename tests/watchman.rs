//! End-to-end tests against a real watchman. Skipped when watchman isn't installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn have_watchman() -> bool {
    Command::new("watchman").arg("version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A temporary directory that watchman stops watching and that is deleted on drop.
struct TempRepo(PathBuf);

impl TempRepo {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("rs-git-fsmonitor-{}-{}-{}", name, std::process::id(), nanos));
        fs::create_dir_all(dir.join(".git")).unwrap();
        // Resolve symlinks (e.g. /tmp on macOS) so paths match what watchman reports.
        TempRepo(dir.canonicalize().unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = Command::new("watchman").arg("watch-del").arg(&self.0).output();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs the hook and splits its NUL-terminated output.
fn hook(repo: &Path, version: &str, token: &str) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_rs-git-fsmonitor"))
        .args([version, token])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "hook failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.ends_with('\0'), "output not NUL-terminated: {:?}", text);
    text.trim_end_matches('\0').split('\0').map(String::from).collect()
}

#[test]
fn v2_round_trip() {
    if !have_watchman() {
        eprintln!("watchman not installed, skipping");
        return;
    }
    let repo = TempRepo::new("v2");
    fs::write(repo.path().join("old"), "x").unwrap();

    // First run: no token yet, so everything is dirty.
    let first = hook(repo.path(), "2", "0");
    assert!(first[0].starts_with("c:"), "{:?}", first);
    assert_eq!(&first[1..], ["/"]);

    // Nothing changed since.
    let quiet = hook(repo.path(), "2", &first[0]);
    assert_eq!(quiet.len(), 1, "{:?}", quiet);
    let clock = quiet[0].clone();

    // Additions and deletions are reported; .git is not.
    fs::write(repo.path().join("new"), "x").unwrap();
    fs::remove_file(repo.path().join("old")).unwrap();
    fs::write(repo.path().join(".git").join("index"), "x").unwrap();
    let mut changed = hook(repo.path(), "2", &clock)[1..].to_vec();
    changed.sort();
    assert_eq!(changed, ["new", "old"]);
}

#[test]
fn v2_fresh_instance_means_everything() {
    if !have_watchman() {
        return;
    }
    let repo = TempRepo::new("fresh");
    fs::write(repo.path().join("a"), "x").unwrap();
    hook(repo.path(), "2", "0");

    // A clock from some other watchman instance can't be answered incrementally.
    let out = hook(repo.path(), "2", "c:1:1:1:1");
    assert!(out[0].starts_with("c:"));
    assert_eq!(&out[1..], ["/"]);
}

#[test]
fn v2_reports_ignored_dirs() {
    if !have_watchman() {
        return;
    }
    let repo = TempRepo::new("ignored");
    fs::write(repo.path().join(".watchmanconfig"), r#"{"ignore_dirs": ["skip", "absent"]}"#).unwrap();
    fs::create_dir(repo.path().join("skip")).unwrap();

    let first = hook(repo.path(), "2", "0");
    fs::write(repo.path().join("skip").join("f"), "x").unwrap();
    let out = hook(repo.path(), "2", &first[0]);
    // watchman itself won't see skip/f, so the hook must flag the directory.
    assert_eq!(&out[1..], ["skip/"]);
}

#[test]
fn v2_ignored_dir_reports_can_be_turned_off() {
    if !have_watchman() {
        return;
    }
    let repo = TempRepo::new("noreport");
    let git = |args: &[&str]| {
        let ok = Command::new("git").args(args).current_dir(repo.path()).output().unwrap().status.success();
        assert!(ok, "git {:?} failed", args);
    };
    // The hook reads its setting with `git config`, so this needs a real repo.
    fs::remove_dir_all(repo.path().join(".git")).unwrap();
    git(&["init", "-q"]);
    git(&["config", "rs-git-fsmonitor.reportIgnoredDirs", "false"]);
    fs::write(repo.path().join(".watchmanconfig"), r#"{"ignore_dirs": ["skip"]}"#).unwrap();
    fs::create_dir(repo.path().join("skip")).unwrap();

    let first = hook(repo.path(), "2", "0");
    fs::write(repo.path().join("skip").join("f"), "x").unwrap();
    let out = hook(repo.path(), "2", &first[0]);
    assert_eq!(out.len(), 1, "{:?}", out);
}
