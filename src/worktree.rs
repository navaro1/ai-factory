//! Per-issue git worktrees and the marker files that carry stage state.
//!
//! One worktree per issue lives under `<state_dir>/worktrees/<repo>/` and is
//! reused across the stages of that issue. Marker files in the worktree's
//! `.aif` directory carry the agent session id and the last reviewed head
//! sha, so a daemon restart resumes work in place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::config::RepoConfig;
use crate::exec::{CmdOut, Exec};

/// The directory inside a worktree that holds the marker files.
const AIF_DIR: &str = ".aif";

/// The exclude-pattern line that hides [`AIF_DIR`] from git.
const EXCLUDE_ENTRY: &str = ".aif/";

/// The marker file that holds the agent session id.
const SESSION_MARKER: &str = "session";

/// The marker file that holds the head sha of the last completed review.
const REVIEWED_SHA_MARKER: &str = "reviewed-sha";

/// Proof that an issue is no longer live, so its worktree may go away.
///
/// [`WorktreeManager::remove_issue`] takes this value as its only guard.
/// There is no boolean force flag: a caller must state the proof, and the
/// proof comes from GitHub, not from local bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanable {
    /// The issue is merged or closed on GitHub. The factory proved this
    /// through the release stage or the poller, so the worktree and its
    /// branch hold no live work.
    MergedOrClosed,
}

/// One kind of item worktree the manager owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorktreeKind {
    /// The worktree of one ticket: the `issue-<n>` directory.
    Issue,
    /// The worktree of one PR: the `pr-<n>` directory.
    Pr,
}

impl WorktreeKind {
    /// The directory prefix: `issue-` or `pr-`.
    pub fn prefix(self) -> &'static str {
        match self {
            WorktreeKind::Issue => "issue-",
            WorktreeKind::Pr => "pr-",
        }
    }
}

/// The worktree kinds the manager owns. The doctor asks for this list, so
/// the manager is the single source of the directory names.
pub const WORKTREE_KINDS: [WorktreeKind; 2] = [WorktreeKind::Issue, WorktreeKind::Pr];

/// The directory name of the train worktree. The manager owns this name,
/// so every caller that prints it reads this const.
pub const TRAIN_DIR: &str = "train";

/// Creates, reuses, and removes one git worktree per issue, plus the train
/// worktree of a repository.
///
/// The paths follow the naming rules in `docs/v0.5/SPEC.md`:
/// `<state_dir>/worktrees/<alias>/issue-<n>` and
/// `<state_dir>/worktrees/<alias>/train`. The alias comes from a validated
/// [`RepoConfig`], so it is always a safe single path segment.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// The state directory under which all worktrees live.
    state_dir: PathBuf,
}

