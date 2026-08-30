//! Read-only reporting on the installation for `aif doctor`, plus the
//! detached daemon start and the stop wait that the `aif` binary shares.
//!
//! The report reads: it never changes anything. The one exception is
//! [`clean`], which removes the worktrees of issues that are no longer open,
//! and only after it passes [`Cleanable::MergedOrClosed`] to the removal.
//! Every external command goes through the [`Exec`] indirection, so a test
//! injects a [`ScriptExec`] and never runs a real tool.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{parse_owner_repo, Config, RepoConfig};
use crate::exec::Exec;
use crate::gh::GhClient;
use crate::sched::{self, Limits};
use crate::sock::Client;
use crate::worktree::{Cleanable, WorktreeManager};

/// The oldest claude version the factory accepts.
///
/// The runner relies on machine-wide `--resume` lookup, which older versions
/// do not have. A silently older claude breaks session resume in a way that
/// looks like random task failures, so the doctor reports a failure.
pub const CLAUDE_FLOOR: Version = Version {
    major: 2,
    minor: 1,
    patch: 223,
};

/// How often the socket wait helpers retry a connect.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The tools whose versions the doctor reports.
const TOOLS: [&str; 4] = ["gh", "git", "claude", "opencode"];

/// A semantic version triple parsed out of a tool version line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// The major number.
    pub major: u64,
    /// The minor number.
    pub minor: u64,
    /// The patch number, or 0 when the tool printed only `major.minor`.
    pub patch: u64,
}

impl Version {
    /// Parse the first version-like word of `text`.
    ///
    /// A version-like word is `major.minor` or `major.minor.patch` after the
    /// surrounding punctuation is gone. Later words that merely carry numbers,
    /// such as a release date, never win.
    pub fn parse(text: &str) -> Option<Version> {
        for word in text.split_whitespace() {
            let word = word.trim_matches(|c: char| !(c.is_ascii_digit() || c == '.'));
            if !word.chars().all(|c| c.is_ascii_digit() || c == '.') {
                continue;
            }
            if word.split('.').any(str::is_empty) {
                continue;
            }
            let numbers: Vec<u64> = word.split('.').filter_map(|p| p.parse().ok()).collect();
            let (Some(major), Some(minor)) = (numbers.first().copied(), numbers.get(1).copied())
            else {
                continue;
            };
            return Some(Version {
                major,
                minor,
                patch: numbers.get(2).copied().unwrap_or(0),
            });
        }
        None
    }

    /// Whether this version is the floor or newer.
    pub fn at_least(&self, floor: &Version) -> bool {
        self >= floor
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How severe one reported check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The check holds.
    Pass,
    /// A neutral fact that needs no action.
    Info,
    /// Something looks wrong but does not stop the factory.
    Warn,
    /// Something must be fixed. Any failure makes `aif doctor` exit non-zero.
    Fail,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Status::Pass => "PASS",
            Status::Info => "INFO",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        };
        f.write_str(text)
    }
}

/// One reported check of the doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The short name of the check, for example `claude` or `repo acme`.
    pub label: String,
    /// How severe the result is.
    pub status: Status,
    /// The human-readable result.
    pub detail: String,
}

/// Everything the doctor needs, resolved by the caller.
///
/// The caller owns the paths, so a test passes temporary directories and a
/// [`ScriptExec`] and never touches the user's real installation.
pub struct DoctorEnv<'a> {
    /// The config file to report on and to clean against.
    pub config_path: &'a Path,
    /// The state directory whose `worktrees/` subtree the doctor inspects.
    pub state_dir: &'a Path,
    /// The control socket of the daemon.
    pub socket: &'a Path,
    /// The command executor. Production passes `RealExec`; tests pass a
    /// scripted executor.
    pub exec: &'a dyn Exec,
}

/// Run every read-only check and return them in report order.
///
/// The report never changes anything. A failed config does not stop the
/// report: the tools and the daemon are still checked, and everything that
/// needs the config is skipped.
pub fn report(env: &DoctorEnv) -> Vec<Check> {
    let mut checks = Vec::new();
    match read_config(env) {
        Ok(config) => {
            checks.push(Check {
                label: "config".to_string(),
                status: Status::Pass,
                detail: format!(
                    "{} parses, {} repositories",
                    env.config_path.display(),
                    config.repos.len()
                ),
            });
            let facts = repo_facts(env.exec, &config);
            checks.extend(repo_checks(&config, &facts));
            checks.extend(tool_checks(env.exec));
            checks.push(daemon_check(env.socket));
            checks.extend(scheduler_checks(&config));
            checks.extend(worktree_checks(env, &config, &facts));
        }
        Err(error) => {
            checks.push(Check {
                label: "config".to_string(),
                status: Status::Fail,
                detail: format!("{error:#}"),
            });
            checks.extend(tool_checks(env.exec));
            checks.push(daemon_check(env.socket));
        }
    }
    checks
}

/// Print the report, one check per line.
pub fn print_report(checks: &[Check]) {
    for check in checks {
        println!("{:<22} {:>4} {}", check.label, check.status, check.detail);
    }
}

/// Whether any check failed, which decides the `aif doctor` exit code.
pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.status == Status::Fail)
}

