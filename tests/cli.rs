//! Verifies the command-line interface without external tools, daemons, or
//! network access.
//!
//! The doctor report runs real tools, and the TUI path starts a daemon, so
//! only the pure clap surface and the no-daemon stop path run here. The
//! doctor logic itself runs in `src/doctor.rs` tests against a scripted
//! executor.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(binary: &str, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(arguments)
        .output()
        .expect("the test binary must run")
}

fn run_with_env(binary: &str, arguments: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(binary);
    command.args(arguments);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("the test binary must run")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("the test output must use UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("the test output must use UTF-8")
}

/// A unique temporary directory for one test.
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aif-task17-cli-{label}-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the old temp dir must be removable");
    }
    fs::create_dir_all(&dir).expect("the temp dir must be creatable");
    dir
}

#[test]
fn aif_help_lists_all_subcommands() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("\n  tui "));
    assert!(stdout(&output).contains("\n  stop "));
    assert!(stdout(&output).contains("\n  doctor "));
}

#[test]
fn aif_doctor_help_lists_the_clean_options() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["doctor", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("--config <CONFIG>"));
    assert!(stdout(&output).contains("--clean"));
    assert!(stdout(&output).contains("--yes"));
}

#[test]
fn aif_stop_without_a_daemon_fails_with_a_clear_message() {
    let dir = temp_dir("no-daemon");
    let result = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["stop"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );

    assert_eq!(result.status.code(), Some(1));
    assert!(
        stderr(&result).contains("no daemon is listening"),
        "stderr: {}",
        stderr(&result)
    );
    assert!(
        stderr(&result).contains("daemon.sock"),
        "stderr: {}",
        stderr(&result)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn aifd_run_help_lists_the_config_option() {
    let output = run(env!("CARGO_BIN_EXE_aifd"), &["run", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("--config <CONFIG>"));
}

#[test]
fn aifd_run_accepts_a_config_path() {
    let output = run(
        env!("CARGO_BIN_EXE_aifd"),
        &["run", "--config", "/test/factory.toml"],
    );

    assert!(output.status.success());
    assert_eq!(stdout(&output), "aifd: not implemented yet\n");
}
