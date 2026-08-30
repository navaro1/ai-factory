use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ids;

#[derive(Debug, Clone)]
pub struct FactoryPaths {
    pub factory_id: String,
    pub root: PathBuf,
    pub state: PathBuf,
    pub runtime: PathBuf,
}

fn xdg_state() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local").join("state")
        })
}

fn xdg_runtime() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let uid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
    PathBuf::from(format!("/tmp/aif-{uid}"))
}

pub fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .context("failed to run git rev-parse")?;
    if !out.status.success() {
        anyhow::bail!("not a git repository: {}", root.display());
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let joined = if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    joined
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", joined.display()))
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

pub fn ensure_factory_id(root: &Path) -> Result<String> {
    let common = git_common_dir(root)?;
    let dir = common.join("aif");
    fs::create_dir_all(&dir)?;
    let file = dir.join("factory-id");
    if let Ok(raw) = fs::read_to_string(&file) {
        let id = raw.trim().to_owned();
        if id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(id);
        }
    }
    let id = ids::rand_hex(8)?;
    fs::write(&file, format!("{id}\n"))?;
    Ok(id)
}

impl FactoryPaths {
    pub fn open(root: &Path) -> Result<Self> {
        let factory_id = ensure_factory_id(root)?;
        Ok(Self::from_id(root, &factory_id))
    }

    pub fn from_id(root: &Path, factory_id: &str) -> Self {
        FactoryPaths {
            factory_id: factory_id.to_owned(),
            root: root.to_path_buf(),
            state: xdg_state().join("aif").join("factories").join(factory_id),
            runtime: xdg_runtime().join("aif").join(&factory_id[..8]),
        }
    }

    pub fn short_id(&self) -> &str {
        &self.factory_id[..8]
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.state)?;
        set_mode(&self.state, 0o700)?;
        fs::create_dir_all(self.state.join("logs"))?;
        fs::create_dir_all(self.state.join("worktrees"))?;
        fs::create_dir_all(&self.runtime)?;
        set_mode(&self.runtime, 0o700)?;
        Ok(())
    }

    pub fn journal(&self) -> PathBuf {
        self.state.join("journal.jsonl")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }

    pub fn trust(&self) -> PathBuf {
        self.state.join("trust.json")
    }

    pub fn meta(&self) -> PathBuf {
        self.state.join("meta.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state.join("logs")
    }

    pub fn task_log(&self, task: &str) -> PathBuf {
        self.logs_dir().join(format!("{}.log", ids::sanitize_component(task)))
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.state.join("worktrees")
    }

    pub fn trusted(&self) -> bool {
        fs::read_to_string(self.trust())
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("granted").and_then(|g| g.as_bool()))
            .unwrap_or(false)
    }

    pub fn write_trust(&self, granted: bool) -> Result<()> {
        self.ensure()?;
        let body = serde_json::json!({
            "granted": granted,
            "at": ids::now_iso(),
            "root": self.root.display().to_string(),
        });
        fs::write(self.trust(), serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }

    pub fn write_meta(&self, repo: &str) -> Result<()> {
        self.ensure()?;
        let body = serde_json::json!({
            "factory_id": self.factory_id,
            "root": self.root.display().to_string(),
            "repo": repo,
            "version": env!("CARGO_PKG_VERSION"),
        });
        fs::write(self.meta(), serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }
}

pub fn list_factories() -> Vec<(String, PathBuf)> {
    let base = xdg_state().join("aif").join("factories");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() == 16 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                out.push((name, entry.path()));
            }
        }
    }
    out.sort();
    out
}

pub fn socket_alive(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}
