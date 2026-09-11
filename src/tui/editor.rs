//! Runs the operator's editor from the terminal UI.
//!
//! The UI owns raw mode and the alternate screen. An external editor needs
//! the real terminal, so the bridge restores the terminal before the editor
//! starts and re-enables the UI terminal afterwards through a guard that
//! runs on every path.

use std::fs;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use crossterm::execute;
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};

use super::{enable_terminal_with, restore_terminal};

/// The result of one editor run over one file.
#[derive(Debug, PartialEq, Eq)]
pub enum EditorOutcome {
    /// The editor exited with success and the file bytes changed.
    Saved,
    /// The editor exited with success and the file bytes did not change.
    Unchanged,
    /// The editor did not run to success. The string holds the reason.
    Failed(String),
}

/// Edit `path` in the operator's editor from the live terminal UI.
///
/// Uses `$EDITOR` split on whitespace, with `vi` as the fallback.
pub fn edit_file(path: &Path) -> Result<EditorOutcome> {
    let value = std::env::var("EDITOR").ok();
    edit_file_with(
        path,
        &editor_command(value.as_deref()),
        restore_terminal,
        || {
            enable_terminal_with(
                enable_raw_mode,
                || execute!(stdout(), EnterAlternateScreen),
                restore_terminal,
            )
        },
    )
}

/// Run `editor` over `path` with the real terminal handed to the editor.
///
/// The call reads the file bytes, restores the terminal with `restore`,
/// starts the editor with inherited standard streams, and re-enables the UI
/// terminal with `enable` through a guard that runs on every path. The
/// outcome compares the file bytes before and after the editor run.
pub fn edit_file_with(
    path: &Path,
    editor: &[String],
    restore: impl FnOnce() -> Result<()>,
    enable: impl FnOnce() -> Result<()>,
) -> Result<EditorOutcome> {
    let Some((program, args)) = editor.split_first() else {
        return Err(anyhow!("the editor command is empty"));
    };
    let before = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    restore()?;
    let mut guard = EnableGuard {
        enable: Some(enable),
    };
    let status = Command::new(program).args(args).arg(path).status();
    let outcome = match status {
        Err(error) => EditorOutcome::Failed(format!("cannot run the editor {program}: {error:#}")),
        Ok(status) if !status.success() => {
            EditorOutcome::Failed(format!("the editor exited with {status}"))
        }
        Ok(_) => {
            let after =
                fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
            if after == before {
                EditorOutcome::Unchanged
            } else {
                EditorOutcome::Saved
            }
        }
    };
    guard.run_enable()?;
    Ok(outcome)
}

/// The scratch directory for editor templates: `<state_dir>/edit/`.
///
/// Creates the directory when it does not exist.
pub fn edit_dir() -> Result<PathBuf> {
    let dir = crate::config::state_dir().join("edit");
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// The editor command: `value` split on whitespace, else `vi`.
///
/// `value` is the `$EDITOR` setting. `None` and a blank value both fall
/// back to `vi`, so the caller owns the environment read.
fn editor_command(value: Option<&str>) -> Vec<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.split_whitespace().map(str::to_string).collect())
        .unwrap_or_else(|| vec!["vi".to_string()])
}

/// Calls `enable` once, on drop when the caller never reached the call.
struct EnableGuard<F: FnOnce() -> Result<()>> {
    enable: Option<F>,
}

impl<F: FnOnce() -> Result<()>> EnableGuard<F> {
    /// Run the stored enable step and report its result.
    fn run_enable(&mut self) -> Result<()> {
        match self.enable.take() {
            Some(enable) => enable(),
            None => Ok(()),
        }
    }
}

