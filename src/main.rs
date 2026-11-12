use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};

/// Where watchman looks for its global config if `WATCHMAN_CONFIG_FILE` isn't set.
/// The real location is a compile-time option of watchman, so we try the usual ones.
const GLOBAL_CONFIG_CANDIDATES: &[&str] = &[
    "/etc/watchman.json",
    "/usr/local/etc/watchman.json",
    "/opt/homebrew/etc/watchman.json",
];

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        bail!("Expected two arguments, got {:?}", &args[1..]);
    }
    let hook_version = args[1]
        .parse::<isize>()
        .context("First arg wasn't a version number")?;

    // Git runs the hook from the root of the working tree.
    let worktree = env::current_dir().context("Couldn't get working directory")?;

    // Build the whole response in memory and only write it once watchman has
    // answered successfully. If anything fails we exit non-zero without having
    // printed a partial answer, and Git falls back to scanning the worktree.
    let output = match hook_version {
        1 => query_watchman_v1(&worktree, &args[2])?,
        2 => query_watchman_v2(&worktree, &args[2])?,
        _ => bail!(
            "Unsupported fsmonitor-watchman hook version: {}",
            hook_version
        ),
    };

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    out.write_all(&output)?;
    out.flush()?;
    Ok(())
}

/// V2 of the API takes a clock token from `watchman clock`,
/// and asks watchman what files have changed since the provided time.
fn query_watchman_v2(worktree: &Path, last_update_token: &str) -> Result<Vec<u8>> {
    // Gracefully upgrade repo fsmonitor from v1 timestamp to v2 opaque clock token.
    // Modern watchman only accepts clock tokens, not numeric timestamps, so for
    // anything else (like "0" on the first run) get a fresh clock and tell Git
    // that everything is dirty.
    if !last_update_token.starts_with('c') {
        let clock = watched_clock(worktree)?;
        return Ok(format_v2(&clock, &[], true));
    }

    // From the Perl that ships with Git:
    //
    // In the query expression below we're asking for names of files that
    // changed since $last_update_token but not from the .git folder.
    // (We also drop the .git directory entry itself, which changes on every
    // query because watchman puts its sync cookies in it.)
    //
    // To accomplish this, we're using the "since" generator to use the
    // recency index to select candidate nodes and "fields" to limit the
    // output to file names only. Then we're using the "expression" term to
    // further constrain the results.
    //
    // "empty_on_fresh_instance" stops watchman from listing every file in the
    // repo when it can't answer relative to our token; we report "/" instead.
    let response = watchman_query(&json!(
        [
            "query",
            worktree,
            {
                "since": last_update_token,
                "fields": ["name"],
                "empty_on_fresh_instance": true,
                "expression": [
                    "not", [
                        "anyof",
                        ["dirname", ".git"],
                        ["name", ".git", "wholename"]
                    ]
                ]
            }
        ]
    ))?;

    if let Some(err) = response["error"].as_str() {
        ensure!(
            is_not_watched_error(err),
            "Watchman failed for an unexpected reason {}",
            err
        );

        // Start a watch, then get the clock ID, and return the fast
        // "everything is dirty" indication to Git. The next query will then
        // be relative to a clock from _after_ the watch started.
        let clock = watched_clock(worktree)?;
        return Ok(format_v2(&clock, &[], true));
    }

    let new_clock = response["clock"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing new clock ID in watchman response {:#}", response))?;

    // A fresh instance means watchman can't tell us what changed since the
    // token: it was restarted, the watch was deleted and re-added, or the
    // token belongs to another root. Files deleted in the meantime would be
    // missing from any list, so the only safe answer is "everything".
    if response["is_fresh_instance"].as_bool().unwrap_or(false) {
        return Ok(format_v2(new_clock, &[], true));
    }

    let mut files = response_files(&response)?;
    files.extend(ignored_dirs(worktree));
    Ok(format_v2(new_clock, &files, false))
}

/// V1 of the API takes a time of elapsed nanoseconds since the POSIX epoch,
/// and asks watchman what files have changed since the provided time.
fn query_watchman_v1(worktree: &Path, time: &str) -> Result<Vec<u8>> {
    let time_nanoseconds: u64 = time
        .parse::<u64>()
        .context("Second arg wasn't an integer")?;
    let time_seconds = time_nanoseconds / 1_000_000_000;

    // From the Perl that ships with Git:
    //
    // In the query expression below we're asking for names of files that
    // changed since $time but were not transient (ie created after
    // $time but no longer exist).
    //
    // To accomplish this, we're using the "since" generator to use the
    // recency index to select candidate nodes and "fields" to limit the
    // output to file names only. Then we're using the "expression" term to
    // further constrain the results.
    //
    // The category of transient files that we want to ignore will have a
    // creation clock (cclock) newer than $time_t value and will also not
    // currently exist.
    let response = watchman_query(&json!(
        [
            "query",
            worktree,
            {
                "since": time_seconds,
                "fields": ["name"],
                "empty_on_fresh_instance": true,
                "expression": [
                    "not", [
                        "allof",[
                            "since",
                            time_seconds,
                            "cclock"
                        ],
                        [
                            "not",
                            "exists"
                        ]
                    ]
                ]
            }
        ]
    ))?;

    if let Some(err) = response["error"].as_str() {
        ensure!(
            is_not_watched_error(err),
            "Watchman failed for an unexpected reason {}",
            err
        );
        add_watch(worktree)?;
        return Ok(format_v1(&[], true));
    }

    if response["is_fresh_instance"].as_bool().unwrap_or(false) {
        return Ok(format_v1(&[], true));
    }

    let mut files = response_files(&response)?;
    files.extend(ignored_dirs(worktree));
    Ok(format_v1(&files, false))
}

/// The V2 answer: the new clock token, then every changed path, all
/// NUL-terminated. A lone "/" tells Git to treat everything as changed.
fn format_v2(clock: &str, files: &[String], everything: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(clock.as_bytes());
    out.push(0);
    out.extend(format_v1(files, everything));
    out
}

/// The V1 answer: every changed path, NUL-terminated.
fn format_v1(files: &[String], everything: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if everything {
        out.extend_from_slice(b"/\0");
        return out;
    }
    for file in files {
        out.extend_from_slice(file.as_bytes());
        out.push(0);
    }
    out
}

fn response_files(response: &Value) -> Result<Vec<String>> {
    let files = response["files"]
        .as_array()
        .ok_or_else(|| anyhow!("Missing file data in watchman response {:#}", response))?;
    Ok(files
        .iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect())
}

fn is_not_watched_error(err: &str) -> bool {
    err.contains("unable to resolve root") || err.contains("is not watched")
}

/// Directories inside the worktree that watchman is configured to ignore
/// (`ignore_dirs`), each with a trailing slash.
///
/// Watchman never reports changes under these, so without help Git would trust
/// its cached state for them forever and miss edits there. Reporting them on
/// every query makes Git re-check them itself: slower for those directories,
/// but correct.
fn ignored_dirs(worktree: &Path) -> Vec<String> {
    let dirs = configured_ignore_dirs(worktree);
    dirs.iter()
        .filter_map(|d| {
            let d = d.trim_matches('/');
            if d.is_empty() || d == ".git" || !worktree.join(d).is_dir() {
                return None;
            }
            Some(format!("{}/", d))
        })
        .collect()
}

/// `ignore_dirs` from the root's `.watchmanconfig`, or, if it doesn't set
/// one, from watchman's global config. This mirrors watchman, where a per-root
/// setting replaces the global one.
fn configured_ignore_dirs(worktree: &Path) -> Vec<String> {
    let global = env::var_os("WATCHMAN_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            GLOBAL_CONFIG_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        });

    let sources = [Some(worktree.join(".watchmanconfig")), global];
    for path in sources.iter().flatten() {
        if let Some(dirs) = read_json(path).and_then(|v| string_array(&v["ignore_dirs"])) {
            return dirs;
        }
    }
    Vec::new()
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

/// Returns a clock for the worktree, starting a watch first if needed.
fn watched_clock(worktree: &Path) -> Result<String> {
    match watchman_clock(worktree)? {
        Ok(clock) => Ok(clock),
        Err(err) if is_not_watched_error(&err) => {
            add_watch(worktree)?;
            watchman_clock(worktree)?.map_err(|e| anyhow!("`watchman clock` returned an error: {}", e))
        }
        Err(err) => bail!("`watchman clock` returned an error: {}", err),
    }
}

/// Calls `watchman clock` on the worktree. The inner result carries the error
/// text watchman sent back, so callers can decide whether to start a watch.
fn watchman_clock(worktree: &Path) -> Result<std::result::Result<String, String>> {
    let response = run_watchman(&[OsStr::new("clock"), worktree.as_os_str()], None)?;

    if let Some(error) = response["error"].as_str() {
        Ok(Err(error.to_string()))
    } else if let Some(clock_id) = response["clock"].as_str() {
        Ok(Ok(clock_id.to_string()))
    } else {
        bail!(
            "`watchman clock` didn't provide a clock ID in response {:#}",
            response
        )
    }
}

fn watchman_query(query: &Value) -> Result<Value> {
    run_watchman(
        &[OsStr::new("-j"), OsStr::new("--no-pretty")],
        Some(query.to_string().as_bytes()),
    )
}

fn add_watch(worktree: &Path) -> Result<()> {
    eprintln!("Adding {} to Watchman's watch list", worktree.display());

    let response = run_watchman(&[OsStr::new("watch"), worktree.as_os_str()], None)?;
    if let Some(error) = response["error"].as_str() {
        bail!("`watchman watch` failed: {}", error);
    }
    Ok(())
}

/// Runs the watchman CLI and parses its JSON reply.
fn run_watchman(args: &[&OsStr], input: Option<&[u8]>) -> Result<Value> {
    let mut child = Command::new("watchman")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Couldn't start watchman (is it installed and on PATH?)")?;

    {
        // Dropping stdin at the end of this block closes it, so watchman
        // sees EOF even when we have nothing to send.
        let mut stdin = child
            .stdin
            .take()
            .expect("child Watchman process's stdin isn't piped");
        if let Some(input) = input {
            stdin.write_all(input)?;
        }
    }

    let output = child
        .wait_with_output()
        .context("Failed to wait on watchman")?;

    // Watchman reports most failures as JSON with an "error" field, which the
    // callers handle, so only an empty or unparseable reply is fatal here.
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "Couldn't parse watchman's reply (exit status {}): {:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn v2_lists_changed_files() {
        let out = format_v2("c:1:2:3:4", &strings(&["a", "dir/b"]), false);
        assert_eq!(out, b"c:1:2:3:4\0a\0dir/b\0");
    }

    #[test]
    fn v2_nothing_changed_is_just_the_clock() {
        assert_eq!(format_v2("c:1:2:3:4", &[], false), b"c:1:2:3:4\0");
    }

    #[test]
    fn v2_everything_ignores_file_list() {
        let out = format_v2("c:1:2:3:4", &strings(&["a"]), true);
        assert_eq!(out, b"c:1:2:3:4\0/\0");
    }

    #[test]
    fn v1_everything() {
        assert_eq!(format_v1(&[], true), b"/\0");
    }

    #[test]
    fn not_watched_errors_are_recognised() {
        assert!(is_not_watched_error(
            "watchman::RootResolveError: failed to resolve root: unable to resolve root /x: \
             failed to resolve root: directory /x is not watched"
        ));
        assert!(!is_not_watched_error("inotify-add-watch(/x) -> No space left on device"));
    }

    #[test]
    fn missing_files_field_is_an_error() {
        assert!(response_files(&json!({"clock": "c:1"})).is_err());
    }
}