/// Remove the worktrees of issues that are no longer open.
///
/// The doctor prints every removal and every keep, and asks for
/// confirmation through `confirm` unless `yes` is set. A worktree whose
/// issue state cannot be determined stays, because doubt never removes
/// work. Each removal passes [`Cleanable::MergedOrClosed`] to
/// [`WorktreeManager::remove_issue`], so no other proof can reach the
/// deletion. An open issue is never passed, and no code path can pass it,
/// because the proof is built only for issues that the open-issue fetch
/// did not return.
///
/// Returns 0 when nothing failed and 1 when a removal failed.
pub fn clean(env: &DoctorEnv, yes: bool, confirm: &mut dyn FnMut() -> bool) -> Result<i32> {
    let config = read_config(env).context("cannot clean without a valid config")?;
    let facts = repo_facts(env.exec, &config);
    let manager = WorktreeManager::new(env.state_dir);

    let mut removals: Vec<Removal> = Vec::new();
    let mut keeps: Vec<String> = Vec::new();
    for repo in config.repos.values() {
        let worktrees = issue_worktrees(&env.state_dir.join("worktrees").join(&repo.alias));
        if worktrees.is_empty() {
            continue;
        }
        let Some(fact) = facts.get(&repo.alias) else {
            continue;
        };
        let open = match &fact.owner_repo {
            Ok(owner) => match open_issue_numbers(env.exec, owner) {
                Ok(open) => Some(open),
                Err(error) => {
                    for (number, _) in &worktrees {
                        keeps.push(format!(
                            "{} (cannot fetch the issue state: {error:#})",
                            issue_path(env.state_dir, &repo.alias, *number).display()
                        ));
                    }
                    None
                }
            },
            Err(reason) => {
                for (number, _) in &worktrees {
                    keeps.push(format!(
                        "{} (cannot check: {reason})",
                        issue_path(env.state_dir, &repo.alias, *number).display()
                    ));
                }
                None
            }
        };
        let Some(open) = open else { continue };
        for (number, _) in worktrees {
            if open.contains(&number) {
                keeps.push(format!(
                    "{} (issue {number} is open)",
                    issue_path(env.state_dir, &repo.alias, number).display()
                ));
            } else {
                removals.push(Removal {
                    alias: repo.alias.clone(),
                    number,
                    path: issue_path(env.state_dir, &repo.alias, number),
                });
            }
        }
    }

    if removals.is_empty() {
        println!(
            "nothing to clean: every worktree belongs to an open issue, or its \
             issue state is unknown"
        );
        return Ok(0);
    }
    println!("The doctor removes these worktrees, because their issues are closed:");
    for removal in &removals {
        println!("  {}", removal.path.display());
    }
    if !keeps.is_empty() {
        println!("The doctor keeps these worktrees:");
        for keep in &keeps {
            println!("  {keep}");
        }
    }
    if !yes && !confirm() {
        println!("aborted; nothing was removed");
        return Ok(0);
    }

    let mut failures = 0usize;
    for removal in &removals {
        let Some(repo) = config.repos.get(&removal.alias) else {
            failures += 1;
            continue;
        };
        match manager.remove_issue(env.exec, repo, removal.number, Cleanable::MergedOrClosed) {
            Ok(()) => println!("removed {}", removal.path.display()),
            Err(error) => {
                eprintln!("cannot remove {}: {error:#}", removal.path.display());
                failures += 1;
            }
        }
    }
    if failures == 0 {
        Ok(0)
    } else {
        Ok(1)
    }
}

/// One worktree that [`clean`] proposes to remove.
struct Removal {
    /// The repository alias.
    alias: String,
    /// The issue number of the worktree.
    number: u64,
    /// The worktree path, for the printout.
    path: PathBuf,
}

/// Whether a daemon answers on `socket` right now.
///
/// The helper connects and drops the stream again, so it never blocks longer
/// than one connect attempt.
pub fn socket_answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Wait until a daemon answers on `socket`, or until `timeout` passes.
///
/// Returns true when a connect succeeded.
pub fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if socket_answers(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(SOCKET_POLL_INTERVAL);
    }
}

/// Wait until nothing answers on `socket` any more.
///
/// A connect that fails means the listener is gone, whether the socket file
/// was removed or only turned stale. Returns true when the daemon stopped.
pub fn wait_socket_gone(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !socket_answers(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(SOCKET_POLL_INTERVAL);
    }
}