impl WorktreeManager {
    /// A manager that keeps worktrees under `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        WorktreeManager {
            state_dir: state_dir.into(),
        }
    }

    /// The worktree path for one issue: `<state_dir>/worktrees/<alias>/issue-<n>`.
    pub fn issue_path(&self, repo: &RepoConfig, number: u64) -> PathBuf {
        self.path(repo, WorktreeKind::Issue, number)
    }

    /// The worktree path for one PR: `<state_dir>/worktrees/<alias>/pr-<n>`.
    pub fn pr_path(&self, repo: &RepoConfig, number: u64) -> PathBuf {
        self.path(repo, WorktreeKind::Pr, number)
    }

    /// The worktree path of one kind: `issue-<n>` or `pr-<n>`.
    pub fn path(&self, repo: &RepoConfig, kind: WorktreeKind, number: u64) -> PathBuf {
        self.state_dir
            .join("worktrees")
            .join(&repo.alias)
            .join(format!("{}{number}", kind.prefix()))
    }

    /// The train worktree path: `<state_dir>/worktrees/<alias>/train`.
    pub fn train_path(&self, repo: &RepoConfig) -> PathBuf {
        self.state_dir
            .join("worktrees")
            .join(&repo.alias)
            .join(TRAIN_DIR)
    }

    /// The issue branch name: `aif/<alias>/issue-<n>`.
    pub fn issue_branch(repo: &RepoConfig, number: u64) -> String {
        format!("aif/{}/issue-{number}", repo.alias)
    }

    /// The PR branch name: `aif/<alias>/pr-<n>`.
    pub fn pr_branch(repo: &RepoConfig, number: u64) -> String {
        format!("aif/{}/pr-{number}", repo.alias)
    }

    /// The train branch name: `aif/<alias>/train`.
    pub fn train_branch(repo: &RepoConfig) -> String {
        format!("aif/{}/train", repo.alias)
    }

    /// Create the marker directory and hide it in one repository checkout.
    pub fn prepare_checkout(&self, exec: &dyn Exec, checkout: &Path) -> Result<()> {
        self.prepare(exec, checkout)
    }

    /// Whether the issue worktree exists and git still registers it.
    ///
    /// This is the same condition [`WorktreeManager::ensure_issue`] reuses
    /// on, so callers can use it to choose between create and reuse. It is
    /// not a dispatch blocker: after a restart the gates legitimately re-open
    /// work whose worktree already exists, and that work resumes in place.
    /// A git failure writes an error to standard error and reports `false`.
    /// [`WorktreeManager::ensure_issue`] remains the authority.
    pub fn exists_issue(&self, exec: &dyn Exec, repo: &RepoConfig, number: u64) -> bool {
        let path = self.issue_path(repo, number);
        if !path.exists() {
            return false;
        }
        match self.registered(exec, &repo.path, &path) {
            Ok(registered) => registered,
            Err(error) => {
                eprintln!(
                    "cannot check the issue worktree {}: {error:#}",
                    path.display()
                );
                false
            }
        }
    }

    /// Return the worktree for one issue, and create it when missing.
    ///
    /// When the path exists and git registers it, the worktree returns as it
    /// stands, so work resumes in place. Otherwise the manager cuts the
    /// branch `aif/<alias>/issue-<n>` from the default branch and adds the
    /// worktree there. The default branch is `origin/HEAD`, resolved through
    /// `git symbolic-ref`; without it the repository's own `HEAD` is the
    /// base. When the branch already exists, the worktree is added on that
    /// branch without `-b`, so the old work survives the loss of the
    /// directory.
    pub fn ensure_issue(&self, exec: &dyn Exec, repo: &RepoConfig, number: u64) -> Result<PathBuf> {
        self.ensure_on(
            exec,
            repo,
            &self.issue_path(repo, number),
            &Self::issue_branch(repo, number),
        )
    }

    /// Return the PR worktree, and create it when missing.
    ///
    /// The worktree sits on the branch `aif/<alias>/pr-<n>`, cut from the
    /// default branch. The call then fetches the GitHub pull ref
    /// `pull/<n>/head` and resets the branch hard to it, so the worktree
    /// always holds the PR content, whatever branch the PR came from.
    /// Both commands run in the worktree: git holds `FETCH_HEAD` per
    /// worktree, so a fetch in the main checkout writes a file the worktree
    /// reset never reads.
    pub fn ensure_pr(&self, exec: &dyn Exec, repo: &RepoConfig, number: u64) -> Result<PathBuf> {
        let path = self.pr_path(repo, number);
        let worktree = self.ensure_on(exec, repo, &path, &Self::pr_branch(repo, number))?;
        let reference = format!("pull/{number}/head");
        let out = git(exec, &worktree, &["fetch", "origin", reference.as_str()])?;
        require_zero(out, "git fetch")?;
        let out = git(exec, &worktree, &["reset", "--hard", "FETCH_HEAD"])?;
        require_zero(out, "git reset")?;
        Ok(worktree)
    }

    /// Return the worktree at `path` on `branch`, and create it when missing.
    ///
    /// When the path exists and git registers it, the worktree returns as it
    /// stands, so work resumes in place. Otherwise the create path first
    /// recovers a broken previous worktree (see [`WorktreeManager::recover`]),
    /// and then cuts the branch from the default branch: `origin/HEAD`
    /// resolved through `git symbolic-ref`, else the repository's own
    /// `HEAD`. When the branch already exists, the worktree is added on that
    /// branch without `-b`, so the old work survives the loss of the
    /// directory.
    fn ensure_on(
        &self,
        exec: &dyn Exec,
        repo: &RepoConfig,
        path: &Path,
        branch: &str,
    ) -> Result<PathBuf> {
        if path.exists() && self.registered(exec, &repo.path, path)? {
            self.prepare(exec, path)?;
            return Ok(path.to_path_buf());
        }

        self.recover(exec, &repo.path, path)?;
        let path_text = path.to_string_lossy().into_owned();
        if self.branch_exists(exec, &repo.path, branch)? {
            let out = git(
                exec,
                &repo.path,
                &["worktree", "add", path_text.as_str(), branch],
            )?;
            require_zero(out, "git worktree add")?;
        } else {
            let base = self.default_base(exec, &repo.path)?;
            let out = git(
                exec,
                &repo.path,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    path_text.as_str(),
                    base.as_str(),
                ],
            )?;
            require_zero(out, "git worktree add")?;
        }
        self.prepare(exec, path)?;
        Ok(path.to_path_buf())
    }

    /// Return the train worktree, always cut from the default branch.
    ///
    /// A missing worktree is created on the branch `aif/<alias>/train` at
    /// the resolved default branch. An existing worktree is reset hard to
    /// the default branch, so every train starts clean. An existing branch
    /// without a worktree is reused and reset, so the branch also ends at
    /// the default branch. The create path recovers a broken previous
    /// worktree before it checks the branch.
    pub fn ensure_train(&self, exec: &dyn Exec, repo: &RepoConfig) -> Result<PathBuf> {
        let path = self.train_path(repo);
        let branch = Self::train_branch(repo);
        let base = self.default_base(exec, &repo.path)?;

        if path.exists() && self.registered(exec, &repo.path, &path)? {
            self.reset_to(exec, &path, &base)?;
            self.prepare(exec, &path)?;
            return Ok(path);
        }

        self.recover(exec, &repo.path, &path)?;
        let path_text = path.to_string_lossy().into_owned();
        if self.branch_exists(exec, &repo.path, &branch)? {
            let out = git(
                exec,
                &repo.path,
                &["worktree", "add", path_text.as_str(), branch.as_str()],
            )?;
            require_zero(out, "git worktree add")?;
            self.reset_to(exec, &path, &base)?;
        } else {
            let out = git(
                exec,
                &repo.path,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch.as_str(),
                    path_text.as_str(),
                    base.as_str(),
                ],
            )?;
            require_zero(out, "git worktree add")?;
        }
        self.prepare(exec, &path)?;
        Ok(path)
    }

    /// Clear a broken previous worktree before the create path adds a new
    /// one.
    ///
    /// Two states make `git worktree add` fail forever. Git can register the
    /// path while its directory is gone; `git worktree prune` drops that
    /// registration. Git can also face a directory it does not register;
    /// neither a prune nor `add -f` clears that, so the manager renames the
    /// directory to `<name>.stale-<unix millis>` in the same parent. The
    /// rename keeps uncommitted agent work, and the dot suffix keeps the
    /// doctor from parsing the sibling as an item worktree. A rename failure
    /// returns an error that names the path. The reuse path never runs this.
    fn recover(&self, exec: &dyn Exec, repo_path: &Path, path: &Path) -> Result<()> {
        let out = git(exec, repo_path, &["worktree", "prune"])?;
        require_zero(out, "git worktree prune")?;
        if !path.exists() {
            return Ok(());
        }
        let stale = stale_sibling(path, unix_millis())?;
        fs::rename(path, &stale).with_context(|| {
            format!(
                "cannot move the stale directory {} to {}",
                path.display(),
                stale.display()
            )
        })?;
        Ok(())
    }

    /// Remove the worktree of a finished issue and delete its branch.
    ///
    /// `proof` is the whole safety contract: without it the call does not
    /// compile, and no code path can delete the worktree of a live issue by
    /// accident. The removal runs `git worktree remove --force`, so leftover
    /// untracked agent scratch cannot block a proven-cleanable removal, and
    /// then deletes the branch with `git branch -D`, which also covers a
    /// closed issue whose branch never merged.
    pub fn remove_issue(
        &self,
        exec: &dyn Exec,
        repo: &RepoConfig,
        number: u64,
        proof: Cleanable,
    ) -> Result<()> {
        // Match exhaustively, so a future variant must be handled here
        // before any removal proceeds.
        match proof {
            Cleanable::MergedOrClosed => {}
        }

        self.remove_on(
            exec,
            repo,
            &self.issue_path(repo, number),
            &Self::issue_branch(repo, number),
        )
    }

    /// Remove the worktree of a finished PR and delete its branch.
    ///
    /// The proof contract matches [`WorktreeManager::remove_issue`].
    pub fn remove_pr(
        &self,
        exec: &dyn Exec,
        repo: &RepoConfig,
        number: u64,
        proof: Cleanable,
    ) -> Result<()> {
        match proof {
            Cleanable::MergedOrClosed => {}
        }
        self.remove_on(
            exec,
            repo,
            &self.pr_path(repo, number),
            &Self::pr_branch(repo, number),
        )
    }

    /// Remove the worktree at `path` and delete `branch`.
    fn remove_on(
        &self,
        exec: &dyn Exec,
        repo: &RepoConfig,
        path: &Path,
        branch: &str,
    ) -> Result<()> {
        let path_text = path.to_string_lossy().into_owned();
        let out = git(
            exec,
            &repo.path,
            &["worktree", "remove", "--force", path_text.as_str()],
        )?;
        require_zero(out, "git worktree remove")?;
        let out = git(exec, &repo.path, &["branch", "-D", branch])?;
        require_zero(out, "git branch -D")?;
        Ok(())
    }

    /// Read the agent session id marker, or `None` when it is absent.
    pub fn read_session(&self, worktree: &Path) -> Result<Option<String>> {
        read_marker(worktree, SESSION_MARKER)
    }

    /// Store the agent session id marker for this worktree.
    pub fn write_session(&self, worktree: &Path, session_id: &str) -> Result<()> {
        write_marker(worktree, SESSION_MARKER, session_id)
    }

    /// Read the Claude session id of one task.
    ///
    /// The task-specific name prevents concurrent refine tasks in one checkout
    /// from replacing each other's restart data.
    pub fn read_task_session(&self, worktree: &Path, task: &str) -> Result<Option<String>> {
        read_marker(worktree, &task_session_marker(task))
    }

    /// Store the Claude session id of one task.
    pub fn write_task_session(&self, worktree: &Path, task: &str, session_id: &str) -> Result<()> {
        write_marker(worktree, &task_session_marker(task), session_id)
    }

    /// Remove the Claude session id of one terminal task.
    pub fn remove_task_session(&self, worktree: &Path, task: &str) -> Result<()> {
        let path = worktree.join(AIF_DIR).join(task_session_marker(task));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(anyhow::Error::new(error).context(format!("cannot remove {}", path.display())))
            }
        }
    }

    /// Read the head sha of the last completed review, or `None` when absent.
    pub fn read_reviewed_sha(&self, worktree: &Path) -> Result<Option<String>> {
        read_marker(worktree, REVIEWED_SHA_MARKER)
    }

    /// Store the head sha of the last completed review for this worktree.
    pub fn write_reviewed_sha(&self, worktree: &Path, sha: &str) -> Result<()> {
        write_marker(worktree, REVIEWED_SHA_MARKER, sha)
    }

    /// Forget the last completed review of this worktree.
    ///
    /// An operator answer on a `needs-human` pull request calls this, so
    /// the fresh review of the same head is not skipped as already done.
    pub fn clear_reviewed_sha(&self, worktree: &Path) -> Result<()> {
        let path = worktree.join(AIF_DIR).join(REVIEWED_SHA_MARKER);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(anyhow::Error::new(error).context(format!("cannot remove {}", path.display())))
            }
        }
    }

    /// Whether git lists `worktree` as a registered worktree of the
    /// repository at `repo_path`. The caller ensures the path exists.
    fn registered(&self, exec: &dyn Exec, repo_path: &Path, worktree: &Path) -> Result<bool> {
        let out = git(exec, repo_path, &["worktree", "list", "--porcelain"])?;
        let out = require_zero(out, "git worktree list")?;
        let want = fs::canonicalize(worktree)
            .with_context(|| format!("cannot resolve {}", worktree.display()))?;
        let listed = out
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| fs::canonicalize(listed).is_ok_and(|resolved| resolved == want));
        Ok(listed)
    }

    /// Whether the branch `refs/heads/<branch>` exists in the repository.
    fn branch_exists(&self, exec: &dyn Exec, repo_path: &Path, branch: &str) -> Result<bool> {
        let reference = format!("refs/heads/{branch}");
        let out = git(
            exec,
            repo_path,
            &["rev-parse", "--verify", "--quiet", reference.as_str()],
        )?;
        if out.status == 0 {
            return Ok(true);
        }
        if out.status == 1 {
            return Ok(false);
        }
        require_zero(out, "git rev-parse --verify").map(|_| false)
    }

    /// Resolve the base commitish: `origin/HEAD` when git knows it, else the
    /// repository's own `HEAD`. Status 1 means that the reference is absent.
    /// Other failures propagate.
    fn default_base(&self, exec: &dyn Exec, repo_path: &Path) -> Result<String> {
        let out = git(
            exec,
            repo_path,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        )?;
        if out.status == 1 {
            return Ok("HEAD".to_string());
        }
        let out = require_zero(out, "git symbolic-ref")?;
        let resolved = out.stdout.trim();
        if resolved.is_empty() {
            bail!("git symbolic-ref returned an empty reference");
        }
        Ok(resolved.to_string())
    }

    /// Reset the worktree hard to `base`.
    fn reset_to(&self, exec: &dyn Exec, worktree: &Path, base: &str) -> Result<()> {
        let out = git(exec, worktree, &["reset", "--hard", base])?;
        require_zero(out, "git reset --hard")?;
        Ok(())
    }

    /// Create the `.aif` directory and hide it from git.
    ///
    /// Git ignores `info/exclude` from the shared git directory for every
    /// worktree, so the `.aif/` entry goes there. A per-worktree exclude is
    /// not honored by git. The entry is added once; repeated calls change
    /// nothing.
    fn prepare(&self, exec: &dyn Exec, worktree: &Path) -> Result<()> {
        let aif = worktree.join(AIF_DIR);
        fs::create_dir_all(&aif).with_context(|| format!("cannot create {}", aif.display()))?;

        let out = git(
            exec,
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let out = require_zero(out, "git rev-parse --git-common-dir")?;
        let common = PathBuf::from(out.stdout.trim());
        let info = common.join("info");
        fs::create_dir_all(&info).with_context(|| format!("cannot create {}", info.display()))?;
        let exclude = info.join("exclude");

        let mut text = match fs::read_to_string(&exclude) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("cannot read {}", exclude.display()))
                )
            }
        };
        let present = text.lines().any(|line| {
            let line = line.trim();
            line == EXCLUDE_ENTRY || line == AIF_DIR
        });
        if present {
            return Ok(());
        }
        // Keep the user's last line intact when the file lacks a newline.
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(EXCLUDE_ENTRY);
        text.push('\n');
        fs::write(&exclude, text).with_context(|| format!("cannot write {}", exclude.display()))?;
        Ok(())
    }
}

