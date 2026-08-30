//! Read-only reporting on the installation for `aif doctor`, plus the
//! detached daemon start and the stop wait that the `aif` binary shares.
//!
//! The report reads: it never changes anything. The one exception is
//! [`clean`], which removes worktrees for closed issues or merged pull
//! requests. It passes [`Cleanable::MergedOrClosed`] to each removal.
//! Every diagnostic command uses [`Exec`]. Tests inject
//! [`aif::exec::ScriptExec`] and do not run a diagnostic tool.

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use aif::config::{parse_owner_repo, Config, RepoConfig};
use aif::exec::Exec;
use aif::sched::{self, Limits};
use aif::sock::Client;
use aif::worktree::{Cleanable, WorktreeManager};

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
            let mut parts = word.split('.');
            let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
                continue;
            };
            let patch = parts.next();
            if parts.next().is_some() {
                continue;
            }
            let (Ok(major), Ok(minor), Ok(patch)) =
                (major.parse(), minor.parse(), patch.unwrap_or("0").parse())
            else {
                continue;
            };
            return Some(Version {
                major,
                minor,
                patch,
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

/// Remove worktrees for closed issues or merged pull requests.
///
/// The doctor prints every removal and every keep, and asks for
/// confirmation through `confirm` unless `yes` is set. A worktree whose
/// issue state cannot be determined stays, because doubt never removes
/// work. Each removal passes [`Cleanable::MergedOrClosed`] to
/// [`WorktreeManager::remove_issue`], so no other proof can reach the
/// deletion. An open issue or pull request stays. A closed pull request also
/// stays when GitHub reports that it did not merge.
///
/// Returns 0 when nothing failed and 1 when a removal failed.
pub fn clean(env: &DoctorEnv, yes: bool, confirm: &mut dyn FnMut() -> Result<bool>) -> Result<i32> {
    let config = read_config(env).context("cannot clean without a valid config")?;
    let facts = repo_facts(env.exec, &config);
    let manager = WorktreeManager::new(env.state_dir);

    let mut removals: Vec<Removal> = Vec::new();
    let mut keeps: Vec<String> = Vec::new();
    for repo in config.repos.values() {
        let worktree_dir = env.state_dir.join("worktrees").join(&repo.alias);
        let worktrees = issue_worktrees(&worktree_dir)
            .with_context(|| format!("cannot inspect {}", worktree_dir.display()))?;
        if worktrees.is_empty() {
            continue;
        }
        let Some(fact) = facts.get(&repo.alias) else {
            for (number, _) in &worktrees {
                keeps.push(format!(
                    "{} (cannot check: no repository facts exist)",
                    issue_path(env.state_dir, &repo.alias, *number).display()
                ));
            }
            continue;
        };
        let owner = match &fact.owner_repo {
            Ok(owner) => owner,
            Err(reason) => {
                for (number, _) in &worktrees {
                    keeps.push(format!(
                        "{} (cannot check: {reason})",
                        issue_path(env.state_dir, &repo.alias, *number).display()
                    ));
                }
                continue;
            }
        };
        for (number, _) in worktrees {
            match worktree_state(env.exec, owner, number) {
                Ok(state) if state.is_cleanable() => removals.push(Removal {
                    alias: repo.alias.clone(),
                    number,
                    path: issue_path(env.state_dir, &repo.alias, number),
                }),
                Ok(state) => keeps.push(format!(
                    "{} ({})",
                    issue_path(env.state_dir, &repo.alias, number).display(),
                    state.detail(number)
                )),
                Err(error) => keeps.push(format!(
                    "{} (cannot fetch the item state: {error:#})",
                    issue_path(env.state_dir, &repo.alias, number).display()
                )),
            }
        }
    }

    if removals.is_empty() {
        println!(
            "nothing to clean: every worktree belongs to an open item, an \
             unmerged pull request, or an item with unknown state"
        );
        return Ok(0);
    }
    println!(
        "The doctor removes these worktrees, because their issues are closed \
         or their pull requests are merged:"
    );
    for removal in &removals {
        println!("  {}", removal.path.display());
    }
    if !keeps.is_empty() {
        println!("The doctor keeps these worktrees:");
        for keep in &keeps {
            println!("  {keep}");
        }
    }
    if !yes && !confirm()? {
        println!("aborted; nothing was removed");
        return Ok(0);
    }

    let mut failures = 0usize;
    for removal in &removals {
        let Some(repo) = config.repos.get(&removal.alias) else {
            eprintln!(
                "cannot remove {}: repository {} is not configured",
                removal.path.display(),
                removal.alias
            );
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
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            false
        }
        Err(error) => {
            eprintln!("aif: cannot connect to {}: {error}", socket.display());
            false
        }
    }
}

/// Wait until a daemon answers on `socket`, or until `timeout` passes.
///
/// Returns true when a connect succeeded.
pub fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
    if let Some(parent) = usable_parent(socket) {
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!(
                "aif: cannot create the socket directory {}: {error}",
                parent.display()
            );
            return false;
        }
    }
    wait_for_path_event(socket, timeout, || socket_answers(socket))
}