/// Start the daemon detached and wait for its socket.
///
/// The start goes through
/// `systemd-run --user --collect --unit aif-daemon -- <program> run` first.
/// When `systemd-run` is missing or fails, the plain detached spawn
/// `spawn_detached` is the fallback. Then the helper waits up to `timeout`
/// for the daemon to open `socket`. A daemon that already answers wins over
/// every spawn: the helper returns at once.
pub fn start_detached(
    socket: &Path,
    daemon_program: &Path,
    exec: &dyn Exec,
    timeout: Duration,
    spawn_detached: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<()> {
    if socket_answers(socket) {
        return Ok(());
    }
    let program_text = daemon_program.to_string_lossy().into_owned();
    let args = [
        "--user",
        "--collect",
        "--unit",
        "aif-daemon",
        "--",
        program_text.as_str(),
        "run",
    ];
    match exec.run("systemd-run", &args, None) {
        Ok(out) if out.status == 0 => {}
        Ok(out) => {
            let detail = out.stderr.lines().next().unwrap_or("no stderr");
            eprintln!(
                "aif: systemd-run exited with status {} ({detail}); falling back \
                 to a plain spawn",
                out.status
            );
            spawn_detached(daemon_program).context("the plain detached spawn failed too")?;
        }
        Err(error) => {
            eprintln!("aif: cannot run systemd-run ({error:#}); falling back to a plain spawn");
            spawn_detached(daemon_program)?;
        }
    }
    if wait_for_socket(socket, timeout) {
        Ok(())
    } else {
        bail!(
            "the daemon did not open {} within {} s; check the aif-daemon unit \
             with journalctl --user -u aif-daemon, or run `aifd run` in the \
             foreground",
            socket.display(),
            timeout.as_secs()
        );
    }
}

/// Spawn `program run` detached from the terminal.
///
/// The child gets its own process group and no standard streams, so closing
/// the terminal that started it cannot kill the daemon. The caller forgets
/// the child on purpose: the daemon is expected to outlive `aif`.
pub fn spawn_detached(program: &Path) -> Result<()> {
    let mut command = Command::new(program);
    command
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
        .spawn()
        .with_context(|| format!("cannot spawn {} detached", program.display()))?;
    Ok(())
}

/// Locate the `aifd` program to start.
///
/// The sibling of the running `aif` binary wins, so a build tree or an
/// install prefix stays self-contained. Otherwise `aifd` must be on `PATH`.
pub fn daemon_program() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("aifd");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("aifd")
}

/// Read and parse the config file, with the missing-file message of the
/// config module.
///
/// The doctor resolves repositories itself through the injected executor, so
/// it never uses `Config::load`, which always runs the real git.
fn read_config(env: &DoctorEnv) -> Result<Config> {
    if !env.config_path.exists() {
        bail!(
            "no config file at {}; create it there, or copy \
             docs/v0.5/factory.example.toml as a starting point",
            env.config_path.display()
        );
    }
    let text = fs::read_to_string(env.config_path)
        .with_context(|| format!("cannot read {}", env.config_path.display()))?;
    Config::parse(&text).with_context(|| format!("in {}", env.config_path.display()))
}

/// Check the version of one tool.
fn tool_check(exec: &dyn Exec, tool: &str) -> Check {
    let label = tool.to_string();
    let out = match exec.run(tool, &["--version"], None) {
        Ok(out) => out,
        Err(error) => {
            return Check {
                label,
                status: Status::Fail,
                detail: format!("cannot run {tool}: {error:#}"),
            };
        }
    };
    if out.status != 0 {
        return Check {
            label,
            status: Status::Fail,
            detail: format!("{tool} --version exited with status {}", out.status),
        };
    }
    let first_line = out.stdout.lines().next().unwrap_or("").trim();
    let Some(version) = Version::parse(first_line) else {
        return Check {
            label,
            status: Status::Fail,
            detail: format!("cannot parse a version from {first_line:?}"),
        };
    };
    if tool == "claude" && !version.at_least(&CLAUDE_FLOOR) {
        return Check {
            label,
            status: Status::Fail,
            detail: format!("claude {version} is older than the required floor {CLAUDE_FLOOR}"),
        };
    }
    Check {
        label,
        status: Status::Pass,
        detail: format!("{tool} {version}"),
    }
}

/// Check the versions of every tool the factory runs.
fn tool_checks(exec: &dyn Exec) -> Vec<Check> {
    TOOLS.iter().map(|tool| tool_check(exec, tool)).collect()
}

/// Check whether the daemon answers on the socket.
fn daemon_check(socket: &Path) -> Check {
    match Client::connect(socket) {
        Ok(_) => Check {
            label: "daemon".to_string(),
            status: Status::Pass,
            detail: format!("running and answering on {}", socket.display()),
        },
        Err(_) => Check {
            label: "daemon".to_string(),
            status: Status::Info,
            detail: format!("not running at {}", socket.display()),
        },
    }
}

/// Report the scheduler warnings of the configuration.
fn scheduler_checks(config: &Config) -> Vec<Check> {
    let warnings = sched::warnings(&Limits::from_config(config));
    if warnings.is_empty() {
        vec![Check {
            label: "scheduler".to_string(),
            status: Status::Pass,
            detail: "no lane or limit warnings".to_string(),
        }]
    } else {
        warnings
            .into_iter()
            .map(|warning| Check {
                label: "scheduler".to_string(),
                status: Status::Warn,
                detail: warning,
            })
            .collect()
    }
}

/// What the doctor learned about one repository on disk.
struct RepoFacts {
    /// The resolved `owner/name`, or the reason it could not be resolved.
    owner_repo: Result<String, String>,
}

/// Resolve every repository once, so the report and the clean share the
/// answers and the executor sees each git call only once.
fn repo_facts(exec: &dyn Exec, config: &Config) -> std::collections::BTreeMap<String, RepoFacts> {
    let mut facts = std::collections::BTreeMap::new();
    for repo in config.repos.values() {
        let git_repo = repo.path.join(".git").exists();
        let owner_repo = if git_repo {
            resolve_owner_repo(exec, repo).map_err(|error| format!("{error:#}"))
        } else {
            Err(format!(
                "{} holds no .git entry and is not a git repository",
                repo.path.display()
            ))
        };
        facts.insert(repo.alias.clone(), RepoFacts { owner_repo });
    }
    facts
}

