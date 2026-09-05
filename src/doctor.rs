//! Read-only reporting on the installation for `aif doctor`, plus the
//! detached daemon start and the stop wait that the `aif` binary shares.
//!
//! The report reads: it never changes anything. The one exception is
//! [`clean`], which removes worktrees for closed issues or merged pull
//! requests. It passes [`Cleanable::MergedOrClosed`] to each removal.
//! Every diagnostic command uses [`Exec`]. Tests inject
//! [`aif::exec::ScriptExec`] and do not run a diagnostic tool.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use aif::config::{parse_owner_repo, Config, ExecutionRole, Harness, RepoConfig};
use aif::exec::Exec;
use aif::sched::{self, Limits};
use aif::sock::{Client, PauseScope, PausedView, Push};
use aif::worktree::{Cleanable, WorktreeKind, WorktreeManager, WORKTREE_KINDS};

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

/// The tools that every factory installation needs.
const CORE_TOOLS: [&str; 2] = ["gh", "git"];

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

/// The leading run of digits and dots of `word`.
///
/// A git-describe word such as `2.74.0-19-gea8fc856e` keeps its head
/// `2.74.0`. The caller trims surrounding punctuation first, so a word
/// such as `v2.1.3` also starts inside its run.
fn leading_digits_and_dots(word: &str) -> &str {
    match word.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(end) => &word[..end],
        None => word,
    }
}