/// Wait until the socket path is removed or the timeout passes.
///
/// A stale socket file remains present. The function returns false for it.
pub fn wait_socket_gone(socket: &Path, timeout: Duration) -> bool {
    wait_for_path_event(socket, timeout, || !socket.exists())
}

/// Return a directory that can hold `path`.
fn usable_parent(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

/// Wait for file system events until `ready` holds or the deadline passes.
fn wait_for_path_event(path: &Path, timeout: Duration, ready: impl Fn() -> bool) -> bool {
    match path_watch::wait(path, timeout, ready) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("aif: cannot watch {}: {error}", path.display());
            false
        }
    }
}

/// Linux file event support for the bounded socket waits.
#[cfg(target_os = "linux")]
mod path_watch {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{FromRawFd, RawFd};
    use std::os::raw::{c_char, c_int, c_short};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Events that can create, remove, move, or replace a socket path.
    const WATCH_MASK: u32 = 0x0000_0004
        | 0x0000_0040
        | 0x0000_0080
        | 0x0000_0100
        | 0x0000_0200
        | 0x0000_0400
        | 0x0000_0800;
    const POLLIN: c_short = 0x0001;

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: c_short,
        revents: c_short,
    }

    extern "C" {
        fn inotify_init() -> c_int;
        fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
        fn poll(fds: *mut PollFd, count: usize, timeout_ms: c_int) -> c_int;
    }

    /// Wait for relevant directory events without a sleep retry loop.
    pub(super) fn wait(
        path: &Path,
        timeout: Duration,
        ready: impl Fn() -> bool,
    ) -> io::Result<bool> {
        if ready() {
            return Ok(true);
        }
        let parent = super::usable_parent(path).unwrap_or_else(|| Path::new("."));
        let parent = CString::new(parent.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "path contains a null byte")
        })?;

        // SAFETY: inotify_init takes no arguments and returns an owned file descriptor.
        let fd = unsafe { inotify_init() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is new and owned by this function.
        let mut watcher = unsafe { File::from_raw_fd(fd) };
        // SAFETY: `parent` is a live, null-terminated C string for this call.
        if unsafe { inotify_add_watch(fd, parent.as_ptr(), WATCH_MASK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready() {
            return Ok(true);
        }

        let deadline = Instant::now() + timeout;
        let mut events = [0_u8; 4096];
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(ready());
            };
            if remaining.is_zero() {
                return Ok(ready());
            }
            let timeout_ms = remaining.as_millis().clamp(1, c_int::MAX as u128) as c_int;
            let mut descriptor = PollFd {
                fd: watcher_fd(&watcher),
                events: POLLIN,
                revents: 0,
            };
            // SAFETY: `descriptor` points to one valid PollFd for this call.
            let result = unsafe { poll(&mut descriptor, 1, timeout_ms) };
            if result == 0 {
                return Ok(ready());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptor.revents & POLLIN == 0 {
                return Err(io::Error::other("the file event descriptor failed"));
            }
            match watcher.read(&mut events) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if ready() {
                return Ok(true);
            }
        }
    }

    /// Read the raw descriptor without transferring ownership.
    fn watcher_fd(file: &File) -> RawFd {
        use std::os::fd::AsRawFd;
        file.as_raw_fd()
    }
}