/// Run `git remote get-url origin` for one repository and parse the url.
fn resolve_owner_repo(exec: &dyn Exec, repo: &RepoConfig) -> Result<String> {
    let path_text = repo.path.to_string_lossy().into_owned();
    let out = exec
        .run(
            "git",
            &["-C", &path_text, "remote", "get-url", "origin"],
            None,
        )
        .context("cannot run git")?;
    if out.status != 0 {
        bail!("git remote get-url origin failed: {}", out.stderr.trim());
    }
    parse_owner_repo(out.stdout.trim()).ok_or_else(|| {
        anyhow!(
            "cannot read owner/repo from the origin url {:?}",
            out.stdout.trim()
        )
    })
}

/// Report every repository with its resolved `owner/name` and its git state.
fn repo_checks(
    config: &Config,
    facts: &std::collections::BTreeMap<String, RepoFacts>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    for repo in config.repos.values() {
        let Some(fact) = facts.get(&repo.alias) else {
            continue;
        };
        let (status, detail) = match &fact.owner_repo {
            Ok(owner) => (
                Status::Pass,
                format!("{} resolves to {owner}", repo.path.display()),
            ),
            Err(reason) => (Status::Fail, reason.clone()),
        };
        checks.push(Check {
            label: format!("repo {}", repo.alias),
            status,
            detail,
        });
    }
    checks
}

/// The `(number, path)` pairs of the `issue-<n>` directories under `dir`.
///
/// Other entries, such as a train worktree, are not issue worktrees and are
/// skipped. A missing directory yields nothing.
fn issue_worktrees(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(number) = name.strip_prefix("issue-") else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        out.push((number, entry.path()));
    }
    out.sort_by_key(|(number, _)| *number);
    out
}

/// The worktree path of one issue, as [`WorktreeManager`] builds it.
fn issue_path(state_dir: &Path, alias: &str, number: u64) -> PathBuf {
    state_dir
        .join("worktrees")
        .join(alias)
        .join(format!("issue-{number}"))
}

/// The numbers of the issues of `owner_repo` that are open on GitHub.
///
/// The reader fetches the open issues only, so a number that is missing from
/// the answer is closed.
fn open_issue_numbers(exec: &dyn Exec, owner_repo: &str) -> Result<BTreeSet<u64>> {
    let mut client = GhClient::new(exec);
    let fetched = client.fetch_issues(owner_repo)?;
    Ok(fetched.items.keys().copied().collect())
}

/// Report the number of worktrees and the state of their issues.
fn worktree_checks(
    env: &DoctorEnv,
    config: &Config,
    facts: &std::collections::BTreeMap<String, RepoFacts>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut total = 0usize;
    for repo in config.repos.values() {
        let worktrees = issue_worktrees(&env.state_dir.join("worktrees").join(&repo.alias));
        if worktrees.is_empty() {
            continue;
        }
        total += worktrees.len();
        let Some(fact) = facts.get(&repo.alias) else {
            continue;
        };
        let open = match &fact.owner_repo {
            Ok(owner) => match open_issue_numbers(env.exec, owner) {
                Ok(open) => Some(open),
                Err(error) => {
                    for (number, _) in &worktrees {
                        checks.push(Check {
                            label: worktree_label(&repo.alias, *number),
                            status: Status::Warn,
                            detail: format!("issue state unknown: {error:#}"),
                        });
                    }
                    None
                }
            },
            Err(reason) => {
                for (number, _) in &worktrees {
                    checks.push(Check {
                        label: worktree_label(&repo.alias, *number),
                        status: Status::Warn,
                        detail: format!("issue state unknown: {reason}"),
                    });
                }
                None
            }
        };
        let Some(open) = open else { continue };
        for (number, _) in &worktrees {
            let detail = if open.contains(number) {
                format!("issue {number} is open")
            } else {
                format!("issue {number} is closed")
            };
            checks.push(Check {
                label: worktree_label(&repo.alias, *number),
                status: Status::Info,
                detail,
            });
        }
    }
    let detail = if total == 0 {
        "no worktrees".to_string()
    } else {
        format!("{total} worktrees")
    };
    checks.push(Check {
        label: "worktrees".to_string(),
        status: Status::Info,
        detail,
    });
    checks
}