/// Run `git -C <dir> <args>` and return the raw output.
fn git(exec: &dyn Exec, dir: &Path, args: &[&str]) -> Result<CmdOut> {
    let dir_text = dir.to_string_lossy().into_owned();
    let mut argv: Vec<&str> = vec!["-C", dir_text.as_str()];
    argv.extend_from_slice(args);
    exec.run("git", &argv, None).context("git could not run")
}

/// The free sibling path that receives a stale worktree directory.
///
/// The name is `<name>.stale-<stamp>`. A second recovery of the same path
/// inside one millisecond finds that name taken, so the function appends
/// `-1`, `-2`, and so on until the path is free. A rename onto a taken
/// path would fail on a non-empty directory, or merge two stale trees.
fn stale_sibling(path: &Path, stamp: u64) -> Result<PathBuf> {
    let Some(name) = path.file_name() else {
        bail!("cannot name the stale sibling of {}", path.display());
    };
    let name = name.to_string_lossy();
    let mut candidate = path.with_file_name(format!("{name}.stale-{stamp}"));
    let mut extra = 0u32;
    while candidate.exists() {
        extra += 1;
        candidate = path.with_file_name(format!("{name}.stale-{stamp}-{extra}"));
    }
    Ok(candidate)
}

/// The current time in milliseconds since the Unix epoch.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|span| span.as_millis() as u64)
        .unwrap_or_default()
}