/// A non-Linux Unix waits once, then checks the path again.
#[cfg(not(target_os = "linux"))]
mod path_watch {
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    /// Wait once for the timeout on a system without Linux file events.
    pub(super) fn wait(
        _path: &Path,
        timeout: Duration,
        ready: impl Fn() -> bool,
    ) -> io::Result<bool> {
        if ready() {
            return Ok(true);
        }
        std::thread::sleep(timeout);
        Ok(ready())
    }
}

/// Start the daemon detached and wait for its socket.
///
/// The start goes through
/// `systemd-run --user --collect --unit aif-daemon -- <program> run` first.
/// When `systemd-run` is missing, `spawn_detached` starts the fallback.
/// Other `systemd-run` errors propagate. The helper then waits for `socket`.
/// The wait ends when the socket answers or when `timeout` passes.
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
            bail!("systemd-run exited with status {}: {detail}", out.status,);
        }
        Err(error) if command_is_missing(&error) => {
            eprintln!("aif: cannot run systemd-run ({error:#}); falling back to a plain spawn");
            spawn_detached(daemon_program).context("the plain detached spawn failed")?;
        }
        Err(error) => return Err(error).context("cannot run systemd-run"),
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

/// Whether an external command failed because its executable is absent.
fn command_is_missing(error: &anyhow::Error) -> bool {
    error_has_io_kind(error, &[io::ErrorKind::NotFound])
}

/// Whether an error chain holds one specified operating system error.
fn error_has_io_kind(error: &anyhow::Error, kinds: &[io::ErrorKind]) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|source| kinds.contains(&source.kind()))
    })
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
        let detail = out.stderr.lines().next().unwrap_or("no stderr");
        return Check {
            label,
            status: Status::Fail,
            detail: format!(
                "{tool} --version exited with status {}: {detail}",
                out.status
            ),
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
        Err(error)
            if error_has_io_kind(
                &error,
                &[io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused],
            ) =>
        {
            Check {
                label: "daemon".to_string(),
                status: Status::Info,
                detail: format!("not running at {}", socket.display()),
            }
        }
        Err(error) => Check {
            label: "daemon".to_string(),
            status: Status::Fail,
            detail: format!("cannot check {}: {error:#}", socket.display()),
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
    /// Whether Git accepts the configured path as a working tree.
    git_repo: Result<bool, String>,
    /// The resolved `owner/name`, or the reason it could not be resolved.
    owner_repo: Result<String, String>,
}

/// Resolve every repository once, so the report and the clean share the
/// answers and the executor sees each git call only once.
fn repo_facts(exec: &dyn Exec, config: &Config) -> std::collections::BTreeMap<String, RepoFacts> {
    let mut facts = std::collections::BTreeMap::new();
    for repo in config.repos.values() {
        let git_repo = is_git_repository(exec, repo).map_err(|error| format!("{error:#}"));
        let owner_repo = match &git_repo {
            Ok(true) => resolve_owner_repo(exec, repo).map_err(|error| format!("{error:#}")),
            Ok(false) => Err("the path is not a Git working tree".to_string()),
            Err(reason) => Err(format!("the Git state is unknown: {reason}")),
        };
        facts.insert(
            repo.alias.clone(),
            RepoFacts {
                git_repo,
                owner_repo,
            },
        );
    }
    facts
}

/// Ask Git whether one configured path is a working tree.
fn is_git_repository(exec: &dyn Exec, repo: &RepoConfig) -> Result<bool> {
    let path_text = repo.path.to_string_lossy().into_owned();
    let out = exec
        .run(
            "git",
            &["-C", &path_text, "rev-parse", "--is-inside-work-tree"],
            None,
        )
        .context("cannot run git rev-parse")?;
    if out.status != 0 {
        return Ok(false);
    }
    match out.stdout.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => bail!("git rev-parse returned an invalid answer {value:?}"),
    }
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
            checks.push(Check {
                label: format!("repo {}", repo.alias),
                status: Status::Fail,
                detail: format!("{}; repository facts are unavailable", repo.path.display()),
            });
            continue;
        };
        let git_text = match &fact.git_repo {
            Ok(true) => "yes".to_string(),
            Ok(false) => "no".to_string(),
            Err(reason) => format!("unknown ({reason})"),
        };
        let (status, owner_text) = match &fact.owner_repo {
            Ok(owner) => (Status::Pass, owner.clone()),
            Err(reason) => (Status::Fail, format!("unavailable ({reason})")),
        };
        checks.push(Check {
            label: format!("repo {}", repo.alias),
            status,
            detail: format!(
                "{}; git repository: {git_text}; owner/repo: {owner_text}",
                repo.path.display()
            ),
        });
    }
    checks
}