/// The report label of one issue worktree.
fn worktree_label(alias: &str, number: u64) -> String {
    format!("worktree {alias} issue-{number}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmdOut, ScriptExec};
    use crate::model::Stage;
    use crate::sock::Action;
    use std::cell::{Cell, RefCell};
    use std::fs::Permissions;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    // --- Helpers. ---

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique temporary directory for one test.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-task17-{}-{}-{}",
            label,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the old temp dir must be removable");
        }
        fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }

    /// Configuration text with plain stages, optional extra lines per stage
    /// table, and the given repository tables.
    fn config_text(stage_extras: &[(&str, &str)], repos: &str) -> String {
        let mut text = String::new();
        for stage in Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nrunner = \"runner\"\n"
            ));
            for (name, extra) in stage_extras {
                if *name == stage.as_str() {
                    text.push_str(extra);
                    text.push('\n');
                }
            }
        }
        text.push_str(repos);
        text
    }

    /// One raw `gh api -i` response text.
    fn response(status_line: &str, headers: &[&str], body: &str) -> String {
        let mut text = format!("{status_line}\r\n");
        for header in headers {
            text.push_str(header);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        text.push_str(body);
        text
    }

    /// One GitHub issue object with the given number.
    fn issue_json(number: u64) -> String {
        format!(
            r#"{{"number":{number},"node_id":"node-{number}","title":"issue {number}","body":"body {number}","state":"open","labels":[]}}"#
        )
    }

    /// One page of open issues carrying only `open_number`.
    fn open_issues_page(open_number: u64) -> CmdOut {
        CmdOut::ok(response(
            "HTTP/2 200",
            &["etag: \"e1\""],
            &format!("[{}]", issue_json(open_number)),
        ))
    }

    /// The exact `git` argument vector as owned strings.
    fn git_args(expected: &[&str]) -> Vec<String> {
        expected.iter().map(|s| (*s).to_string()).collect()
    }

    /// One repository `acme` with the worktrees of issue 7 and issue 8.
    struct Fixture {
        /// The root to remove at the end of the test.
        dir: PathBuf,
        config_path: PathBuf,
        state_dir: PathBuf,
        socket: PathBuf,
        repo_path: PathBuf,
    }

    /// Build the fixture: a fake checkout, two worktree directories, and a
    /// config that names the checkout.
    fn fixture() -> Fixture {
        let dir = temp_dir("fixture");
        let repo_path = dir.join("repo");
        fs::create_dir_all(repo_path.join(".git")).expect("the fake checkout must be creatable");
        let state_dir = dir.join("state");
        for number in [7, 8] {
            fs::create_dir_all(issue_path(&state_dir, "acme", number))
                .expect("the worktree dirs must be creatable");
        }
        let config_path = dir.join("factory.toml");
        fs::write(
            &config_path,
            config_text(
                &[],
                &format!("[repo.acme]\npath = \"{}\"\n", repo_path.display()),
            ),
        )
        .expect("the config write must succeed");
        let socket = dir.join("daemon.sock");
        Fixture {
            dir,
            config_path,
            state_dir,
            socket,
            repo_path,
        }
    }

    /// The doctor environment of one fixture with its executor.
    fn fixture_env<'a>(fx: &'a Fixture, exec: &'a ScriptExec) -> DoctorEnv<'a> {
        DoctorEnv {
            config_path: &fx.config_path,
            state_dir: &fx.state_dir,
            socket: &fx.socket,
            exec,
        }
    }

    /// A git step that answers the origin query of the fixture checkout.
    fn origin_answer(
        repo_path: &Path,
    ) -> (
        impl Fn(&crate::exec::Call) -> bool + Send + Sync + 'static,
        CmdOut,
    ) {
        let path_text = repo_path.to_string_lossy().into_owned();
        let matcher = move |call: &crate::exec::Call| {
            call.program == "git"
                && call.args == git_args(&["-C", &path_text, "remote", "get-url", "origin"])
        };
        (matcher, CmdOut::ok("git@github.com:acme/borsuk.git\n"))
    }

    // --- Version parsing. ---

    #[test]
    fn version_parse_takes_the_first_version_word_and_skips_dates() {
        let cases = [
            ("2.1.251 (Claude Code)", Some((2, 1, 251))),
            ("git version 2.43.0", Some((2, 43, 0))),
            ("gh version 2.63.0 (2024-12-13)", Some((2, 63, 0))),
            ("opencode 1.18.25", Some((1, 18, 25))),
            ("v2.1.3", Some((2, 1, 3))),
            ("no version here", None),
            ("", None),
        ];
        for (text, want) in cases {
            let parsed = Version::parse(text);
            assert_eq!(
                parsed.map(|v| (v.major, v.minor, v.patch)),
                want,
                "input {text:?}"
            );
        }
    }

    #[test]
    fn the_floor_comparison_is_component_wise() {
        assert!(Version {
            major: 2,
            minor: 1,
            patch: 223
        }
        .at_least(&CLAUDE_FLOOR));
        assert!(Version {
            major: 2,
            minor: 2,
            patch: 0
        }
        .at_least(&CLAUDE_FLOOR));
        assert!(Version {
            major: 3,
            minor: 0,
            patch: 0
        }
        .at_least(&CLAUDE_FLOOR));
        assert!(!Version {
            major: 2,
            minor: 1,
            patch: 222
        }
        .at_least(&CLAUDE_FLOOR));
        assert!(!Version {
            major: 2,
            minor: 0,
            patch: 999
        }
        .at_least(&CLAUDE_FLOOR));
        assert!(!Version {
            major: 1,
            minor: 9,
            patch: 999
        }
        .at_least(&CLAUDE_FLOOR));
    }

    // --- The report. ---

    #[test]
    fn a_claude_below_the_floor_fails_and_names_the_floor() {
        let dir = temp_dir("old-claude");
        let config_path = dir.join("factory.toml");
        fs::write(&config_path, config_text(&[], "")).expect("the config write must succeed");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "gh",
                CmdOut::ok("gh version 2.63.0\n"),
            )
            .expect(
                |call| call.program == "git",
                CmdOut::ok("git version 2.43.0\n"),
            )
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("2.1.100 (Claude Code)\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            );
        let env = DoctorEnv {
            config_path: &config_path,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let claude = checks
            .iter()
            .find(|check| check.label == "claude")
            .expect("the claude check must exist");
        assert_eq!(claude.status, Status::Fail);
        assert!(
            claude.detail.contains("2.1.100"),
            "detail: {}",
            claude.detail
        );
        assert!(
            claude.detail.contains("2.1.223"),
            "the floor must be named; detail: {}",
            claude.detail
        );
        assert!(has_failures(&checks));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn claude_at_the_floor_passes() {
        let dir = temp_dir("floor-claude");
        let config_path = dir.join("factory.toml");
        fs::write(&config_path, config_text(&[], "")).expect("the config write must succeed");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "gh",
                CmdOut::ok("gh version 2.63.0\n"),
            )
            .expect(
                |call| call.program == "git",
                CmdOut::ok("git version 2.43.0\n"),
            )
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("2.1.223 (Claude Code)\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            );
        let env = DoctorEnv {
            config_path: &config_path,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let claude = checks
            .iter()
            .find(|check| check.label == "claude")
            .expect("the claude check must exist");
        assert_eq!(claude.status, Status::Pass);
        assert!(claude.detail.contains("2.1.223"));
        assert!(!has_failures(&checks));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_missing_config_still_reports_the_tools_and_the_daemon() {
        let dir = temp_dir("no-config");
        let missing = dir.join("factory.toml");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new();
        let env = DoctorEnv {
            config_path: &missing,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let config = checks
            .iter()
            .find(|check| check.label == "config")
            .expect("the config check must exist");
        assert_eq!(config.status, Status::Fail);
        assert!(
            config.detail.contains("no config file at"),
            "detail: {}",
            config.detail
        );
        assert!(config.detail.contains("factory.example.toml"));
        assert!(!checks.iter().any(|check| check.label.starts_with("repo ")));
        for tool in TOOLS {
            let check = checks
                .iter()
                .find(|check| check.label == tool)
                .unwrap_or_else(|| panic!("the {tool} check must exist"));
            assert_eq!(check.status, Status::Fail, "{tool}: {}", check.detail);
            assert!(
                check.detail.contains("cannot run"),
                "{tool}: {}",
                check.detail
            );
        }
        let daemon = checks
            .iter()
            .find(|check| check.label == "daemon")
            .expect("the daemon check must exist");
        assert_eq!(daemon.status, Status::Info);
        assert!(has_failures(&checks));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_full_report_passes_with_injected_answers() {
        let fx = fixture();
        let (origin_matcher, origin_out) = origin_answer(&fx.repo_path);
        let exec = ScriptExec::new()
            .expect(origin_matcher, origin_out)
            .expect(
                |call| call.program == "gh",
                CmdOut::ok("gh version 2.63.0\n"),
            )
            .expect(
                |call| call.program == "git",
                CmdOut::ok("git version 2.43.0\n"),
            )
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("2.1.251 (Claude Code)\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            )
            .expect(
                |call| call.program == "gh" && call.args.len() == 5,
                open_issues_page(7),
            );
        let env = fixture_env(&fx, &exec);

        let checks = report(&env);

        let config = checks
            .iter()
            .find(|check| check.label == "config")
            .expect("the config check must exist");
        assert_eq!(config.status, Status::Pass);
        let acme = checks
            .iter()
            .find(|check| check.label == "repo acme")
            .expect("the acme check must exist");
        assert_eq!(acme.status, Status::Pass);
        assert!(
            acme.detail.contains("acme/borsuk"),
            "detail: {}",
            acme.detail
        );
        for tool in TOOLS {
            let check = checks
                .iter()
                .find(|check| check.label == tool)
                .unwrap_or_else(|| panic!("the {tool} check must exist"));
            assert_eq!(check.status, Status::Pass, "{tool}: {}", check.detail);
        }
        let daemon = checks
            .iter()
            .find(|check| check.label == "daemon")
            .expect("the daemon check must exist");
        assert_eq!(daemon.status, Status::Info);
        let open = checks
            .iter()
            .find(|check| check.label == "worktree acme issue-7")
            .expect("the open worktree check must exist");
        assert_eq!(open.status, Status::Info);
        assert!(
            open.detail.contains("issue 7 is open"),
            "detail: {}",
            open.detail
        );
        let closed = checks
            .iter()
            .find(|check| check.label == "worktree acme issue-8")
            .expect("the closed worktree check must exist");
        assert!(
            closed.detail.contains("issue 8 is closed"),
            "detail: {}",
            closed.detail
        );
        let summary = checks
            .iter()
            .find(|check| check.label == "worktrees")
            .expect("the worktree summary must exist");
        assert!(
            summary.detail.contains("2 worktrees"),
            "detail: {}",
            summary.detail
        );
        assert!(!has_failures(&checks));
        assert_eq!(exec.calls().len(), 6, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_scheduler_lane_warning_is_reported() {
        let dir = temp_dir("lane-warn");
        let config_path = dir.join("factory.toml");
        fs::write(
            &config_path,
            config_text(
                &[("implement", "limit = 1")],
                "[repo.acme]\npath = \"/nonexistent/aif-task17-lane-warn\"\nlanes = { implement = 1 }\n",
            ),
        )
        .expect("the config write must succeed");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "gh",
                CmdOut::ok("gh version 2.63.0\n"),
            )
            .expect(
                |call| call.program == "git",
                CmdOut::ok("git version 2.43.0\n"),
            )
            .expect(|call| call.program == "claude", CmdOut::ok("2.1.251\n"))
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            );
        let env = DoctorEnv {
            config_path: &config_path,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let warning = checks
            .iter()
            .find(|check| check.label == "scheduler" && check.status == Status::Warn)
            .expect("the lane warning must be reported");
        assert!(
            warning.detail.contains("lane reservations cover"),
            "detail: {}",
            warning.detail
        );
        let summary = checks
            .iter()
            .find(|check| check.label == "worktrees")
            .expect("the worktree summary must exist");
        assert_eq!(summary.detail, "no worktrees");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_repository_that_is_not_a_git_checkout_fails() {
        let dir = temp_dir("plain-repo");
        let repo = dir.join("plain");
        fs::create_dir_all(&repo).expect("the plain dir must be creatable");
        let config_path = dir.join("factory.toml");
        fs::write(
            &config_path,
            config_text(
                &[],
                &format!("[repo.plain]\npath = \"{}\"\n", repo.display()),
            ),
        )
        .expect("the config write must succeed");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "gh",
                CmdOut::ok("gh version 2.63.0\n"),
            )
            .expect(
                |call| call.program == "git",
                CmdOut::ok("git version 2.43.0\n"),
            )
            .expect(|call| call.program == "claude", CmdOut::ok("2.1.251\n"))
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            );
        let env = DoctorEnv {
            config_path: &config_path,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let plain = checks
            .iter()
            .find(|check| check.label == "repo plain")
            .expect("the plain check must exist");
        assert_eq!(plain.status, Status::Fail);
        assert!(plain.detail.contains(".git"), "detail: {}", plain.detail);
        assert!(
            exec.calls()
                .iter()
                .all(|call| call.program != "git"
                    || call.args.first().map(String::as_str) != Some("-C")),
            "a non-git checkout must get no git call"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    // --- The clean. ---

    #[test]
    fn clean_removes_only_the_worktree_of_the_closed_issue() {
        let fx = fixture();
        let (origin_matcher, origin_out) = origin_answer(&fx.repo_path);
        let repo_text = fx.repo_path.to_string_lossy().into_owned();
        let closed_text = issue_path(&fx.state_dir, "acme", 7)
            .to_string_lossy()
            .into_owned();
        let removal_argv = git_args(&[
            "-C",
            &repo_text,
            "worktree",
            "remove",
            "--force",
            &closed_text,
        ]);
        let branch_argv = git_args(&["-C", &repo_text, "branch", "-D", "aif/acme/issue-7"]);
        let exec = ScriptExec::new()
            .expect(origin_matcher, origin_out)
            .expect(
                |call| call.program == "gh" && call.args.len() == 5,
                open_issues_page(8),
            )
            .expect(
                move |call| call.program == "git" && call.args == removal_argv,
                CmdOut::ok(""),
            )
            .expect(
                move |call| call.program == "git" && call.args == branch_argv,
                CmdOut::ok(""),
            );
        let env = fixture_env(&fx, &exec);
        let asked = Cell::new(false);

        let code = clean(&env, true, &mut || {
            asked.set(true);
            false
        })
        .expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(!asked.get(), "--yes must skip the confirmation");
        let calls = exec.calls();
        assert_eq!(calls.len(), 4, "calls: {calls:?}");
        // The dangerous case: every call past the two lookups names the
        // closed issue and never the open one. A call that touched the open
        // issue would have failed the scripted executor already, because no
        // step matches it.
        for call in &calls[2..] {
            let text = format!("{} {}", call.program, call.args.join(" "));
            assert!(text.contains("issue-7"), "unexpected removal call: {text}");
            assert!(
                !text.contains("issue-8"),
                "an open issue was touched: {text}"
            );
        }
        assert!(
            issue_path(&fx.state_dir, "acme", 8).exists(),
            "the open issue's worktree must survive the clean"
        );
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_asks_and_aborts_on_a_refusal() {
        let fx = fixture();
        let (origin_matcher, origin_out) = origin_answer(&fx.repo_path);
        let exec = ScriptExec::new().expect(origin_matcher, origin_out).expect(
            |call| call.program == "gh" && call.args.len() == 5,
            open_issues_page(8),
        );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, false, &mut || false).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(
            exec.calls().len(),
            2,
            "no removal may run before the confirmation"
        );
        assert!(
            issue_path(&fx.state_dir, "acme", 7).exists(),
            "the closed issue's worktree must survive an aborted clean"
        );
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_proceeds_on_confirmation() {
        let fx = fixture();
        let (origin_matcher, origin_out) = origin_answer(&fx.repo_path);
        let repo_text = fx.repo_path.to_string_lossy().into_owned();
        let closed_text = issue_path(&fx.state_dir, "acme", 7)
            .to_string_lossy()
            .into_owned();
        let removal_argv = git_args(&[
            "-C",
            &repo_text,
            "worktree",
            "remove",
            "--force",
            &closed_text,
        ]);
        let branch_argv = git_args(&["-C", &repo_text, "branch", "-D", "aif/acme/issue-7"]);
        let exec = ScriptExec::new()
            .expect(origin_matcher, origin_out)
            .expect(
                |call| call.program == "gh" && call.args.len() == 5,
                open_issues_page(8),
            )
            .expect(
                move |call| call.program == "git" && call.args == removal_argv,
                CmdOut::ok(""),
            )
            .expect(
                move |call| call.program == "git" && call.args == branch_argv,
                CmdOut::ok(""),
            );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, false, &mut || true).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(exec.calls().len(), 4, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_keeps_everything_when_the_issue_state_is_unknown() {
        let fx = fixture();
        let (origin_matcher, origin_out) = origin_answer(&fx.repo_path);
        let exec = ScriptExec::new().expect(origin_matcher, origin_out).expect(
            |call| call.program == "gh" && call.args.len() == 5,
            CmdOut {
                status: 1,
                stdout: String::new(),
                stderr: "boom\n".to_string(),
            },
        );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || false).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(exec.calls().len(), 2, "calls: {:?}", exec.calls());
        assert!(
            exec.calls()
                .iter()
                .all(|call| call.program != "git" || call.args.len() != 4),
            "no worktree removal may run while the issue state is unknown"
        );
        assert!(issue_path(&fx.state_dir, "acme", 7).exists());
        assert!(issue_path(&fx.state_dir, "acme", 8).exists());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    // --- The detached start and the socket waits. ---

    /// Bind a fake daemon socket on `socket` after a short delay and hold it
    /// long enough for one wait to observe it.
    fn fake_daemon(socket: PathBuf, delay_ms: u64) {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(delay_ms));
            let listener =
                UnixListener::bind(&socket).expect("the fake daemon must bind the socket");
            thread::sleep(Duration::from_millis(400));
            drop(listener);
        });
    }

    #[test]
    fn start_detached_runs_systemd_run_with_the_exact_argv_and_skips_the_fallback() {
        let dir = temp_dir("systemd");
        let socket = dir.join("daemon.sock");
        fake_daemon(socket.clone(), 50);
        let exec = ScriptExec::new().expect(|call| call.program == "systemd-run", CmdOut::ok(""));
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path| {
            spawned.set(true);
            Ok(())
        };

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            &mut spawn,
        )
        .expect("the start must succeed");

        let calls = exec.calls();
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(
            calls[0].argv(),
            [
                "--user",
                "--collect",
                "--unit",
                "aif-daemon",
                "--",
                "/opt/aif/bin/aifd",
                "run"
            ]
        );
        assert!(
            !spawned.get(),
            "the fallback must not run when systemd-run works"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_falls_back_when_systemd_run_is_missing() {
        let dir = temp_dir("no-systemd");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new();
        let spawned: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let spawn_socket = socket.clone();
        let mut spawn = |program: &Path| {
            spawned.borrow_mut().push(program.to_path_buf());
            fake_daemon(spawn_socket.clone(), 30);
            Ok(())
        };

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            &mut spawn,
        )
        .expect("the start must succeed");

        let spawned = spawned.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].as_os_str(), "/opt/aif/bin/aifd");
        // The executor records even a rejected call, so the single record is
        // the failed systemd-run attempt itself.
        let calls = exec.calls();
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].program, "systemd-run");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_falls_back_when_systemd_run_fails() {
        let dir = temp_dir("systemd-fails");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new().expect(
            |call| call.program == "systemd-run",
            CmdOut {
                status: 1,
                stdout: String::new(),
                stderr: "Failed to start unit\n".to_string(),
            },
        );
        let spawned = Cell::new(false);
        let spawn_socket = socket.clone();
        let mut spawn = |_program: &Path| {
            spawned.set(true);
            fake_daemon(spawn_socket.clone(), 30);
            Ok(())
        };

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            &mut spawn,
        )
        .expect("the start must succeed");

        assert!(
            spawned.get(),
            "the fallback must run when systemd-run fails"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_times_out_when_no_daemon_opens_the_socket() {
        let dir = temp_dir("timeout");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new();
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path| {
            spawned.set(true);
            Ok(())
        };

        let result = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_millis(150),
            &mut spawn,
        );

        let error = result.expect_err("the start must time out");
        assert!(error.to_string().contains("did not open"), "error: {error}");
        assert!(
            spawned.get(),
            "the spawn must have happened before the timeout"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn spawn_detached_runs_a_real_child() {
        let dir = temp_dir("real-spawn");
        let marker = dir.join("marker");
        let script = dir.join("fake-aifd");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nsleep 1\n", marker.display()),
        )
        .expect("the script write must succeed");
        fs::set_permissions(&script, Permissions::from_mode(0o755))
            .expect("the script must be chmodable");

        spawn_detached(&script).expect("the detached spawn must succeed");

        let mut found = false;
        for _ in 0..100 {
            if marker.exists() {
                found = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(found, "the detached child never ran");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn stop_round_trip_waits_for_the_socket_to_disappear() {
        let dir = temp_dir("stop");
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("the fake daemon must bind the socket");
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the stop client must connect");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("the stop line must be readable");
            assert!(
                line.contains("\"action\":\"stop\""),
                "the client sent: {line}"
            );
            drop(listener);
            fs::remove_file(&server_socket).expect("the socket file must be removable");
        });

        let mut client = Client::connect(&socket).expect("the client must connect");
        client.send(&Action::Stop).expect("the send must succeed");

        assert!(wait_socket_gone(&socket, Duration::from_secs(5)));
        server.join().expect("the server thread must not panic");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn wait_socket_gone_is_true_without_any_socket() {
        let dir = temp_dir("gone");
        let socket = dir.join("absent.sock");
        assert!(wait_socket_gone(&socket, Duration::from_millis(200)));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn wait_for_socket_waits_for_a_late_listener() {
        let dir = temp_dir("late");
        let socket = dir.join("daemon.sock");
        fake_daemon(socket.clone(), 100);

        assert!(wait_for_socket(&socket, Duration::from_secs(5)));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn daemon_program_ends_with_aifd() {
        let program = daemon_program();
        assert_eq!(
            program.file_name(),
            Some(std::ffi::OsStr::new("aifd")),
            "program: {}",
            program.display()
        );
    }
}