impl<F: FnOnce() -> Result<()>> Drop for EnableGuard<F> {
    fn drop(&mut self) {
        let _ = self.run_enable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;
    use std::thread;
    use std::time::Duration;

    /// The shared record of the restore and enable calls.
    type Order = Rc<RefCell<Vec<&'static str>>>;

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-editor-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        drop(file);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// A restore closure that records its call.
    fn record_restore(order: &Order) -> impl FnOnce() -> Result<()> {
        let order = order.clone();
        move || {
            order.borrow_mut().push("restore");
            Ok(())
        }
    }

    /// An enable closure that records its call.
    fn record_enable(order: &Order) -> impl FnOnce() -> Result<()> {
        let order = order.clone();
        move || {
            order.borrow_mut().push("enable");
            Ok(())
        }
    }

    /// Run the bridge with the fake editor script in `body`.
    ///
    /// The test writes its fake editor and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file and fail with `Text file busy` for a few
    /// microseconds. Production never executes a file it just wrote, so the
    /// retry lives in this helper and not in the bridge.
    fn run_script_editor(
        dir: &Path,
        target: &Path,
        body: &str,
        order: &Order,
    ) -> Result<EditorOutcome> {
        let editor = script(dir, "editor", body);
        let editor = vec![editor.to_string_lossy().into_owned()];
        for _ in 0..100 {
            order.borrow_mut().clear();
            match edit_file_with(target, &editor, record_restore(order), record_enable(order)) {
                Ok(EditorOutcome::Failed(reason)) if reason.contains("Text file busy") => {
                    thread::sleep(Duration::from_millis(10));
                }
                outcome => return outcome,
            }
        }
        panic!("the fake editor did not start after 100 attempts");
    }

    #[test]
    fn a_saving_editor_reports_saved_and_the_guard_order() {
        let dir = temp_dir("saved");
        let file = dir.join("note.md");
        fs::write(&file, "before\n").unwrap();
        let order: Order = Rc::new(RefCell::new(Vec::new()));
        let outcome =
            run_script_editor(&dir, &file, "#!/bin/sh\necho after > \"$1\"\n", &order).unwrap();
        assert_eq!(outcome, EditorOutcome::Saved);
        assert_eq!(*order.borrow(), vec!["restore", "enable"]);
    }

    #[test]
    fn a_failing_editor_reports_failed_and_the_guard_order() {
        let dir = temp_dir("failed");
        let file = dir.join("note.md");
        fs::write(&file, "before\n").unwrap();
        let order: Order = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_script_editor(&dir, &file, "#!/bin/sh\nexit 1\n", &order).unwrap();
        assert!(matches!(outcome, EditorOutcome::Failed(reason) if reason.contains("exit")));
        assert_eq!(*order.borrow(), vec!["restore", "enable"]);
    }

    #[test]
    fn an_unchanged_file_reports_unchanged() {
        let dir = temp_dir("unchanged");
        let file = dir.join("note.md");
        fs::write(&file, "same\n").unwrap();
        let order: Order = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_script_editor(&dir, &file, "#!/bin/sh\ntrue\n", &order).unwrap();
        assert_eq!(outcome, EditorOutcome::Unchanged);
        assert_eq!(*order.borrow(), vec!["restore", "enable"]);
    }

    #[test]
    fn a_missing_editor_reports_failed_with_the_reason() {
        let dir = temp_dir("missing");
        let file = dir.join("note.md");
        fs::write(&file, "before\n").unwrap();
        let order: Order = Rc::new(RefCell::new(Vec::new()));
        let editor = vec![dir
            .join("aif-no-such-editor")
            .to_string_lossy()
            .into_owned()];
        let outcome = edit_file_with(
            &file,
            &editor,
            record_restore(&order),
            record_enable(&order),
        )
        .unwrap();
        assert!(
            matches!(outcome, EditorOutcome::Failed(reason) if reason.contains("aif-no-such-editor"))
        );
        assert_eq!(*order.borrow(), vec!["restore", "enable"]);
    }

    #[test]
    fn the_editor_command_splits_the_value_and_falls_back_to_vi() {
        assert_eq!(
            editor_command(Some("code --wait")),
            vec!["code".to_string(), "--wait".to_string()]
        );
        assert_eq!(editor_command(Some("   ")), vec!["vi".to_string()]);
        assert_eq!(editor_command(None), vec!["vi".to_string()]);
    }

    #[test]
    fn edit_dir_creates_the_scratch_directory() {
        let state = std::env::temp_dir().join(format!("aif-editor-state-{}", std::process::id()));
        fs::remove_dir_all(&state).ok();
        let previous = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", &state);
        let first = edit_dir();
        let second = edit_dir();
        match previous {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let dir = first.unwrap();
        assert_eq!(dir, state.join("aif").join("edit"));
        assert!(dir.is_dir());
        assert_eq!(second.unwrap(), dir);
    }
}