impl Version {
    /// Parse the first version-like word of `text`.
    ///
    /// A version-like word is `major.minor` or `major.minor.patch` after the
    /// surrounding punctuation is gone. Only the leading digits-and-dots run
    /// of a word counts, so `2.74.0-19-gea8fc856e` yields `2.74.0`. A date
    /// word such as `2025-06-09` keeps only `2025`, has no minor component,
    /// and is skipped. The first version-like word wins.
    pub fn parse(text: &str) -> Option<Version> {
        for word in text.split_whitespace() {
            let word = word.trim_matches(|c: char| !(c.is_ascii_digit() || c == '.'));
            let word = leading_digits_and_dots(word);
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

/// Report the version of the `aif` package itself.
fn aif_version_check() -> Check {
    Check {
        label: "aif".to_string(),
        status: Status::Pass,
        detail: format!("aif {}", env!("CARGO_PKG_VERSION")),
    }
}

/// Run every read-only check and return them in report order.
///
/// The report never changes anything. A failed config does not stop the
/// report: the tools and the daemon are still checked, and everything that
/// needs the config is skipped.
pub fn report(env: &DoctorEnv) -> Vec<Check> {
    let mut checks = vec![aif_version_check()];
    match read_config(env) {
        Ok(config) => {
            checks.push(Check {
                label: "config".to_string(),
                status: Status::Pass,
                detail: format!(
                    "{} parses, schema {}, {} repositories",
                    env.config_path.display(),
                    config.schema_version,
                    config.repos.len()
                ),
            });
            let facts = repo_facts(env.exec, &config);
            checks.extend(repo_checks(&config, &facts));
            checks.extend(tool_checks(env.exec, Some(&config)));
            checks.push(gh_auth_check(env.exec));
            checks.extend(usage_curl_check(env.exec, &config));
            checks.extend(daemon_checks(env.socket));
            checks.extend(scheduler_checks(&config));
            checks.extend(permission_checks(&config));
            checks.extend(worktree_checks(env, &config, &facts));
        }
        Err(error) => {
            checks.push(Check {
                label: "config".to_string(),
                status: Status::Fail,
                detail: format!("{error:#}"),
            });
            checks.extend(tool_checks(env.exec, None));
            checks.push(gh_auth_check(env.exec));
            checks.extend(daemon_checks(env.socket));
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
        let worktrees = item_worktrees(&worktree_dir)
            .with_context(|| format!("cannot inspect {}", worktree_dir.display()))?;
        if worktrees.is_empty() {
            continue;
        }
        let Some(fact) = facts.get(&repo.alias) else {
            for (kind, number, _) in &worktrees {
                keeps.push(format!(
                    "{} (cannot check: no repository facts exist)",
                    item_path(env.state_dir, &repo.alias, *kind, *number).display()
                ));
            }
            continue;
        };
        let owner = match &fact.owner_repo {
            Ok(owner) => owner,
            Err(reason) => {
                for (kind, number, _) in &worktrees {
                    keeps.push(format!(
                        "{} (cannot check: {reason})",
                        item_path(env.state_dir, &repo.alias, *kind, *number).display()
                    ));
                }
                continue;
            }
        };
        for (kind, number, _) in worktrees {
            match worktree_state(env.exec, owner, number) {
                Ok(state) if state.is_cleanable(kind) => removals.push(Removal {
                    alias: repo.alias.clone(),
                    kind,
                    number,
                    path: item_path(env.state_dir, &repo.alias, kind, number),
                }),
                Ok(state) => keeps.push(format!(
                    "{} ({})",
                    item_path(env.state_dir, &repo.alias, kind, number).display(),
                    state.detail(number)
                )),
                Err(error) => keeps.push(format!(
                    "{} (cannot fetch the item state: {error:#})",
                    item_path(env.state_dir, &repo.alias, kind, number).display()
                )),
            }
        }
    }

    if removals.is_empty() {
        println!(
            "nothing to clean: every worktree belongs to an open item, an \
             unmerged PR, or an item with unknown state"
        );
        return Ok(0);
    }
    println!(
        "The doctor removes these worktrees, because their tickets are closed \
         or their PRs are merged:"
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
        let removal_result = match removal.kind {
            WorktreeKind::Issue => {
                manager.remove_issue(env.exec, repo, removal.number, Cleanable::MergedOrClosed)
            }
            WorktreeKind::Pr => {
                manager.remove_pr(env.exec, repo, removal.number, Cleanable::MergedOrClosed)
            }
        };
        match removal_result {
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
    /// The worktree kind: ticket or PR.
    kind: WorktreeKind,
    /// The item number of the worktree.
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

/// The result of one detached daemon start request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// This call started a daemon and observed its socket.
    Started,
    /// A daemon answered before this call started one.
    AlreadyRunning,
}

/// How long the start path waits after it resets a stale daemon unit.
const UNIT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Start the daemon detached and wait for its socket.
///
/// The start goes through
/// `systemd-run --user --collect --unit aif-daemon -- <program> run` first.
/// A true `paused` appends `--paused`, so the daemon starts with the whole
/// factory paused. When `systemd-run` is missing, `spawn_detached` starts
/// the fallback. Other `systemd-run` errors propagate. A failure that names
/// an existing unit gets one `systemctl --user reset-failed` and one retry,
/// because a daemon that just stopped leaves its unit loaded for a moment.
/// The helper then waits for `socket`. The wait ends when the socket answers
/// or when `timeout` passes. The result identifies an existing daemon
/// without hiding it.
pub fn start_detached(
    socket: &Path,
    daemon_program: &Path,
    exec: &dyn Exec,
    timeout: Duration,
    paused: bool,
    spawn_detached: &mut dyn FnMut(&Path, bool) -> Result<()>,
) -> Result<StartOutcome> {
    if socket_answers(socket) {
        return Ok(StartOutcome::AlreadyRunning);
    }
    let program_text = daemon_program.to_string_lossy().into_owned();
    let mut retried = false;
    loop {
        let mut args = vec![
            "--user",
            "--collect",
            "--unit",
            "aif-daemon",
            // The daemon stops its own agent children in the right order, so
            // systemd sends SIGTERM to the daemon only (`mixed`) and waits
            // before any SIGKILL. A survivor still dies after the timeout.
            "--property=KillMode=mixed",
            "--property=TimeoutStopSec=45",
            "--",
            program_text.as_str(),
            "run",
        ];
        if paused {
            args.push("--paused");
        }
        match exec.run("systemd-run", &args, None) {
            Ok(out) if out.status == 0 => break,
            Ok(out) if !retried && stderr_names_existing_unit(&out.stderr) => {
                retried = true;
                eprintln!("aif: the aif-daemon unit is still loaded; reset it and start again");
                reset_daemon_unit(exec);
                std::thread::sleep(UNIT_RETRY_DELAY);
            }
            Ok(out) => {
                let detail = out.stderr.lines().next().unwrap_or("no stderr");
                bail!("systemd-run exited with status {}: {detail}", out.status,);
            }
            Err(error) if command_is_missing(&error) => {
                eprintln!("aif: cannot run systemd-run ({error:#}); falling back to a plain spawn");
                spawn_detached(daemon_program, paused)
                    .context("the plain detached spawn failed")?;
                break;
            }
            Err(error) => return Err(error).context("cannot run systemd-run"),
        }
    }
    if wait_for_socket(socket, timeout) {
        Ok(StartOutcome::Started)
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

/// Whether `systemd-run` failed because the unit name is still loaded.
///
/// A daemon that just exited leaves `aif-daemon.service` loaded for a short
/// time even with `--collect`, and `systemd-run` then rejects the name.
fn stderr_names_existing_unit(stderr: &str) -> bool {
    stderr.contains("already exists")
}

/// Clear the failed state of the transient daemon unit.
///
/// A failure stays ignored: the unit may not exist and systemd may be
/// absent on a system that uses the plain spawn fallback.
fn reset_daemon_unit(exec: &dyn Exec) {
    let _ = exec.run("systemctl", &["--user", "reset-failed", "aif-daemon"], None);
}

/// Unload the transient daemon unit after a stop.
///
/// `aif stop` talks to the daemon over the socket, so systemd can still be
/// finishing the unit exit when the next `aif` starts. `systemctl --user
/// stop` waits for that exit and `reset-failed` clears a failed unit.
/// Every failure stays ignored: the unit may not exist and systemd may be
/// absent on a system that uses the plain spawn fallback.
pub fn cleanup_daemon_unit(exec: &dyn Exec) {
    let _ = exec.run("systemctl", &["--user", "stop", "aif-daemon"], None);
    reset_daemon_unit(exec);
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
/// A true `paused` appends `--paused`, so the fallback spawn starts the
/// daemon paused like the `systemd-run` path does. The child gets its own
/// process group and no standard streams, so closing the terminal that
/// started it cannot kill the daemon. The caller forgets the child on
/// purpose: the daemon is expected to outlive `aif`.
pub fn spawn_detached(program: &Path, paused: bool) -> Result<()> {
    let mut command = Command::new(program);
    command.arg("run");
    if paused {
        command.arg("--paused");
    }
    command
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
            "no config file at {}; run ./install.sh or copy \
             docs/v0.5/factory.example.toml as a starting point",
            env.config_path.display()
        );
    }
    let text = fs::read_to_string(env.config_path)
        .with_context(|| format!("cannot read {}", env.config_path.display()))?;
    Config::parse(&text).with_context(|| format!("in {}", env.config_path.display()))
}

/// Check the version of one tool.
fn tool_check(exec: &dyn Exec, tool: &str, requires_claude_floor: bool) -> Check {
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
    if requires_claude_floor && !version.at_least(&CLAUDE_FLOOR) {
        return Check {
            label,
            status: Status::Fail,
            detail: format!("{tool} {version} is older than the required floor {CLAUDE_FLOOR}"),
        };
    }
    Check {
        label,
        status: Status::Pass,
        detail: format!("{tool} {version}"),
    }
}

/// Check the core tools and each configured harness program once.
fn tool_checks(exec: &dyn Exec, config: Option<&Config>) -> Vec<Check> {
    let mut programs = BTreeMap::<String, bool>::new();
    if let Some(config) = config {
        for settings in config.roles.values() {
            programs
                .entry(settings.program.clone())
                .and_modify(|claude| *claude |= settings.harness == Harness::Claude)
                .or_insert(settings.harness == Harness::Claude);
        }
        for alias in config.repos.keys() {
            for role in ExecutionRole::ALL {
                let Ok(resolved) = config.resolved_role(Some(alias), role.table_name()) else {
                    continue;
                };
                let settings = resolved.settings;
                programs
                    .entry(settings.program)
                    .and_modify(|claude| *claude |= settings.harness == Harness::Claude)
                    .or_insert(settings.harness == Harness::Claude);
            }
        }
    }
    for tool in CORE_TOOLS {
        programs.entry(tool.to_string()).or_insert(false);
    }
    let mut checks = Vec::new();
    for tool in CORE_TOOLS {
        let claude = programs
            .remove(tool)
            .expect("each core program was inserted");
        checks.push(tool_check(exec, tool, claude));
    }
    checks.extend(
        programs
            .into_iter()
            .map(|(program, claude)| tool_check(exec, &program, claude)),
    );
    checks
}

/// Check that `curl` is present when the usage probes need it.
///
/// The usage probes read the provider endpoints through `curl`, so a
/// missing `curl` leaves the USAGE band of the pipeline view empty. The
/// check runs only when the `[usage]` table is enabled and the config
/// derives at least one billed identity. A missing `curl` never stops the
/// factory, so the check warns instead of failing.
fn usage_curl_check(exec: &dyn Exec, config: &Config) -> Option<Check> {
    if !config.usage.enabled || aif::usage::identities(config).is_empty() {
        return None;
    }
    let label = "usage curl".to_string();
    let empty = "the USAGE band stays empty".to_string();
    Some(match exec.run("curl", &["--version"], None) {
        Ok(out) if out.status == 0 => {
            let version = out
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("unknown")
                .to_string();
            Check {
                label,
                status: Status::Pass,
                detail: format!("curl {version} reads the usage endpoints"),
            }
        }
        Ok(out) => Check {
            label,
            status: Status::Warn,
            detail: format!("curl --version exited with status {}; {empty}", out.status),
        },
        Err(error) => Check {
            label,
            status: Status::Warn,
            detail: format!("cannot run curl: {error:#}; {empty}"),
        },
    })
}

/// Check that `gh` is authenticated.
///
/// The whole factory reads GitHub through `gh`, so an unauthenticated
/// `gh` stops every repository poll in silence. The check runs
/// `gh auth status` for the active github.com account. An unrelated host
/// cannot make this check fail. The failure line names the required fix.
fn gh_auth_check(exec: &dyn Exec) -> Check {
    let label = "gh auth".to_string();
    let out = match exec.run(
        "gh",
        &["auth", "status", "--hostname", "github.com", "--active"],
        None,
    ) {
        Ok(out) => out,
        Err(error) => {
            return Check {
                label,
                status: Status::Fail,
                detail: format!("cannot run gh: {error:#}"),
            };
        }
    };
    // Some `gh` versions print the status on stdout and others on stderr,
    // so the check reads both streams. The exit code alone does not
    // decide: the account line must be present too.
    let combined = format!("{}{}", out.stdout, out.stderr);
    match (out.status, extract_gh_account(&combined)) {
        (0, Some(account)) => Check {
            label,
            status: Status::Pass,
            detail: format!("logged in to github.com as {account}"),
        },
        _ => Check {
            label,
            status: Status::Fail,
            detail: "not logged in; run: gh auth login".to_string(),
        },
    }
}

/// Extract the account name from the
/// `✓ Logged in to <host> account <name> (...)` line of
/// `gh auth status`.
fn extract_gh_account(output: &str) -> Option<String> {
    for line in output.lines() {
        let Some(start) = line.find(" account ") else {
            continue;
        };
        let rest = &line[start + " account ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Report the daemon connection and its current paused state.
fn daemon_checks(socket: &Path) -> Vec<Check> {
    match Client::connect(socket) {
        Ok(client) => vec![
            Check {
                label: "daemon".to_string(),
                status: Status::Pass,
                detail: format!("running and answering on {}", socket.display()),
            },
            paused_check(&client),
        ],
        Err(error)
            if error_has_io_kind(
                &error,
                &[io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused],
            ) =>
        {
            vec![Check {
                label: "daemon".to_string(),
                status: Status::Info,
                detail: format!("not running at {}", socket.display()),
            }]
        }
        Err(error) => vec![Check {
            label: "daemon".to_string(),
            status: Status::Fail,
            detail: format!("cannot check {}: {error:#}", socket.display()),
        }],
    }
}

/// Report the paused state of a running daemon.
///
/// The call reads the first state push from the connection that proved the
/// daemon is available.
fn paused_check(client: &Client) -> Check {
    if let Err(error) = client.set_read_timeout(PUSH_READ_TIMEOUT) {
        return no_state_check(error);
    }
    let mut pushes = match client.pushes() {
        Ok(pushes) => pushes,
        Err(error) => return no_state_check(error),
    };
    loop {
        match pushes.next() {
            Some(Ok(Push::State(view))) => return paused_check_from_view(&view.paused),
            Some(Ok(
                Push::TicketDetails(_)
                | Push::TicketMentions(_)
                | Push::TicketLabels(_)
                | Push::TicketResult(_)
                | Push::Ask(_)
                | Push::SettingsResult(_),
            )) => {}
            Some(Err(error)) => return no_state_check(error),
            None => {
                return no_state_check(anyhow!(
                    "the daemon closed the stream without a state push"
                ));
            }
        }
    }
}

/// How long the paused check waits for the first state push.
const PUSH_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The check for a daemon that answered but sent no readable state.
fn no_state_check(error: anyhow::Error) -> Check {
    Check {
        label: "paused".to_string(),
        status: Status::Warn,
        detail: format!("the daemon answered but sent no state: {error:#}"),
    }
}

/// The paused check for one paused-state view, split out for tests.
fn paused_check_from_view(paused: &PausedView) -> Check {
    let exact_states: Vec<String> = paused
        .overrides
        .iter()
        .map(|entry| {
            let operation = if entry.paused { "paused" } else { "resumed" };
            match &entry.scope {
                PauseScope::Global => format!("{operation} factory"),
                PauseScope::Stage { stage } => {
                    format!("{operation} stage {}", stage.as_str())
                }
                PauseScope::Lane { stage, repo } => {
                    format!("{operation} lane {}/{repo}", stage.as_str())
                }
                PauseScope::Task { task } => format!("{operation} task {task}"),
            }
        })
        .collect();
    let (status, detail) = if paused.global && exact_states.is_empty() {
        (
            Status::Info,
            "the whole factory is paused; no task dispatches until the operator resumes (press P in the UI)"
                .to_string(),
        )
    } else if paused.global {
        (
            Status::Info,
            format!(
                "the whole factory is paused; exact states: {}",
                exact_states.join(", ")
            ),
        )
    } else if exact_states.is_empty() {
        (Status::Pass, "running; nothing is paused".to_string())
    } else {
        (
            Status::Info,
            format!("exact states: {}", exact_states.join(", ")),
        )
    };
    Check {
        label: "paused".to_string(),
        status,
        detail,
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

/// Report every opencode role that runs without permission auto-approval.
///
/// An unattended `opencode run` without `--auto` auto-rejects every
/// permission request, so tools that read outside the project directory
/// fail. The check resolves each global role and each repository override
/// and warns for every opencode role whose resolved `auto_approve` is not
/// true. A repository resolution that still points at the global role stays
/// silent, because the global table carries the answer.
fn permission_checks(config: &Config) -> Vec<Check> {
    let offenders = |resolved: &aif::config::ResolvedRoleSettings| {
        resolved.settings.harness == Harness::Opencode
            && resolved.settings.auto_approve != Some(true)
    };
    let mut warnings = Vec::new();
    for role in ExecutionRole::ALL {
        if let Ok(resolved) = config.resolved_role(None, role.table_name()) {
            if offenders(&resolved) {
                warnings.push(format!(
                    "{}: opencode auto-rejects every permission request without auto_approve; \
                     unattended tools fail",
                    role.table_name()
                ));
            }
        }
    }
    for repo in config.repos.values() {
        for role in ExecutionRole::ALL {
            if let Ok(resolved) = config.resolved_role(Some(&repo.alias), role.table_name()) {
                if matches!(
                    resolved.source,
                    aif::config::SettingsSource::Repository { .. }
                ) && offenders(&resolved)
                {
                    warnings.push(format!(
                        "repo.{}.{}: opencode auto-rejects every permission request without \
                         auto_approve; unattended tools fail",
                        repo.alias,
                        role.table_name()
                    ));
                }
            }
        }
    }
    if warnings.is_empty() {
        vec![Check {
            label: "permissions".to_string(),
            status: Status::Pass,
            detail: "all opencode roles auto-approve permissions".to_string(),
        }]
    } else {
        warnings
            .into_iter()
            .map(|warning| Check {
                label: "permissions".to_string(),
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

/// The worktrees the manager owns under `dir`: one `(kind, number, path)`
/// triple per `issue-<n>` or `pr-<n>` directory.
///
/// The kinds come from [`WORKTREE_KINDS`], so the manager is the single
/// source of the directory names. Other entries, such as a train worktree,
/// are skipped. A missing directory yields nothing. Other read errors
/// propagate.
fn item_worktrees(dir: &Path) -> Result<Vec<(WorktreeKind, u64, PathBuf)>> {
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
        for kind in WORKTREE_KINDS {
            let Some(number) = name.strip_prefix(kind.prefix()) else {
                continue;
            };
            let Ok(number) = number.parse::<u64>() else {
                continue;
            };
            out.push((kind, number, entry.path()));
        }
    }
    out.sort_by_key(|(kind, number, _)| (*kind, *number));
    Ok(out)
}

/// The worktree path of one item, from the manager's directory names.
fn item_path(state_dir: &Path, alias: &str, kind: WorktreeKind, number: u64) -> PathBuf {
    state_dir
        .join("worktrees")
        .join(alias)
        .join(format!("{}{number}", kind.prefix()))
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
    ///
    /// A ticket worktree is dead when its ticket closes or its PR merged.
    /// A PR worktree is dead when its PR merged or closed: the review it
    /// served is over either way.
    fn is_cleanable(self, kind: WorktreeKind) -> bool {
        match kind {
            WorktreeKind::Issue => {
                matches!(self, WorktreeState::IssueClosed | WorktreeState::PullMerged)
            }
            WorktreeKind::Pr => {
                matches!(self, WorktreeState::PullMerged | WorktreeState::PullClosed)
            }
        }
    }

    /// Describe the state for the report and the clean preview.
    fn detail(self, number: u64) -> String {
        match self {
            WorktreeState::IssueOpen => format!("ticket {number} is open"),
            WorktreeState::IssueClosed => format!("ticket {number} is closed"),
            WorktreeState::PullOpen => format!("PR {number} is open"),
            WorktreeState::PullClosed => {
                format!("PR {number} is closed without a merge")
            }
            WorktreeState::PullMerged => format!("PR {number} is merged"),
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
        let worktrees = match item_worktrees(&worktree_dir) {
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
            for (kind, number, _) in &worktrees {
                checks.push(Check {
                    label: worktree_label(&repo.alias, *kind, *number),
                    status: Status::Warn,
                    detail: "item state unknown: no repository facts exist".to_string(),
                });
            }
            continue;
        };
        let owner = match &fact.owner_repo {
            Ok(owner) => owner,
            Err(reason) => {
                for (kind, number, _) in &worktrees {
                    checks.push(Check {
                        label: worktree_label(&repo.alias, *kind, *number),
                        status: Status::Warn,
                        detail: format!("item state unknown: {reason}"),
                    });
                }
                continue;
            }
        };
        for (kind, number, _) in &worktrees {
            let (status, detail) = match worktree_state(env.exec, owner, *number) {
                Ok(state) => (Status::Info, state.detail(*number)),
                Err(error) => (Status::Warn, format!("item state unknown: {error:#}")),
            };
            checks.push(Check {
                label: worktree_label(&repo.alias, *kind, *number),
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

/// The report label of one item worktree.
fn worktree_label(alias: &str, kind: WorktreeKind, number: u64) -> String {
    format!("worktree {alias} {}{number}", kind.prefix())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aif::exec::{CmdOut, ScriptExec};
    use aif::model::Stage;
    use aif::sock::{Action, Server, SettingsView, StateView, WIRE_PROTOCOL_REVISION};
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
        let mut text = "schema_version = 1\n".to_string();
        for stage in Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nharness = \"claude\"\n"
            ));
            for (name, extra) in stage_extras {
                if *name == stage.as_str() {
                    text.push_str(extra);
                    text.push('\n');
                }
            }
        }
        text.push_str("[ticket.create]\nmodel = \"model\"\nharness = \"opencode\"\n");
        text.push_str("[ticket.chat]\nmodel = \"model\"\nharness = \"claude\"\n");
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
            fs::create_dir_all(item_path(&state_dir, "acme", WorktreeKind::Issue, number))
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
            ("2.74.0-19-gea8fc856e", Some((2, 74, 0))),
            ("v2.1.3-rc1", Some((2, 1, 3))),
            ("release 1.2.3.4", None),
            ("1.2.3.4-rc1", None),
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

    /// The version lines of this machine, captured on 2026-08-30 by running
    /// each tool. Tests never run a tool; these lines are hard-coded.
    #[test]
    fn version_parse_reads_the_real_version_lines_of_this_machine() {
        let cases = [
            (
                "gh version 2.74.0-19-gea8fc856e (2025-06-09)",
                Some((2, 74, 0)),
            ),
            ("git version 2.34.1", Some((2, 34, 1))),
            ("2.1.251 (Claude Code)", Some((2, 1, 251))),
            ("1.18.25", Some((1, 18, 25))),
        ];
        for (line, want) in cases {
            let parsed = Version::parse(line);
            assert_eq!(
                parsed.map(|v| (v.major, v.minor, v.patch)),
                want,
                "input {line:?}"
            );
        }
    }

    #[test]
    fn a_date_word_and_a_fourth_component_stay_rejected() {
        // A release date keeps only its year prefix `2025`. That prefix has
        // no minor component, so the date never becomes a version.
        assert_eq!(Version::parse("(2025-06-09)"), None);
        assert_eq!(Version::parse("2025-06-09"), None);
        // More than three numeric components stay rejected, with and without
        // a suffix.
        assert_eq!(Version::parse("release 1.2.3.4"), None);
        assert_eq!(Version::parse("1.2.3.4-rc1"), None);
        // The first version-like word wins, so a trailing date cannot
        // override a real version.
        assert_eq!(
            Version::parse("gh version 2.74.0-19-gea8fc856e (2025-06-09)")
                .map(|v| (v.major, v.minor, v.patch)),
            Some((2, 74, 0))
        );
    }

    #[test]
    fn the_real_gh_describe_output_passes_tool_check() {
        let exec = ScriptExec::new().expect(
            |call| call.program == "gh" && call.args == ["--version"],
            CmdOut::ok(
                "gh version 2.74.0-19-gea8fc856e (2025-06-09)\n\
                 https://github.com/cli/cli/releases/latest\n",
            ),
        );

        let check = tool_check(&exec, "gh", false);

        assert_eq!(check.status, Status::Pass);
        assert_eq!(check.detail, "gh 2.74.0");
    }

    #[test]
    fn tool_checks_use_each_configured_program_once() {
        let text = config_text(
            &[
                ("refine", "program = \"shared-agent\""),
                ("implement", "program = \"shared-agent\""),
                ("review", "program = \"review-agent\""),
            ],
            "[repo.demo]\npath = \"/tmp/demo\"\n\
             [repo.demo.stage.review]\nprogram = \"repo-review\"\n",
        );
        let config = Config::parse(&text).expect("the role configuration must parse");
        let exec = ScriptExec::new()
            .expect(|call| call.program == "gh", CmdOut::ok("gh 2.74.0\n"))
            .expect(|call| call.program == "git", CmdOut::ok("git 2.43.0\n"))
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("Claude Code 2.1.251\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            )
            .expect(
                |call| call.program == "repo-review",
                CmdOut::ok("repo-review 2.1.251\n"),
            )
            .expect(
                |call| call.program == "review-agent",
                CmdOut::ok("review-agent 2.1.251\n"),
            )
            .expect(
                |call| call.program == "shared-agent",
                CmdOut::ok("shared-agent 2.1.251\n"),
            );

        let checks = tool_checks(&exec, Some(&config));

        assert_eq!(checks.len(), 7, "checks: {checks:?}");
        let programs: Vec<_> = exec.calls().into_iter().map(|call| call.program).collect();
        assert_eq!(
            programs,
            [
                "gh",
                "git",
                "claude",
                "opencode",
                "repo-review",
                "review-agent",
                "shared-agent"
            ]
        );
    }

    #[test]
    fn claude_floor_applies_only_to_claude_role_programs() {
        let text = "schema_version = 1\n\
            [stage.refine]\nharness = \"claude\"\nprogram = \"claude-wrapper\"\nmodel = \"m\"\n\
            [stage.implement]\nharness = \"opencode\"\nprogram = \"shared\"\nmodel = \"m\"\n\
            [stage.review]\nharness = \"opencode\"\nprogram = \"shared\"\nmodel = \"m\"\n\
            [stage.release]\nharness = \"opencode\"\nprogram = \"shared\"\nmodel = \"m\"\n\
            [ticket.create]\nharness = \"opencode\"\nprogram = \"shared\"\nmodel = \"m\"\n\
            [ticket.chat]\nharness = \"opencode\"\nprogram = \"shared\"\nmodel = \"m\"\n";
        let config = Config::parse(text).expect("the role configuration must parse");
        let exec = ScriptExec::new()
            .expect(|call| call.program == "gh", CmdOut::ok("gh 2.74.0\n"))
            .expect(|call| call.program == "git", CmdOut::ok("git 2.43.0\n"))
            .expect(
                |call| call.program == "claude-wrapper",
                CmdOut::ok("Claude Code 2.1.100\n"),
            )
            .expect(
                |call| call.program == "shared",
                CmdOut::ok("shared 1.0.0\n"),
            );

        let checks = tool_checks(&exec, Some(&config));

        let claude = checks
            .iter()
            .find(|check| check.label == "claude-wrapper")
            .expect("the Claude program must be checked");
        assert_eq!(claude.status, Status::Fail);
        assert!(claude.detail.contains("required floor 2.1.223"));
        let shared = checks
            .iter()
            .find(|check| check.label == "shared")
            .expect("the OpenCode program must be checked");
        assert_eq!(shared.status, Status::Pass);
    }

    #[test]
    fn a_missing_configured_codex_program_is_a_failure() {
        let text = config_text(
            &[(
                "review",
                "harness = \"codex\"\nprogram = \"missing-codex\"\nprofile = \"reviewer\"",
            )],
            "",
        )
        .replacen(
            "harness = \"claude\"\nharness = \"codex\"",
            "harness = \"codex\"",
            1,
        );
        let config = Config::parse(&text).expect("the Codex role configuration must parse");
        let exec = ScriptExec::new()
            .expect(|call| call.program == "gh", CmdOut::ok("gh 2.74.0\n"))
            .expect(|call| call.program == "git", CmdOut::ok("git 2.43.0\n"))
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("Claude Code 2.1.251\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            );

        let checks = tool_checks(&exec, Some(&config));

        let codex = checks
            .iter()
            .find(|check| check.label == "missing-codex")
            .expect("the Codex program must be checked");
        assert_eq!(codex.status, Status::Fail);
        assert!(codex.detail.contains("cannot run missing-codex"));
        assert!(exec
            .calls()
            .iter()
            .any(|call| call.program == "missing-codex" && call.args == ["--version"]));
    }

    #[test]
    fn a_configured_program_remains_one_direct_executable_string() {
        let text = config_text(&[("review", "program = \"review-wrapper --strict\"")], "");
        let config = Config::parse(&text).expect("the custom program must parse");
        let exec = ScriptExec::new()
            .expect(|call| call.program == "gh", CmdOut::ok("gh 2.74.0\n"))
            .expect(|call| call.program == "git", CmdOut::ok("git 2.43.0\n"))
            .expect(
                |call| call.program == "claude",
                CmdOut::ok("Claude Code 2.1.251\n"),
            )
            .expect(
                |call| call.program == "opencode",
                CmdOut::ok("opencode 1.18.25\n"),
            )
            .expect(
                |call| call.program == "review-wrapper --strict",
                CmdOut::ok("review-wrapper 1.0.0\n"),
            );

        let checks = tool_checks(&exec, Some(&config));

        assert!(checks
            .iter()
            .any(|check| check.label == "review-wrapper --strict"));
        assert!(exec.calls().iter().any(|call| {
            call.program == "review-wrapper --strict" && call.args == ["--version"]
        }));
    }

    // --- gh authentication. ---

    /// The real stdout of `gh auth status` when logged in, captured by
    /// hand on gh 2.74.0.
    const GH_AUTH_LOGGED_IN: &str = concat!(
        "github.com\n",
        "  ✓ Logged in to github.com account navaro1 \
         (/home/navaro/snap/gh/640/.config/gh/hosts.yml)\n",
        "  - Active account: true\n",
        "  - Git operations protocol: ssh\n",
        "  - Token: gho_************************************\n",
        "  - Token scopes: 'admin:org', 'gist', 'project', 'repo'\n",
    );

    /// The real stderr of `gh auth status` with no configured host,
    /// captured by hand on gh 2.74.0. The exit status is 1.
    const GH_AUTH_LOGGED_OUT: &str =
        "You are not logged into any GitHub hosts. To log in, run: gh auth login\n";

    /// A scripted executor that answers one `gh auth status` call.
    fn gh_auth_exec(out: CmdOut) -> ScriptExec {
        ScriptExec::new().expect(
            |call| {
                call.program == "gh"
                    && call.args == ["auth", "status", "--hostname", "github.com", "--active"]
            },
            out,
        )
    }

    #[test]
    fn a_logged_in_gh_auth_status_passes_with_the_account() {
        // The real answer of gh 2.74.0 when logged in: the status text
        // arrives on stdout and the exit status is 0.
        let exec = gh_auth_exec(CmdOut::ok(GH_AUTH_LOGGED_IN));

        let check = gh_auth_check(&exec);

        assert_eq!(check.label, "gh auth");
        assert_eq!(check.status, Status::Pass);
        assert_eq!(check.detail, "logged in to github.com as navaro1");
    }

    // --- usage curl. ---

    #[test]
    fn the_usage_curl_check_passes_with_a_version_line() {
        let config = Config::parse(&config_text(&[], "")).unwrap();
        let exec = ScriptExec::new().expect(
            |call| call.program == "curl" && call.args == ["--version"],
            CmdOut::ok("curl 8.5.0 (x86_64-pc-linux-gnu) libcurl/8.5.0\n"),
        );

        let check = usage_curl_check(&exec, &config).unwrap();

        assert_eq!(check.label, "usage curl");
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("curl 8.5.0"), "{}", check.detail);
    }

    #[test]
    fn the_usage_curl_check_warns_when_curl_is_missing() {
        let config = Config::parse(&config_text(&[], "")).unwrap();
        let exec = ScriptExec::new();

        let check = usage_curl_check(&exec, &config).unwrap();

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("USAGE band stays empty"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn the_usage_curl_check_disappears_when_probes_are_disabled() {
        let text = format!("{}\n[usage]\nenabled = false\n", config_text(&[], ""));
        let config = Config::parse(&text).unwrap();
        let exec = ScriptExec::new();

        assert!(usage_curl_check(&exec, &config).is_none());
        assert!(
            !exec.calls().iter().any(|call| call.program == "curl"),
            "a disabled usage table must not run curl"
        );
    }

    #[test]
    fn the_report_carries_the_usage_curl_check_for_a_parsed_config() {
        let dir = temp_dir("usage-curl-report");
        let config_path = dir.join("factory.toml");
        std::fs::write(&config_path, config_text(&[], "")).unwrap();
        let exec = ScriptExec::new();
        let env = DoctorEnv {
            config_path: &config_path,
            state_dir: &dir,
            socket: &dir.join("daemon.sock"),
            exec: &exec,
        };

        let checks = report(&env);

        assert!(
            checks.iter().any(|check| check.label == "usage curl"),
            "checks: {:?}",
            checks
                .iter()
                .map(|check| check.label.as_str())
                .collect::<Vec<_>>()
        );
        assert!(exec.calls().iter().any(|call| call.program == "curl"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_logged_out_gh_auth_status_fails_and_names_the_fix() {
        // The real answer of gh 2.74.0 with no configured host: the
        // message arrives on stderr and the exit status is 1.
        let exec = gh_auth_exec(CmdOut {
            status: 1,
            stdout: String::new(),
            stderr: GH_AUTH_LOGGED_OUT.to_string(),
        });

        let check = gh_auth_check(&exec);

        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, "not logged in; run: gh auth login");
        assert!(has_failures(&[check]));
    }

    #[test]
    fn a_logged_in_answer_on_stderr_still_passes() {
        // Some gh versions print the logged-in status on stderr, so the
        // check reads both streams.
        let exec = gh_auth_exec(CmdOut {
            status: 0,
            stdout: String::new(),
            stderr: GH_AUTH_LOGGED_IN.to_string(),
        });

        let check = gh_auth_check(&exec);

        assert_eq!(check.status, Status::Pass);
        assert_eq!(check.detail, "logged in to github.com as navaro1");
    }

    #[test]
    fn a_zero_exit_without_an_account_line_still_fails() {
        // The exit code alone does not decide; the account line must be
        // present too.
        let exec = gh_auth_exec(CmdOut::ok("warning: nothing useful\n"));

        let check = gh_auth_check(&exec);

        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, "not logged in; run: gh auth login");
    }

    /// Whether any check failed, which decides the `aif doctor` exit code.
    #[test]
    fn has_failures_decides_the_doctor_exit_code() {
        let one = |status| {
            vec![Check {
                label: "check".to_string(),
                status,
                detail: "detail".to_string(),
            }]
        };
        // No fail status reports no failure. An empty report does the same.
        assert!(!has_failures(&one(Status::Pass)));
        assert!(!has_failures(&one(Status::Info)));
        assert!(!has_failures(&one(Status::Warn)));
        assert!(!has_failures(&[]));
        // One fail anywhere decides the exit code.
        assert!(has_failures(&one(Status::Fail)));
        let mut mixed = one(Status::Pass);
        mixed.extend(one(Status::Warn));
        mixed.extend(one(Status::Fail));
        assert!(has_failures(&mixed));
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

        let check = tool_check(&exec, "gh", false);

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
            )
            .expect(
                |call| {
                    call.program == "gh"
                        && call.args == ["auth", "status", "--hostname", "github.com", "--active"]
                },
                CmdOut::ok(GH_AUTH_LOGGED_IN),
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

        let first = checks.first().expect("the report must have rows");
        assert_eq!(first.label, "aif");
        assert_eq!(first.status, Status::Pass);
        assert_eq!(first.detail, format!("aif {}", env!("CARGO_PKG_VERSION")));
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
        for tool in CORE_TOOLS {
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
        let gh_auth = checks
            .iter()
            .find(|check| check.label == "gh auth")
            .expect("the gh auth check must exist");
        assert_eq!(gh_auth.status, Status::Fail);
        assert!(
            gh_auth.detail.contains("cannot run"),
            "gh auth: {}",
            gh_auth.detail
        );
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

        let checks = daemon_checks(&socket);

        assert_eq!(checks.len(), 1, "checks: {checks:?}");
        let check = &checks[0];
        assert_eq!(check.label, "daemon");
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
                |call| {
                    call.program == "gh"
                        && call.args == ["auth", "status", "--hostname", "github.com", "--active"]
                },
                CmdOut::ok(GH_AUTH_LOGGED_IN),
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
        assert_eq!(
            config.detail,
            format!(
                "{} parses, schema 1, 1 repositories",
                fx.config_path.display()
            )
        );
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
        for tool in ["gh", "git", "claude", "opencode"] {
            let check = checks
                .iter()
                .find(|check| check.label == tool)
                .unwrap_or_else(|| panic!("the {tool} check must exist"));
            assert_eq!(check.status, Status::Pass, "{tool}: {}", check.detail);
        }
        let gh_auth = checks
            .iter()
            .find(|check| check.label == "gh auth")
            .expect("the gh auth check must exist");
        assert_eq!(gh_auth.status, Status::Pass);
        assert_eq!(gh_auth.detail, "logged in to github.com as navaro1");
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
            open.detail.contains("ticket 7 is open"),
            "detail: {}",
            open.detail
        );
        let closed = checks
            .iter()
            .find(|check| check.label == "worktree acme issue-8")
            .expect("the closed worktree check must exist");
        assert!(
            closed.detail.contains("ticket 8 is closed"),
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
        // The nine tool, auth, and repository answers plus the usage curl
        // version check of the enabled [usage] table.
        assert_eq!(exec.calls().len(), 10, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn the_first_row_of_a_full_report_is_the_aif_row() {
        let fx = fixture();
        let exec = repo_answers(ScriptExec::new(), &fx.repo_path);
        let env = fixture_env(&fx, &exec);

        let checks = report(&env);

        let first = checks.first().expect("the report must have rows");
        assert_eq!(first.label, "aif");
        assert_eq!(first.status, Status::Pass);
        assert_eq!(first.detail, format!("aif {}", env!("CARGO_PKG_VERSION")));
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
    fn item_worktrees_skips_a_stale_sibling_directory() {
        let dir = temp_dir("stale-item");
        let worktrees = dir.join("worktrees");
        fs::create_dir_all(worktrees.join("pr-7")).expect("the pr worktree must be creatable");
        fs::create_dir_all(worktrees.join("pr-7.stale-1"))
            .expect("the stale sibling must be creatable");

        let found = item_worktrees(&worktrees).expect("the listing must succeed");

        assert_eq!(
            found.len(),
            1,
            "the stale sibling must not parse as an item: {found:?}"
        );
        assert_eq!(found[0].0, WorktreeKind::Pr);
        assert_eq!(found[0].1, 7);
        assert_eq!(found[0].2, worktrees.join("pr-7"));
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
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

    /// Plain role text where only `stage.review` runs opencode without
    /// `auto_approve`; every other role runs claude or approves.
    fn permissions_review_text(review_extra: &str) -> String {
        format!(
            "schema_version = 1\n\
             [stage.refine]\nharness = \"claude\"\nmodel = \"m\"\n\
             [stage.implement]\nharness = \"opencode\"\nmodel = \"m\"\nauto_approve = true\n\
             [stage.review]\nharness = \"opencode\"\nmodel = \"m\"\n{review_extra}\
             [stage.release]\nharness = \"claude\"\nmodel = \"m\"\n\
             [ticket.create]\nharness = \"claude\"\nmodel = \"m\"\n\
             [ticket.chat]\nharness = \"claude\"\nmodel = \"m\"\n"
        )
    }

    #[test]
    fn an_opencode_role_without_auto_approve_warns() {
        let config =
            Config::parse(&permissions_review_text("")).expect("the role configuration must parse");

        let checks = permission_checks(&config);

        assert_eq!(checks.len(), 1, "checks: {checks:?}");
        assert_eq!(checks[0].label, "permissions");
        assert_eq!(checks[0].status, Status::Warn);
        assert!(
            checks[0].detail.contains("stage.review"),
            "detail: {}",
            checks[0].detail
        );
        assert!(
            checks[0]
                .detail
                .contains("opencode auto-rejects every permission request"),
            "detail: {}",
            checks[0].detail
        );
    }

    #[test]
    fn a_disapproving_repository_override_warns_with_its_table_path() {
        let text = permissions_review_text("auto_approve = true\n")
            + "[repo.demo]\npath = \"/tmp/demo\"\n\
               [repo.demo.stage.review]\nauto_approve = false\n";
        let config = Config::parse(&text).expect("the role configuration must parse");

        let checks = permission_checks(&config);

        assert_eq!(checks.len(), 1, "checks: {checks:?}");
        assert_eq!(checks[0].status, Status::Warn);
        assert!(
            checks[0].detail.contains("repo.demo.stage.review"),
            "detail: {}",
            checks[0].detail
        );
        assert!(
            !checks[0].detail.starts_with("stage.review"),
            "detail: {}",
            checks[0].detail
        );
    }

    #[test]
    fn approving_and_non_opencode_roles_pass_the_permissions_check() {
        let example = Config::parse(include_str!("../docs/v0.5/factory.example.toml"))
            .expect("the installer example must parse");
        let claude_only = "schema_version = 1\n\
             [stage.refine]\nharness = \"claude\"\nmodel = \"m\"\n\
             [stage.implement]\nharness = \"claude\"\nmodel = \"m\"\n\
             [stage.review]\nharness = \"claude\"\nmodel = \"m\"\n\
             [stage.release]\nharness = \"claude\"\nmodel = \"m\"\n\
             [ticket.create]\nharness = \"claude\"\nmodel = \"m\"\n\
             [ticket.chat]\nharness = \"claude\"\nmodel = \"m\"\n";
        let claude_only = Config::parse(claude_only).expect("the claude roles must parse");
        let codex_review = permissions_review_text("")
            .replacen(
                "[stage.review]\nharness = \"opencode\"\nmodel = \"m\"\n",
                "[stage.review]\nharness = \"codex\"\nmodel = \"m\"\nprofile = \"p\"\n",
                1,
            )
            .replacen(
                "[stage.implement]\nharness = \"opencode\"\nmodel = \"m\"\nauto_approve = true\n",
                "[stage.implement]\nharness = \"claude\"\nmodel = \"m\"\n",
                1,
            );
        let codex_review = Config::parse(&codex_review).expect("the codex roles must parse");

        for config in [&example, &claude_only, &codex_review] {
            let checks = permission_checks(config);
            assert_eq!(checks.len(), 1, "checks: {checks:?}");
            assert_eq!(checks[0].label, "permissions");
            assert_eq!(checks[0].status, Status::Pass);
            assert_eq!(
                checks[0].detail,
                "all opencode roles auto-approve permissions"
            );
            assert!(!checks.iter().any(|check| check.status == Status::Warn));
        }
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
        let closed_text = item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7)
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
            remove_path: item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7),
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
            !item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists(),
            "the closed issue's worktree must be removed"
        );
        assert!(
            item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 8).exists(),
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
            item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists(),
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
        let closed_text = item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7)
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
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists());
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 8).exists());
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
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists());
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 8).exists());
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
        let closed_path = item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7);
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
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 8).exists());
        fs::remove_dir_all(&fx.dir).expect("the temp dir must be removable");
    }

    #[test]
    fn clean_removes_a_pr_worktree_whose_pr_closed_without_a_merge() {
        let fx = fixture();
        let pr_dir = item_path(&fx.state_dir, "acme", WorktreeKind::Pr, 3);
        fs::create_dir_all(&pr_dir).expect("the pr worktree dir must be creatable");
        let repo_text = fx.repo_path.to_string_lossy().into_owned();
        let pr_text = pr_dir.to_string_lossy().into_owned();
        let removal_argv = git_args(&["-C", &repo_text, "worktree", "remove", "--force", &pr_text]);
        let branch_argv = git_args(&["-C", &repo_text, "branch", "-D", "aif/acme/pr-3"]);
        let script = repo_answers(ScriptExec::new(), &fx.repo_path)
            .expect(
                gh_get("repos/acme/borsuk/issues/7"),
                issue_answer(7, "open"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/8"),
                issue_answer(8, "open"),
            )
            .expect(
                gh_get("repos/acme/borsuk/issues/3"),
                pull_item_answer(3, "closed"),
            )
            .expect(
                gh_get("repos/acme/borsuk/pulls/3"),
                CmdOut::ok(r#"{"number":3,"merged":false}"#),
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
            remove_path: pr_dir.clone(),
        };
        let env = fixture_env(&fx, &exec);

        let code = clean(&env, true, &mut || Ok(false)).expect("the clean must succeed");

        assert_eq!(code, 0);
        assert!(
            !pr_dir.exists(),
            "the pr worktree of a closed PR must be removed"
        );
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists());
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
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 7).exists());
        assert!(item_path(&fx.state_dir, "acme", WorktreeKind::Issue, 8).exists());
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
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            false,
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
                "--property=KillMode=mixed",
                "--property=TimeoutStopSec=45",
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
        let spawned: RefCell<Vec<(PathBuf, bool)>> = RefCell::new(Vec::new());
        let spawn_socket = socket.clone();
        let mut spawn = |program: &Path, paused: bool| {
            spawned.borrow_mut().push((program.to_path_buf(), paused));
            fake_daemon(spawn_socket.clone(), 30);
            Ok(())
        };

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            true,
            &mut spawn,
        )
        .expect("the start must succeed");

        let spawned = spawned.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].0.as_os_str(), "/opt/aif/bin/aifd");
        assert!(spawned[0].1, "the fallback must receive the paused flag");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_reports_an_existing_daemon_without_starting_one() {
        let dir = temp_dir("already-running");
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("the fake daemon must bind the socket");
        let exec = ScriptExec::new();
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        let outcome = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            true,
            &mut spawn,
        )
        .expect("the existing daemon check must succeed");

        assert_eq!(outcome, StartOutcome::AlreadyRunning);
        assert!(exec.calls().is_empty());
        assert!(!spawned.get());
        drop(listener);
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_reports_a_systemd_run_execution_error_without_a_fallback() {
        let dir = temp_dir("systemd-denied");
        let socket = dir.join("daemon.sock");
        let exec = IoErrorExec(std::io::ErrorKind::PermissionDenied);
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        let error = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_millis(100),
            false,
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
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        let error = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            false,
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
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        let result = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_millis(150),
            false,
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
    fn start_detached_resets_a_stale_unit_and_retries_systemd_run() {
        let dir = temp_dir("stale-unit");
        let socket = dir.join("daemon.sock");
        fake_daemon(socket.clone(), 150);
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "systemd-run",
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: "Failed to start transient service unit: Unit aif-daemon.service \
                         already exists.\n"
                        .to_string(),
                },
            )
            .expect(
                |call| {
                    call.program == "systemctl"
                        && call.argv() == ["--user", "reset-failed", "aif-daemon"]
                },
                CmdOut::ok(""),
            )
            .expect(|call| call.program == "systemd-run", CmdOut::ok(""));
        let spawned = Cell::new(false);
        let mut spawn = |_program: &Path, _paused: bool| {
            spawned.set(true);
            Ok(())
        };

        let outcome = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            false,
            &mut spawn,
        )
        .expect("the retry must start the daemon");

        assert_eq!(outcome, StartOutcome::Started);
        let calls = exec.calls();
        assert_eq!(calls.len(), 3, "calls: {calls:?}");
        assert_eq!(calls[0].program, "systemd-run");
        assert_eq!(calls[1].argv(), ["--user", "reset-failed", "aif-daemon"]);
        assert_eq!(calls[2].program, "systemd-run");
        assert!(
            !spawned.get(),
            "a stale unit must not use the plain spawn fallback"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_reports_a_stale_unit_that_survives_the_reset() {
        let dir = temp_dir("stale-unit-twice");
        let socket = dir.join("daemon.sock");
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "systemd-run",
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: "Unit aif-daemon.service already exists.\n".to_string(),
                },
            )
            .expect(|call| call.program == "systemctl", CmdOut::ok(""))
            .expect(
                |call| call.program == "systemd-run",
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: "Unit aif-daemon.service already exists again.\n".to_string(),
                },
            );
        let mut spawn = |_program: &Path, _paused: bool| Ok(());

        let error = start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            false,
            &mut spawn,
        )
        .expect_err("a second unit failure must stop the start");

        let message = format!("{error:#}");
        assert!(
            message.contains("systemd-run exited with status 1")
                && message.contains("already exists"),
            "error: {message}"
        );
        assert_eq!(exec.calls().len(), 3, "calls: {:?}", exec.calls());
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn cleanup_daemon_unit_stops_and_resets_the_unit() {
        let exec = ScriptExec::new()
            .expect(
                |call| {
                    call.program == "systemctl" && call.argv() == ["--user", "stop", "aif-daemon"]
                },
                CmdOut {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .expect(
                |call| {
                    call.program == "systemctl"
                        && call.argv() == ["--user", "reset-failed", "aif-daemon"]
                },
                CmdOut::ok(""),
            );

        cleanup_daemon_unit(&exec);

        assert_eq!(exec.calls().len(), 2, "calls: {:?}", exec.calls());
    }

    #[test]
    fn cleanup_daemon_unit_ignores_every_failure() {
        // No scripted steps: each call fails as unexpected, which the
        // cleanup must swallow.
        let exec = ScriptExec::new();

        cleanup_daemon_unit(&exec);

        assert_eq!(exec.calls().len(), 2, "calls: {:?}", exec.calls());
    }

    #[test]
    fn spawn_detached_passes_the_paused_flag_to_a_fake_child() {
        let dir = temp_dir("real-spawn");
        let marker = dir.join("marker");
        let script = dir.join("fake-aifd");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\nsleep 1\n",
                marker.display()
            ),
        )
        .expect("the script write must succeed");
        fs::set_permissions(&script, Permissions::from_mode(0o755))
            .expect("the script must be chmodable");

        spawn_detached(&script, true).expect("the detached spawn must succeed");

        let mut found = false;
        for _ in 0..100 {
            if marker.exists() {
                found = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(found, "the detached child never ran");
        assert_eq!(
            fs::read_to_string(&marker).expect("the fake child arguments must be readable"),
            "run --paused"
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn start_detached_builds_the_paused_flag_into_the_argv() {
        let dir = temp_dir("systemd-paused");
        let socket = dir.join("daemon.sock");
        fake_daemon(socket.clone(), 50);
        let exec = ScriptExec::new().expect(|call| call.program == "systemd-run", CmdOut::ok(""));
        let mut spawn = |_program: &Path, _paused: bool| Ok(());

        start_detached(
            &socket,
            Path::new("/opt/aif/bin/aifd"),
            &exec,
            Duration::from_secs(5),
            true,
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
                "--property=KillMode=mixed",
                "--property=TimeoutStopSec=45",
                "--",
                "/opt/aif/bin/aifd",
                "run",
                "--paused"
            ]
        );
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn the_paused_check_reports_a_globally_paused_daemon() {
        let check = paused_check_from_view(&PausedView {
            global: true,
            overrides: Vec::new(),
        });
        assert_eq!(check.label, "paused");
        assert_eq!(check.status, Status::Info);
        assert!(check.detail.contains("paused"), "detail: {}", check.detail);
        assert!(
            check.detail.contains("P in the UI"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn the_report_reads_the_paused_state_from_the_daemon_socket() {
        let dir = temp_dir("paused-socket");
        let socket = dir.join("daemon.sock");
        let (server, _actions) = Server::bind(&socket).expect("the fake daemon must bind");
        server.publish(StateView {
            protocol_revision: WIRE_PROTOCOL_REVISION,
            links: Vec::new(),
            repos: Vec::new(),
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions: Vec::new(),
            decision_items: Vec::new(),
            tickets: Vec::new(),
            prs: Vec::new(),
            trains: Vec::new(),
            paused: PausedView {
                global: true,
                overrides: Vec::new(),
            },
            settings: SettingsView::default(),
            usage: Vec::new(),
        });
        let missing_config = dir.join("factory.toml");
        let exec = ScriptExec::new();
        let env = DoctorEnv {
            config_path: &missing_config,
            state_dir: &dir,
            socket: &socket,
            exec: &exec,
        };

        let checks = report(&env);

        let paused = checks
            .iter()
            .find(|check| check.label == "paused")
            .expect("the paused check must exist");
        assert_eq!(paused.status, Status::Info);
        assert!(
            paused.detail.contains("the whole factory is paused"),
            "detail: {}",
            paused.detail
        );
        drop(server);
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn the_paused_check_reports_a_running_daemon_and_partial_pauses() {
        let check = paused_check_from_view(&PausedView {
            global: false,
            overrides: Vec::new(),
        });
        assert_eq!(check.status, Status::Pass);
        assert!(
            check.detail.contains("nothing is paused"),
            "detail: {}",
            check.detail
        );

        let check = paused_check_from_view(&PausedView {
            global: false,
            overrides: vec![
                aif::sock::PauseOverrideView {
                    scope: PauseScope::Stage {
                        stage: Stage::Refine,
                    },
                    paused: true,
                },
                aif::sock::PauseOverrideView {
                    scope: PauseScope::Lane {
                        stage: Stage::Release,
                        repo: "borsuk".to_string(),
                    },
                    paused: false,
                },
            ],
        });
        assert_eq!(check.status, Status::Info);
        assert!(check.detail.contains("refine"), "detail: {}", check.detail);
        assert!(check.detail.contains("borsuk"), "detail: {}", check.detail);
        assert!(
            !check.detail.contains("the whole factory"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn the_paused_check_reports_nothing_when_no_daemon_listens() {
        let dir = temp_dir("paused-down");
        let checks = daemon_checks(&dir.join("absent.sock"));
        assert_eq!(checks.len(), 1, "checks: {checks:?}");
        assert_eq!(checks[0].label, "daemon");
        assert_eq!(checks[0].status, Status::Info);
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