/// The `(number, path)` pairs of the `issue-<n>` directories under `dir`.
///
/// Other entries, such as a train worktree, are not issue worktrees and are
/// skipped. A missing directory yields nothing. Other read errors propagate.
fn issue_worktrees(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", dir.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read an entry in {}", dir.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("cannot inspect {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
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
    Ok(out)
}

/// The worktree path of one issue, as [`WorktreeManager`] builds it.
fn issue_path(state_dir: &Path, alias: &str, number: u64) -> PathBuf {
    state_dir
        .join("worktrees")
        .join(alias)
        .join(format!("issue-{number}"))
}

/// The GitHub state that controls one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeState {
    /// An issue remains open.
    IssueOpen,
    /// An issue is closed.
    IssueClosed,
    /// A pull request remains open.
    PullOpen,
    /// A pull request is closed without a merge.
    PullClosed,
    /// A pull request is merged.
    PullMerged,
}

impl WorktreeState {
    /// Whether the state supplies the required removal proof.
    fn is_cleanable(self) -> bool {
        matches!(self, WorktreeState::IssueClosed | WorktreeState::PullMerged)
    }

    /// Describe the state for the report and the clean preview.
    fn detail(self, number: u64) -> String {
        match self {
            WorktreeState::IssueOpen => format!("issue {number} is open"),
            WorktreeState::IssueClosed => format!("issue {number} is closed"),
            WorktreeState::PullOpen => format!("pull request {number} is open"),
            WorktreeState::PullClosed => {
                format!("pull request {number} is closed without a merge")
            }
            WorktreeState::PullMerged => format!("pull request {number} is merged"),
        }
    }
}

/// Read the issue or pull request state for one worktree number.
fn worktree_state(exec: &dyn Exec, owner_repo: &str, number: u64) -> Result<WorktreeState> {
    let issue_url = format!("repos/{owner_repo}/issues/{number}");
    let item = gh_json(exec, &issue_url)?;
    let state = item
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub item {number} has no string state"))?;
    let is_pull = item.get("pull_request").is_some();
    match (is_pull, state) {
        (false, "open") => Ok(WorktreeState::IssueOpen),
        (false, "closed") => Ok(WorktreeState::IssueClosed),
        (true, "open") => Ok(WorktreeState::PullOpen),
        (true, "closed") => {
            let pull_url = format!("repos/{owner_repo}/pulls/{number}");
            let pull = gh_json(exec, &pull_url)?;
            match pull.get("merged").and_then(Value::as_bool) {
                Some(true) => Ok(WorktreeState::PullMerged),
                Some(false) => Ok(WorktreeState::PullClosed),
                None => bail!("GitHub pull request {number} has no boolean merged state"),
            }
        }
        (_, state) => bail!("GitHub item {number} has unknown state {state:?}"),
    }
}

