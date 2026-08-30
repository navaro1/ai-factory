//! Per-issue git worktrees and the marker files that carry stage state.
//!
//! One worktree per issue lives under `<state_dir>/worktrees/<repo>/` and is
//! reused across the stages of that issue. Marker files in the worktree's
//! `.aif` directory carry the agent session id and the last reviewed head
//! sha, so a daemon restart resumes work in place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
        self.state_dir
            .join("worktrees")
            .join(&repo.alias)
            .join(format!("issue-{number}"))
    }

    /// The train worktree path: `<state_dir>/worktrees/<alias>/train`.
    pub fn train_path(&self, repo: &RepoConfig) -> PathBuf {
        self.state_dir
            .join("worktrees")
            .join(&repo.alias)
            .join("train")
    }

    /// The issue branch name: `aif/<alias>/issue-<n>`.
    pub fn issue_branch(repo: &RepoConfig, number: u64) -> String {
        format!("aif/{}/issue-{number}", repo.alias)
    }

    /// The train branch name: `aif/<alias>/train`.
    pub fn train_branch(repo: &RepoConfig) -> String {
        format!("aif/{}/train", repo.alias)
    }

    /// Whether the issue worktree exists and git still registers it.
    ///
    /// This is the same condition [`WorktreeManager::ensure_issue`] reuses
    /// on, so callers can use it to choose between create and reuse. It is
    /// not a dispatch blocker: after a restart the gates legitimately re-open
    /// work whose worktree already exists, and that work resumes in place.
    /// A git failure reports `false`; [`WorktreeManager::ensure_issue`]
    /// remains the authority.
    pub fn exists_issue(&self, exec: &dyn Exec, repo: &RepoConfig, number: u64) -> bool {
        let path = self.issue_path(repo, number);
        if !path.exists() {
            return false;
        }
        self.registered(exec, &repo.path, &path).unwrap_or(false)
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
        let path = self.issue_path(repo, number);
        if path.exists() && self.registered(exec, &repo.path, &path)? {
            self.prepare(exec, &path)?;
            return Ok(path);
        }

        let branch = Self::issue_branch(repo, number);
        let path_text = path.to_string_lossy().into_owned();
        if self.branch_exists(exec, &repo.path, &branch)? {
            let out = git(
                exec,
                &repo.path,
                &["worktree", "add", path_text.as_str(), branch.as_str()],
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

    /// Return the train worktree, always cut from the default branch.
    ///
    /// A missing worktree is created on the branch `aif/<alias>/train` at
    /// the resolved default branch. An existing worktree is reset hard to
    /// the default branch, so every train starts clean. An existing branch
    /// without a worktree is reused and reset, so the branch also ends at
    /// the default branch.
    pub fn ensure_train(&self, exec: &dyn Exec, repo: &RepoConfig) -> Result<PathBuf> {
        let path = self.train_path(repo);
        let branch = Self::train_branch(repo);
        let base = self.default_base(exec, &repo.path)?;

        if path.exists() && self.registered(exec, &repo.path, &path)? {
            self.reset_to(exec, &path, &base)?;
            self.prepare(exec, &path)?;
            return Ok(path);
        }

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

        let path = self.issue_path(repo, number);
        let branch = Self::issue_branch(repo, number);
        let path_text = path.to_string_lossy().into_owned();
        let out = git(
            exec,
            &repo.path,
            &["worktree", "remove", "--force", path_text.as_str()],
        )?;
        require_zero(out, "git worktree remove")?;
        let out = git(exec, &repo.path, &["branch", "-D", branch.as_str()])?;
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

    /// Read the head sha of the last completed review, or `None` when absent.
    pub fn read_reviewed_sha(&self, worktree: &Path) -> Result<Option<String>> {
        read_marker(worktree, REVIEWED_SHA_MARKER)
    }

    /// Store the head sha of the last completed review for this worktree.
    pub fn write_reviewed_sha(&self, worktree: &Path, sha: &str) -> Result<()> {
        write_marker(worktree, REVIEWED_SHA_MARKER, sha)
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
        Ok(out.status == 0)
    }

    /// Resolve the base commitish: `origin/HEAD` when git knows it, else the
    /// repository's own `HEAD`. A non-zero `symbolic-ref` is the designed
    /// fallback, not a swallowed error.
    fn default_base(&self, exec: &dyn Exec, repo_path: &Path) -> Result<String> {
        let out = git(
            exec,
            repo_path,
            &["symbolic-ref", "refs/remotes/origin/HEAD"],
        )?;
        if out.status == 0 {
            let resolved = out.stdout.trim();
            if !resolved.is_empty() {
                return Ok(resolved.to_string());
            }
        }
        Ok("HEAD".to_string())
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

/// Write one marker file through a temporary file and a rename.
///
/// A crash can therefore never leave a half-written marker under the final
/// name, and no `.tmp` file survives a completed call. On a failed write or
/// rename the temporary file is removed on a best-effort basis, and the
/// original error is the one that propagates.
fn write_marker(worktree: &Path, name: &str, value: &str) -> Result<()> {
    let dir = worktree.join(AIF_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let target = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    if let Err(e) = fs::write(&temp, format!("{value}\n")) {
        let _ = fs::remove_file(&temp);
        return Err(anyhow::Error::new(e).context(format!("cannot write {}", temp.display())));
    }
    if let Err(e) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(anyhow::Error::new(e)
            .context(format!("cannot move the marker to {}", target.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReleasePolicy;
    use crate::exec::{RealExec, ScriptExec};
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

    /// A real local git repository, built with plain file transports only.
    struct TestRepo {
        root: PathBuf,
        repo: PathBuf,
        state: PathBuf,
    }

    impl TestRepo {
        /// A repository with a local bare `origin` whose HEAD resolves to
        /// `refs/remotes/origin/main`.
        fn with_origin(label: &str) -> Self {
            Self::build(label, true)
        }

        /// A repository with no remote, so base resolution must fall back.
        fn plain(label: &str) -> Self {
            Self::build(label, false)
        }

        fn build(label: &str, with_origin: bool) -> Self {
            let root = temp_root(label);
            let repo = root.join("repo");
            let state = root.join("state");
            let t = TestRepo {
                root: root.clone(),
                repo: repo.clone(),
                state,
            };
            t.ok(&root, &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
            t.ok(&repo, &["config", "user.email", "aif-test@example.com"]);
            t.ok(&repo, &["config", "user.name", "Aif Tests"]);
            fs::write(repo.join("README.md"), "# test\n").expect("the readme write must succeed");
            t.ok(&repo, &["add", "-A"]);
            t.ok(&repo, &["commit", "-q", "-m", "init"]);
            if with_origin {
                let origin = root.join("origin.git");
                t.ok(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
                t.ok(
                    &repo,
                    &["remote", "add", "origin", origin.to_str().unwrap()],
                );
                t.ok(&repo, &["push", "-q", "origin", "main"]);
                t.ok(&repo, &["fetch", "-q", "origin"]);
                t.ok(&repo, &["remote", "set-head", "origin", "main"]);
            }
            t
        }

        /// Run git in `dir` and return the raw output.
        fn git(&self, dir: &Path, args: &[&str]) -> CmdOut {
            RealExec
                .run("git", args, Some(dir))
                .expect("the test git command must start")
        }

        /// Run git in `dir` and demand success.
        fn ok(&self, dir: &Path, args: &[&str]) -> CmdOut {
            let out = self.git(dir, args);
            assert_eq!(
                out.status,
                0,
                "git {} failed: {}",
                args.join(" "),
                out.stderr
            );
            out
        }

        /// The full sha of `commitish` in `dir`.
        fn sha(&self, dir: &Path, commitish: &str) -> String {
            self.ok(dir, &["rev-parse", commitish])
                .stdout
                .trim()
                .to_string()
        }

        /// A manager over this repository's test state directory.
        fn manager(&self) -> WorktreeManager {
            WorktreeManager::new(self.state.clone())
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// A `RepoConfig` for the test repository, with the alias `demo`.
    fn demo_repo(path: &Path) -> RepoConfig {
        RepoConfig {
            alias: "demo".to_string(),
            path: path.to_path_buf(),
            owner_repo: "owner/demo".to_string(),
            lanes: BTreeMap::new(),
            release: ReleasePolicy::Manual,
        }
    }

    // --- Real git behaviour. ---

    #[test]
    fn ensure_issue_creates_a_worktree_at_the_documented_path() {
        let t = TestRepo::with_origin("create");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let path = manager
            .ensure_issue(&RealExec, &repo, 7)
            .expect("ensure_issue must succeed");

        assert_eq!(path, t.state.join("worktrees").join("demo").join("issue-7"));
        assert!(path.is_dir());
        assert!(path.join(".aif").is_dir());
        let head = t.sha(&path, "HEAD");
        assert_eq!(head, t.sha(&t.repo, "refs/remotes/origin/main"));
        assert_eq!(
            head,
            t.sha(&t.repo, "refs/heads/aif/demo/issue-7"),
            "the new branch must point at the base commit"
        );
        assert!(manager.exists_issue(&RealExec, &repo, 7));
    }

    #[test]
    fn ensure_issue_twice_reuses_in_place() {
        let t = TestRepo::with_origin("reuse");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let first = manager.ensure_issue(&RealExec, &repo, 7).unwrap();
        manager.write_session(&first, "sess-1").unwrap();
        fs::write(first.join("notes.txt"), "agent scratch").expect("the write must succeed");
        t.ok(&first, &["add", "-A"]);
        t.ok(&first, &["commit", "-q", "-m", "wip"]);
        let head = t.sha(&first, "HEAD");

        let second = manager.ensure_issue(&RealExec, &repo, 7).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            t.sha(&second, "HEAD"),
            head,
            "reuse must keep the work in place"
        );
        assert_eq!(
            manager.read_session(&second).unwrap().as_deref(),
            Some("sess-1")
        );
    }

    #[test]
    fn ensure_issue_reuses_a_branch_whose_worktree_was_removed() {
        let t = TestRepo::with_origin("recut");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let path = manager.ensure_issue(&RealExec, &repo, 7).unwrap();
        fs::write(path.join("notes.txt"), "work").expect("the write must succeed");
        t.ok(&path, &["add", "-A"]);
        t.ok(&path, &["commit", "-q", "-m", "wip"]);
        let head = t.sha(&path, "HEAD");
        t.ok(&t.repo, &["worktree", "remove", path.to_str().unwrap()]);
        assert!(!manager.exists_issue(&RealExec, &repo, 7));

        let again = manager.ensure_issue(&RealExec, &repo, 7).unwrap();

        assert_eq!(again, path);
        assert_eq!(
            t.sha(&again, "HEAD"),
            head,
            "the surviving branch must come back, not a fresh cut"
        );
    }

    #[test]
    fn ensure_issue_falls_back_to_head_without_origin() {
        let t = TestRepo::plain("fallback");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let path = manager.ensure_issue(&RealExec, &repo, 3).unwrap();

        assert_eq!(t.sha(&path, "HEAD"), t.sha(&t.repo, "HEAD"));
    }

    #[test]
    fn ensure_train_creates_and_resets_to_the_default_branch() {
        let t = TestRepo::with_origin("train");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);
        let base = t.sha(&t.repo, "refs/remotes/origin/main");

        let path = manager.ensure_train(&RealExec, &repo).unwrap();
        assert_eq!(path, t.state.join("worktrees").join("demo").join("train"));
        assert_eq!(t.sha(&path, "HEAD"), base);

        fs::write(path.join("train.txt"), "x").expect("the write must succeed");
        t.ok(&path, &["add", "-A"]);
        t.ok(&path, &["commit", "-q", "-m", "train wip"]);

        let again = manager.ensure_train(&RealExec, &repo).unwrap();
        assert_eq!(again, path);
        assert_eq!(
            t.sha(&again, "HEAD"),
            base,
            "reuse must reset the train to the default branch"
        );
        assert!(!again.join("train.txt").exists());
    }

    #[test]
    fn remove_issue_with_proof_removes_the_worktree_and_the_branch() {
        let t = TestRepo::with_origin("remove");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let path = manager.ensure_issue(&RealExec, &repo, 9).unwrap();
        manager.write_session(&path, "sess-9").unwrap();
        manager
            .remove_issue(&RealExec, &repo, 9, Cleanable::MergedOrClosed)
            .expect("the merged path must succeed");

        assert!(!path.exists());
        assert!(
            t.git(
                &t.repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/aif/demo/issue-9"
                ]
            )
            .status
                != 0,
            "the branch must be gone"
        );
        assert!(!manager.exists_issue(&RealExec, &repo, 9));
    }

    #[test]
    fn the_aif_directory_is_invisible_to_git() {
        let t = TestRepo::with_origin("exclude");
        let manager = t.manager();
        let repo = demo_repo(&t.repo);

        let path = manager.ensure_issue(&RealExec, &repo, 5).unwrap();
        manager.write_session(&path, "sess-5").unwrap();
        fs::write(path.join("loose.txt"), "untracked").expect("the write must succeed");

        let status = t.ok(&path, &["status", "--porcelain"]).stdout;
        assert_eq!(status, "?? loose.txt\n");
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
        let verify_repo = repo_text.clone();
        let symref_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let exec = ScriptExec::new()
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
        assert_eq!(exec.calls().len(), 4);
        let exclude = fs::read_to_string(wt.join(".git").join("info").join("exclude"))
            .expect("the exclude file must exist");
        assert!(exclude.contains(".aif/"));
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
        let verify_repo = repo_text.clone();
        let add_repo = repo_text.clone();
        let add_wt = wt_text.clone();
        let gitdir_wt = wt_text.clone();
        let exec = ScriptExec::new()
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
            3,
            "no symbolic-ref call when the branch exists"
        );
        assert!(calls
            .iter()
            .all(|c| !c.args.contains(&"symbolic-ref".to_string())));
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