/// Turn a non-zero git status into an error that carries the git stderr.
fn require_zero(out: CmdOut, what: &str) -> Result<CmdOut> {
    if out.status != 0 {
        bail!("{what} failed: {}", out.stderr.trim());
    }
    Ok(out)
}

/// Read one marker file and trim it, or report `None` when it is absent.
fn read_marker(worktree: &Path, name: &str) -> Result<Option<String>> {
    let path = worktree.join(AIF_DIR).join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("cannot read {}", path.display()))),
    }
}

/// Build a collision-free file name from the bytes of one task id.
fn task_session_marker(task: &str) -> String {
    let mut name = String::from("session-");
    for byte in task.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// Write one marker file through a temporary file and a rename.
///
/// A crash can therefore never leave a half-written marker under the final
/// name, and no `.tmp` file survives a completed call. On a failed write or
/// rename, the helper removes the temporary path. The error includes a
/// cleanup failure when removal also fails.
fn write_marker(worktree: &Path, name: &str, value: &str) -> Result<()> {
    let dir = worktree.join(AIF_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let target = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    if let Err(e) = fs::write(&temp, format!("{value}\n")) {
        let error = anyhow::Error::new(e).context(format!("cannot write {}", temp.display()));
        return Err(clean_marker_temp(&temp, error));
    }
    if let Err(e) = fs::rename(&temp, &target) {
        let error = anyhow::Error::new(e)
            .context(format!("cannot move the marker to {}", target.display()));
        return Err(clean_marker_temp(&temp, error));
    }
    Ok(())
}

/// Remove a failed marker write and keep a cleanup error in the error chain.
fn clean_marker_temp(temp: &Path, error: anyhow::Error) -> anyhow::Error {
    let cleanup = match fs::remove_file(temp) {
        Ok(()) => return error,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return error,
        Err(e) if e.kind() == io::ErrorKind::IsADirectory => fs::remove_dir(temp),
        Err(e) => Err(e),
    };
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "cannot clean the temporary marker {}: {cleanup_error}",
            temp.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReleasePolicy;
    use crate::exec::ScriptExec;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique temporary directory for one test.
    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-task7-{}-{}-{}",
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

    /// A `RepoConfig` for the test repository, with the alias `demo`.
    fn demo_repo(path: &Path) -> RepoConfig {
        RepoConfig {
            alias: "demo".to_string(),
            path: path.to_path_buf(),
            owner_repo: "owner/demo".to_string(),
            lanes: BTreeMap::new(),
            release: ReleasePolicy::Manual,
            theory: crate::config::TheoryConfig::default(),
            role_overrides: BTreeMap::new(),
        }
    }

    // --- Marker files. ---

    #[test]
    fn markers_round_trip_and_leave_no_temporary_file() {
        let root = temp_root("markers");
        let worktree = root.join("wt");
        fs::create_dir_all(&worktree).expect("the worktree dir must be creatable");
        let manager = WorktreeManager::new(root.clone());

        assert_eq!(manager.read_session(&worktree).unwrap(), None);
        assert_eq!(manager.read_reviewed_sha(&worktree).unwrap(), None);

        manager.write_session(&worktree, "sess-abc").unwrap();
        manager.write_reviewed_sha(&worktree, "cafe123").unwrap();
        assert_eq!(
            manager.read_session(&worktree).unwrap().as_deref(),
            Some("sess-abc")
        );
        assert_eq!(
            manager.read_reviewed_sha(&worktree).unwrap().as_deref(),
            Some("cafe123")
        );

        manager.write_session(&worktree, "sess-2").unwrap();
        assert_eq!(
            manager.read_session(&worktree).unwrap().as_deref(),
            Some("sess-2")
        );

        let mut names: Vec<String> = fs::read_dir(worktree.join(".aif"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["reviewed-sha".to_string(), "session".to_string()]
        );
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn marker_write_failure_preserves_the_marker_and_removes_the_temporary_path() {
        let root = temp_root("marker-failure");
        let worktree = root.join("wt");
        let marker_dir = worktree.join(AIF_DIR);
        fs::create_dir_all(&marker_dir).expect("the marker dir must be creatable");
        let manager = WorktreeManager::new(root.clone());
        manager.write_session(&worktree, "session-old").unwrap();

        let temp = marker_dir.join(format!("{SESSION_MARKER}.tmp"));
        fs::create_dir(&temp).expect("the temporary dir must be creatable");

        let error = manager
            .write_session(&worktree, "session-new")
            .expect_err("the temporary dir must cause a write failure");

        assert!(error.to_string().contains("cannot write"));
        assert_eq!(
            manager.read_session(&worktree).unwrap().as_deref(),
            Some("session-old")
        );
        assert!(!temp.exists(), "the helper must remove the temporary path");
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn marker_write_reports_a_temporary_cleanup_failure() {
        let root = temp_root("marker-cleanup-failure");
        let worktree = root.join("wt");
        let temp = worktree.join(AIF_DIR).join(format!("{SESSION_MARKER}.tmp"));
        fs::create_dir_all(&temp).expect("the temporary dir must be creatable");
        fs::write(temp.join("blocker"), "x").expect("the blocker write must succeed");
        let manager = WorktreeManager::new(root.clone());

        let error = manager
            .write_session(&worktree, "session-new")
            .expect_err("the nonempty temporary dir must cause a failure");
        let error_chain = format!("{error:#}");

        assert!(error_chain.contains("cannot write"));
        assert!(error_chain.contains("cannot clean"));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn branch_lookup_propagates_an_unexpected_git_failure() {
        let root = temp_root("branch-error");
        let repo_path = root.join("repo");
        let repo_text = repo_path.to_string_lossy().into_owned();
        let exec = ScriptExec::new().expect(
            move |call| {
                call.program == "git"
                    && call.argv()
                        == [
                            "-C",
                            repo_text.as_str(),
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            "refs/heads/aif/demo/issue-7",
                        ]
            },
            CmdOut {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: repository is unavailable\n".to_string(),
            },
        );
        let manager = WorktreeManager::new(root.clone());

        let error = manager
            .branch_exists(&exec, &repo_path, "aif/demo/issue-7")
            .expect_err("an unexpected git status must be an error");

        assert!(error.to_string().contains("git rev-parse --verify failed"));
        assert!(error.to_string().contains("repository is unavailable"));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn default_base_propagates_an_unexpected_git_failure() {
        let root = temp_root("base-error");
        let repo_path = root.join("repo");
        let exec = ScriptExec::new().expect(
            |call| call.program == "git" && call.args.iter().any(|arg| arg == "symbolic-ref"),
            CmdOut {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: repository is unavailable\n".to_string(),
            },
        );
        let manager = WorktreeManager::new(root.clone());

        let error = manager
            .default_base(&exec, &repo_path)
            .expect_err("an unexpected git status must be an error");

        assert!(error.to_string().contains("git symbolic-ref failed"));
        assert!(error.to_string().contains("repository is unavailable"));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn default_base_rejects_an_empty_reference() {
        let root = temp_root("base-empty");
        let repo_path = root.join("repo");
        let exec = ScriptExec::new().expect(
            |call| call.program == "git" && call.args.iter().any(|arg| arg == "symbolic-ref"),
            CmdOut::ok("\n"),
        );
        let manager = WorktreeManager::new(root.clone());

        let error = manager
            .default_base(&exec, &repo_path)
            .expect_err("an empty reference must be an error");

        assert!(error.to_string().contains("empty reference"));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn default_base_falls_back_to_head_when_origin_head_is_missing() {
        let root = temp_root("base-fallback");
        let repo_path = root.join("repo");
        let exec = ScriptExec::new().expect(
            |call| call.program == "git" && call.args.iter().any(|arg| arg == "symbolic-ref"),
            CmdOut {
                status: 1,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let manager = WorktreeManager::new(root.clone());

        assert_eq!(manager.default_base(&exec, &repo_path).unwrap(), "HEAD");
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn prepare_checkout_hides_root_markers_without_starting_git() {
        let root = temp_root("prepare-checkout");
        let checkout = root.join("repo");
        let common = root.join("common-git");
        let checkout_text = checkout.to_string_lossy().into_owned();
        let common_text = common.to_string_lossy().into_owned();
        let exec = ScriptExec::new().expect(
            move |call| {
                call.program == "git"
                    && call.argv()
                        == [
                            "-C",
                            checkout_text.as_str(),
                            "rev-parse",
                            "--path-format=absolute",
                            "--git-common-dir",
                        ]
            },
            CmdOut::ok(format!("{common_text}\n")),
        );
        let manager = WorktreeManager::new(root.join("state"));

        manager.prepare_checkout(&exec, &checkout).unwrap();

        assert!(checkout.join(".aif").is_dir());
        assert_eq!(
            fs::read_to_string(common.join("info/exclude")).unwrap(),
            ".aif/\n"
        );
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    // --- Argument construction, through ScriptExec. ---

    #[test]
    fn ensure_issue_builds_the_documented_commands() {
        let root = temp_root("argv-issue");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.issue_path(&repo, 7);
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let wt_gitdir = wt.join(".git").display().to_string();
        let prune_repo = repo_text.clone();
        let verify_repo = repo_text.clone();
        let symref_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", prune_repo.as_str(), "worktree", "prune"]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                verify_repo.as_str(),
                                "rev-parse",
                                "--verify",
                                "--quiet",
                                "refs/heads/aif/demo/issue-7",
                            ]
                },
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                symref_repo.as_str(),
                                "symbolic-ref",
                                "--quiet",
                                "refs/remotes/origin/HEAD",
                            ]
                },
                CmdOut::ok("refs/remotes/origin/main\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                add_repo.as_str(),
                                "worktree",
                                "add",
                                "-b",
                                "aif/demo/issue-7",
                                add_wt.as_str(),
                                "refs/remotes/origin/main",
                            ]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            );

        let path = manager.ensure_issue(&exec, &repo, 7).unwrap();

        assert_eq!(path, wt);
        assert_eq!(exec.calls().len(), 5);
        let exclude = fs::read_to_string(wt.join(".git").join("info").join("exclude"))
            .expect("the exclude file must exist");
        assert!(exclude.contains(".aif/"));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn ensure_pr_builds_the_documented_commands() {
        let root = temp_root("argv-pr");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.pr_path(&repo, 7);
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let wt_gitdir = wt.join(".git").display().to_string();
        let prune_repo = repo_text.clone();
        let verify_repo = repo_text.clone();
        let symref_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let fetch_wt = wt_text.clone();
        let reset_wt = wt_text.clone();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", prune_repo.as_str(), "worktree", "prune"]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                verify_repo.as_str(),
                                "rev-parse",
                                "--verify",
                                "--quiet",
                                "refs/heads/aif/demo/pr-7",
                            ]
                },
                CmdOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                symref_repo.as_str(),
                                "symbolic-ref",
                                "--quiet",
                                "refs/remotes/origin/HEAD",
                            ]
                },
                CmdOut::ok("refs/remotes/origin/main\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                add_repo.as_str(),
                                "worktree",
                                "add",
                                "-b",
                                "aif/demo/pr-7",
                                add_wt.as_str(),
                                "refs/remotes/origin/main",
                            ]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", fetch_wt.as_str(), "fetch", "origin", "pull/7/head"]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", reset_wt.as_str(), "reset", "--hard", "FETCH_HEAD"]
                },
                CmdOut::ok(""),
            );

        let path = manager.ensure_pr(&exec, &repo, 7).unwrap();

        assert_eq!(path, wt);
        assert_eq!(exec.calls().len(), 7);
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn ensure_issue_adds_an_existing_branch_without_b() {
        let root = temp_root("argv-branch");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.issue_path(&repo, 7);
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let wt_gitdir = wt.join(".git").display().to_string();
        let prune_repo = repo_text.clone();
        let verify_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", prune_repo.as_str(), "worktree", "prune"]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                verify_repo.as_str(),
                                "rev-parse",
                                "--verify",
                                "--quiet",
                                "refs/heads/aif/demo/issue-7",
                            ]
                },
                CmdOut::ok("abc\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                add_repo.as_str(),
                                "worktree",
                                "add",
                                add_wt.as_str(),
                                "aif/demo/issue-7",
                            ]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            );

        manager.ensure_issue(&exec, &repo, 7).unwrap();

        let calls = exec.calls();
        assert_eq!(
            calls.len(),
            4,
            "no symbolic-ref call when the branch exists"
        );
        assert!(calls
            .iter()
            .all(|c| !c.args.contains(&"symbolic-ref".to_string())));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn ensure_issue_reuses_a_registered_worktree_through_two_commands_only() {
        let root = temp_root("argv-reuse-issue");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.issue_path(&repo, 7);
        fs::create_dir_all(&wt).expect("the worktree dir must be creatable");
        let wt_canon = wt.canonicalize().unwrap().display().to_string();
        let list = format!("worktree {wt_canon}\n");
        let list_repo = repo_path.to_string_lossy().into_owned();
        let gitdir_wt = wt.to_string_lossy().into_owned();
        let wt_gitdir = wt.join(".git").display().to_string();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", list_repo.as_str(), "worktree", "list", "--porcelain"]
                },
                CmdOut::ok(list),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            );

        let path = manager.ensure_issue(&exec, &repo, 7).unwrap();

        assert_eq!(path, wt);
        assert_eq!(
            exec.calls().len(),
            2,
            "the reuse path runs no prune and no add"
        );
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn ensure_issue_renames_an_unregistered_directory_and_adds_the_worktree() {
        let root = temp_root("stale-dir");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.issue_path(&repo, 7);
        fs::create_dir_all(&wt).expect("the worktree dir must be creatable");
        fs::write(wt.join("agent-scratch.txt"), "uncommitted work\n")
            .expect("the scratch file must be writable");
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let list_repo = repo_text.clone();
        let prune_repo = repo_text.clone();
        let verify_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let wt_gitdir = wt.join(".git").display().to_string();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", list_repo.as_str(), "worktree", "list", "--porcelain"]
                },
                CmdOut::ok("worktree /somewhere-else\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", prune_repo.as_str(), "worktree", "prune"]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                verify_repo.as_str(),
                                "rev-parse",
                                "--verify",
                                "--quiet",
                                "refs/heads/aif/demo/issue-7",
                            ]
                },
                CmdOut::ok("abc\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                add_repo.as_str(),
                                "worktree",
                                "add",
                                add_wt.as_str(),
                                "aif/demo/issue-7",
                            ]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            );

        let path = manager.ensure_issue(&exec, &repo, 7).unwrap();

        assert_eq!(path, wt);
        assert!(
            path.is_dir(),
            "the added worktree sits at the original path"
        );
        let parent = wt.parent().expect("the worktree path must have a parent");
        let stale: Vec<PathBuf> = fs::read_dir(parent)
            .expect("the parent must be readable")
            .map(|entry| entry.expect("the entry must be readable").path())
            .filter(|sibling| {
                sibling
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with("issue-7.stale-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(stale.len(), 1, "exactly one stale sibling");
        assert_eq!(
            fs::read_to_string(stale[0].join("agent-scratch.txt")).unwrap(),
            "uncommitted work\n",
            "the renamed directory keeps its content"
        );
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn stale_sibling_skips_a_name_that_exists() {
        let root = temp_root("stale-sibling");
        let path = root.join("issue-7");
        fs::create_dir_all(&path).expect("the worktree dir must be creatable");
        fs::create_dir_all(root.join("issue-7.stale-42")).expect("the taken dir must be creatable");
        fs::create_dir_all(root.join("issue-7.stale-42-1"))
            .expect("the second taken dir must be creatable");

        let free = stale_sibling(&path, 42).unwrap();

        assert_eq!(free, root.join("issue-7.stale-42-2"));
        assert_eq!(
            stale_sibling(&root.join("issue-8"), 42).unwrap(),
            root.join("issue-8.stale-42"),
            "a free name takes no counter"
        );
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }
    #[test]
    fn ensure_train_resets_an_existing_worktree_through_the_documented_commands() {
        let root = temp_root("argv-train");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.train_path(&repo);
        fs::create_dir_all(&wt).expect("the worktree dir must be creatable");
        let wt_canon = wt.canonicalize().unwrap().display().to_string();
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let wt_gitdir = wt.join(".git").display().to_string();
        let list = format!("worktree {wt_canon}\nHEAD abc\nbranch refs/heads/aif/demo/train\n\n");
        let list_repo = repo_text.clone();
        let reset_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                repo_text.as_str(),
                                "symbolic-ref",
                                "--quiet",
                                "refs/remotes/origin/HEAD",
                            ]
                },
                CmdOut::ok("refs/remotes/origin/main\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv() == ["-C", list_repo.as_str(), "worktree", "list", "--porcelain"]
                },
                CmdOut::ok(list),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                reset_wt.as_str(),
                                "reset",
                                "--hard",
                                "refs/remotes/origin/main",
                            ]
                },
                CmdOut::ok("HEAD is now at abc\n"),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                gitdir_wt.as_str(),
                                "rev-parse",
                                "--path-format=absolute",
                                "--git-common-dir",
                            ]
                },
                CmdOut::ok(format!("{wt_gitdir}\n")),
            );

        let path = manager.ensure_train(&exec, &repo).unwrap();

        assert_eq!(path, wt);
        assert_eq!(exec.calls().len(), 4);
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn remove_issue_runs_remove_then_branch_delete() {
        let root = temp_root("argv-remove");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let wt = manager.issue_path(&repo, 9);
        let wt_text = wt.to_string_lossy().into_owned();
        let repo_text = repo_path.to_string_lossy().into_owned();
        let remove_repo = repo_text.clone();
        let delete_repo = repo_text;
        let exec = ScriptExec::new()
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                remove_repo.as_str(),
                                "worktree",
                                "remove",
                                "--force",
                                wt_text.as_str(),
                            ]
                },
                CmdOut::ok(""),
            )
            .expect(
                move |c| {
                    c.program == "git"
                        && c.argv()
                            == [
                                "-C",
                                delete_repo.as_str(),
                                "branch",
                                "-D",
                                "aif/demo/issue-9",
                            ]
                },
                CmdOut::ok(""),
            );

        manager
            .remove_issue(&exec, &repo, 9, Cleanable::MergedOrClosed)
            .expect("the merged path must succeed");

        assert_eq!(exec.calls().len(), 2);
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }

    #[test]
    fn exists_issue_reports_registered_worktrees_only() {
        let root = temp_root("exists");
        let repo_path = root.join("repo");
        let repo = demo_repo(&repo_path);
        let manager = WorktreeManager::new(root.join("state"));
        let repo_text = repo_path.to_string_lossy().into_owned();
        let list_repo = repo_text.clone();

        // A missing path reports false and runs no git at all.
        assert!(!manager.exists_issue(&ScriptExec::new(), &repo, 7));

        // A registered worktree reports true.
        let wt = manager.issue_path(&repo, 7);
        fs::create_dir_all(&wt).expect("the worktree dir must be creatable");
        let wt_canon = wt.canonicalize().unwrap().display().to_string();
        let exec = ScriptExec::new().expect(
            move |c| {
                c.program == "git"
                    && c.argv() == ["-C", list_repo.as_str(), "worktree", "list", "--porcelain"]
            },
            CmdOut::ok(format!("worktree {wt_canon}\n")),
        );
        assert!(manager.exists_issue(&exec, &repo, 7));

        // An unregistered directory reports false.
        let exec = ScriptExec::new().expect(
            move |c| {
                c.program == "git"
                    && c.argv() == ["-C", repo_text.as_str(), "worktree", "list", "--porcelain"]
            },
            CmdOut::ok("worktree /somewhere-else\n"),
        );
        assert!(!manager.exists_issue(&exec, &repo, 7));
        fs::remove_dir_all(&root).expect("the temp dir must be removable");
    }
}