/// Run one read-only GitHub API request and parse its JSON object.
fn gh_json(exec: &dyn Exec, url: &str) -> Result<Value> {
    let out = exec
        .run("gh", &["api", "-X", "GET", url], None)
        .context("gh api failed to run")?;
    if out.status != 0 {
        let detail = out.stderr.lines().next().unwrap_or("no stderr");
        bail!("gh api exited with status {}: {detail}", out.status);
    }
    serde_json::from_str(&out.stdout).context("gh api returned invalid JSON")
}

/// Report the number of worktrees and their GitHub item states.
fn worktree_checks(
    env: &DoctorEnv,
    config: &Config,
    facts: &std::collections::BTreeMap<String, RepoFacts>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut total = 0usize;
    let mut scan_errors = Vec::new();
    for repo in config.repos.values() {
        let worktree_dir = env.state_dir.join("worktrees").join(&repo.alias);
        let worktrees = match issue_worktrees(&worktree_dir) {
            Ok(worktrees) => worktrees,
            Err(error) => {
                scan_errors.push(format!("{}: {error:#}", repo.alias));
                continue;
            }
        };
        if worktrees.is_empty() {
            continue;
        }
        total += worktrees.len();
        let Some(fact) = facts.get(&repo.alias) else {
            for (number, _) in &worktrees {
                checks.push(Check {
                    label: worktree_label(&repo.alias, *number),
                    status: Status::Warn,
                    detail: "item state unknown: no repository facts exist".to_string(),
                });
            }
            continue;
        };
        let owner = match &fact.owner_repo {
            Ok(owner) => owner,
            Err(reason) => {
                for (number, _) in &worktrees {
                    checks.push(Check {
                        label: worktree_label(&repo.alias, *number),
                        status: Status::Warn,
                        detail: format!("item state unknown: {reason}"),
                    });
                }
                continue;
            }
        };
        for (number, _) in &worktrees {
            let (status, detail) = match worktree_state(env.exec, owner, *number) {
                Ok(state) => (Status::Info, state.detail(*number)),
                Err(error) => (Status::Warn, format!("item state unknown: {error:#}")),
            };
            checks.push(Check {
                label: worktree_label(&repo.alias, *number),
                status,
                detail,
            });
        }
    }
    let mut detail = if total == 0 {
        "no worktrees".to_string()
    } else {
        format!("{total} worktrees")
    };
    let status = if scan_errors.is_empty() {
        Status::Info
    } else {
        detail.push_str("; cannot read ");
        detail.push_str(&scan_errors.join("; "));
        Status::Fail
    };
    checks.push(Check {
        label: "worktrees".to_string(),
        status,
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
    use aif::exec::{CmdOut, ScriptExec};
    use aif::model::Stage;
    use aif::sock::Action;
    use std::cell::{Cell, RefCell};
    use std::fs::Permissions;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    // --- Helpers. ---

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// An executor that returns one operating system error.
    struct IoErrorExec(std::io::ErrorKind);

    impl Exec for IoErrorExec {
        fn run(&self, program: &str, _args: &[&str], _cwd: Option<&Path>) -> Result<CmdOut> {
            assert_eq!(program, "systemd-run");
            Err(std::io::Error::new(self.0, "test command error").into())
        }
    }

    /// A scripted executor that applies the fake worktree removal on disk.
    struct RemovingExec {
        script: ScriptExec,
        remove_path: PathBuf,
    }

    impl RemovingExec {
        /// Return every call that the scripted executor recorded.
        fn calls(&self) -> Vec<aif::exec::Call> {
            self.script.calls()
        }
    }

    impl Exec for RemovingExec {
        fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<CmdOut> {
            let out = self.script.run(program, args, cwd)?;
            let remove_path = self.remove_path.to_string_lossy();
            if out.status == 0
                && program == "git"
                && args.windows(2).any(|pair| pair == ["worktree", "remove"])
                && args.last().copied() == Some(remove_path.as_ref())
            {
                fs::remove_dir_all(&self.remove_path)
                    .context("the fake git removal could not remove the worktree")?;
            }
            Ok(out)
        }
    }

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

    /// One GitHub issue answer.
    fn issue_answer(number: u64, state: &str) -> CmdOut {
        CmdOut::ok(format!(r#"{{"number":{number},"state":"{state}"}}"#))
    }

    /// One GitHub pull request answer from the issues endpoint.
    fn pull_item_answer(number: u64, state: &str) -> CmdOut {
        CmdOut::ok(format!(
            r#"{{"number":{number},"state":"{state}","pull_request":{{}}}}"#
        ))
    }

    /// The exact `git` argument vector as owned strings.
    fn git_args(expected: &[&str]) -> Vec<String> {
        expected.iter().map(|s| (*s).to_string()).collect()
    }

    /// Match one exact `gh api -X GET` request.
    fn gh_get(path: &str) -> impl Fn(&aif::exec::Call) -> bool + Send + Sync + 'static {
        let expected = git_args(&["api", "-X", "GET", path]);
        move |call| call.program == "gh" && call.args == expected
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
    fn fixture_env<'a>(fx: &'a Fixture, exec: &'a dyn Exec) -> DoctorEnv<'a> {
        DoctorEnv {
            config_path: &fx.config_path,
            state_dir: &fx.state_dir,
            socket: &fx.socket,
            exec,
        }
    }

    /// Add the Git validity and origin answers of the fixture checkout.
    fn repo_answers(exec: ScriptExec, repo_path: &Path) -> ScriptExec {
        let path_text = repo_path.to_string_lossy().into_owned();
        let check_path = path_text.clone();
        exec.expect(
            move |call| {
                call.program == "git"
                    && call.args
                        == git_args(&["-C", &check_path, "rev-parse", "--is-inside-work-tree"])
            },
            CmdOut::ok("true\n"),
        )
        .expect(
            move |call| {
                call.program == "git"
                    && call.args == git_args(&["-C", &path_text, "remote", "get-url", "origin"])
            },
            CmdOut::ok("git@github.com:acme/borsuk.git\n"),
        )
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
            ("release 1.2.3.4", None),
            ("release 1.18446744073709551616.3", None),
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
    fn a_tool_version_failure_keeps_the_command_diagnostic() {
        let exec = ScriptExec::new().expect(
            |call| call.program == "gh" && call.args == ["--version"],
            CmdOut {
                status: 2,
                stdout: String::new(),
                stderr: "authentication data is invalid\n".to_string(),
            },
        );

        let check = tool_check(&exec, "gh");

        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("authentication data is invalid"),
            "detail: {}",
            check.detail
        );
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
    fn a_daemon_socket_access_error_is_a_failure() {
        let socket = PathBuf::from(format!("/tmp/{}", "x".repeat(1024)));

        let check = daemon_check(&socket);

        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("cannot check"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn a_full_report_passes_with_injected_answers() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
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
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "open"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "closed"),
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
        assert_eq!(exec.calls().len(), 8, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_worktree_directory_read_error_is_reported_as_a_failure() {
        let fx = fixture();
        let worktree_dir = fx.state_dir.join("worktrees").join("acme");
        fs::remove_dir_all(&worktree_dir).expect("the fixture worktrees must be removable");
        fs::write(&worktree_dir, "not a directory")
            .expect("the invalid worktree entry must be writable");
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
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
        let env = fixture_env(&fx, &exec);

        let checks = report(&env);

        let worktrees = checks
            .iter()
            .find(|check| check.label == "worktrees")
            .expect("the worktree check must exist");
        assert_eq!(worktrees.status, Status::Fail);
        assert!(
            worktrees.detail.contains("cannot read"),
            "detail: {}",
            worktrees.detail
        );
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
        let repo_text = repo.to_string_lossy().into_owned();
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
        let git_check_args = git_args(&["-C", &repo_text, "rev-parse", "--is-inside-work-tree"]);
        let exec = ScriptExec::new()
            .expect(
                move |call| call.program == "git" && call.args == git_check_args,
                CmdOut {
                    status: 128,
                    stdout: String::new(),
                    stderr: "not a git repository\n".to_string(),
                },
            )
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
        assert!(
            plain.detail.contains("git repository: no"),
            "detail: {}",
            plain.detail
        );
        assert!(
            plain.detail.contains("owner/repo: unavailable"),
            "detail: {}",
            plain.detail
        );
        assert_eq!(exec.calls()[0].program, "git");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    // --- The clean. ---

    #[test]
    fn clean_removes_only_the_worktree_of_the_closed_issue() {
        let fx = fixture();
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
        let script = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            )
            .expect(
                move |call| call.program == "git" && call.args == removal_argv,
                CmdOut::ok(""),
            )
            .expect(
                move |call| call.program == "git" && call.args == branch_argv,
                CmdOut::ok(""),
            );
        let exec = RemovingExec {
            script,
            remove_path: issue_path(&fx.state_dir, "acme", 7),
        };
        let env = fixture_env(&fx, &exec);
        let asked = Cell::new(false);

        let code = clean(&env, true, &mut || {
            asked.set(true);
            Ok(false)
        })
        .expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(!asked.get(), "--yes must skip the confirmation");
        let calls = exec.calls();
        assert_eq!(calls.len(), 6, "calls: {calls:?}");
        // Every call after the four lookups names the closed issue.
        // No script step accepts a call for the open issue.
        for call in &calls[4..] {
            let text = format!("{} {}", call.program, call.args.join(" "));
            assert!(text.contains("issue-7"), "unexpected removal call: {text}");
            assert!(
                !text.contains("issue-8"),
                "an open issue was touched: {text}"
            );
        }
        assert!(
            !issue_path(&fx.state_dir, "acme", 7).exists(),
            "the closed issue's worktree must be removed"
        );
        assert!(
            issue_path(&fx.state_dir, "acme", 8).exists(),
            "the open issue's worktree must survive the clean"
        );
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_asks_and_aborts_on_a_refusal() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, false, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(
            exec.calls().len(),
            4,
            "no removal may run before the confirmation"
        );
        assert!(
            issue_path(&fx.state_dir, "acme", 7).exists(),
            "the closed issue's worktree must survive an aborted clean"
        );
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_propagates_a_confirmation_error_without_a_removal() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            );
        let env = fixture_env(&fx, &exec);

        let error = clean(&env, false, &mut || {
            Err(anyhow!("cannot read the confirmation"))
        })
        .expect_err("the confirmation error must propagate");

        assert!(
            error.to_string().contains("cannot read the confirmation"),
            "error: {error:#}"
        );
        assert_eq!(exec.calls().len(), 4, "no removal may run after the error");
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_proceeds_on_confirmation() {
        let fx = fixture();
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
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
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

        let code = clean(&env, false, &mut || Ok(true)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(exec.calls().len(), 6, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_keeps_everything_when_one_item_state_is_unknown() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: "boom\n".to_string(),
                },
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert_eq!(exec.calls().len(), 4, "calls: {:?}", exec.calls());
        assert!(
            exec.calls().iter().all(|call| !call
                .args
                .windows(2)
                .any(|pair| pair == ["worktree", "remove"])),
            "no worktree removal may run while the issue state is unknown"
        );
        assert!(issue_path(&fx.state_dir, "acme", 7).exists());
        assert!(issue_path(&fx.state_dir, "acme", 8).exists());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_never_removes_an_open_pull_request_worktree() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                pull_item_answer(7, "open"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(issue_path(&fx.state_dir, "acme", 7).exists());
        assert!(issue_path(&fx.state_dir, "acme", 8).exists());
        assert!(
            exec.calls().iter().all(|call| !call
                .args
                .windows(2)
                .any(|pair| pair == ["worktree", "remove"])),
            "an open pull request worktree must not be removed"
        );
        assert_eq!(exec.calls().len(), 4, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_removes_a_merged_pull_request_worktree() {
        let fx = fixture();
        let repo_text = fx.repo_path.to_string_lossy().into_owned();
        let closed_path = issue_path(&fx.state_dir, "acme", 7);
        let closed_text = closed_path.to_string_lossy().into_owned();
        let removal_argv = git_args(&[
            "-C",
            &repo_text,
            "worktree",
            "remove",
            "--force",
            &closed_text,
        ]);
        let branch_argv = git_args(&["-C", &repo_text, "branch", "-D", "aif/acme/issue-7"]);
        let script = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                CmdOut::ok(r#"{"number":7,"state":"closed","pull_request":{}}"#),
            )
            .expect(
                gh_get("repos/acme/borsuk/pulls/7"),
                CmdOut::ok(r#"{"number":7,"merged":true}"#),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                CmdOut::ok(r#"{"number":8,"state":"open"}"#),
            )
            .expect(
                move |call| call.program == "git" && call.args == removal_argv,
                CmdOut::ok(""),
            )
            .expect(
                move |call| call.program == "git" && call.args == branch_argv,
                CmdOut::ok(""),
            );
        let exec = RemovingExec {
            script,
            remove_path: closed_path.clone(),
        };
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(!closed_path.exists());
        assert!(issue_path(&fx.state_dir, "acme", 8).exists());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_keeps_a_closed_pull_request_without_a_merge() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                pull_item_answer(7, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/pulls/7"),
                CmdOut::ok(r#"{"number":7,"merged":false}"#),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            );
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(issue_path(&fx.state_dir, "acme", 7).exists());
        assert!(issue_path(&fx.state_dir, "acme", 8).exists());
        assert_eq!(exec.calls().len(), 5, "calls: {:?}", exec.calls());
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
        let exec = IoErrorExec(std::io::ErrorKind::NotFound);
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
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_reports_a_systemd_run_execution_error_without_a_fallback() {
        let dir = temp_dir("systemd-denied");
        let socket = dir.join("daemon.sock");
        let exec = IoErrorExec(std::io::ErrorKind::PermissionDenied);
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path| {
            spawned.set(true);
            Ok(())
        };

        let error = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_millis(100),
            &mut spawn,
        )
        .expect_err("an execution error must stop the start");

        assert!(
            format!("{error:#}").contains("test command error"),
            "error: {error:#}"
        );
        assert!(
            !spawned.get(),
            "an execution error must not use the fallback"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_reports_a_systemd_run_failure_without_a_fallback() {
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
        let mut spawn = |_program: &Path| {
            spawned.set(true);
            Ok(())
        };

        let error = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            &mut spawn,
        )
        .expect_err("the failed systemd unit must stop the start");

        assert!(
            error.to_string().contains("Failed to start unit"),
            "error: {error:#}"
        );
        assert!(
            !spawned.get(),
            "a systemd unit failure must not start a second daemon"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_times_out_when_no_daemon_opens_the_socket() {
        let dir = temp_dir("timeout");
        let socket = dir.join("daemon.sock");
        let exec = IoErrorExec(std::io::ErrorKind::NotFound);
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
    fn wait_socket_gone_is_false_while_a_stale_socket_file_remains() {
        let dir = temp_dir("stale");
        let socket = dir.join("stale.sock");
        let listener = UnixListener::bind(&socket).expect("the test socket must bind");
        drop(listener);

        assert!(!wait_socket_gone(&socket, Duration::from_millis(100)));

        fs::remove_file(&socket).expect("the stale socket must be removable");
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
