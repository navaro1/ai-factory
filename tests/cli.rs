//! Verifies the command-line interface without external tools, daemons, or
//! network access.
//!
//! The doctor report runs real tools, and the TUI path starts a daemon, so
//! only the pure clap surface and the no-daemon stop path run here. The
//! doctor logic itself runs in `src/doctor.rs` tests against a scripted
//! executor.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
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
    assert!(stdout(&output).contains("--paused"));
}

#[test]
fn aif_reports_the_0_6_0_version() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["--version"]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "aif 0.6.0\n");
}

#[test]
fn aif_doctor_help_lists_the_clean_options() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["doctor", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("--config <CONFIG>"));
    assert!(stdout(&output).contains("--clean"));
    assert!(stdout(&output).contains("--yes"));
    assert!(!stdout(&output).contains("--paused"));
}

#[test]
fn aif_stop_help_does_not_offer_the_start_paused_flag() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["stop", "--help"]);

    assert!(output.status.success());
    assert!(!stdout(&output).contains("--paused"));
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
fn aif_paused_refuses_an_existing_daemon_before_it_starts_the_tui() {
    let dir = temp_dir("paused-existing");
    let socket_dir = dir.join("aif");
    fs::create_dir_all(&socket_dir).expect("the socket directory must be creatable");
    let listener = UnixListener::bind(socket_dir.join("daemon.sock"))
        .expect("the fake daemon must bind the socket");

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["--paused"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("a daemon already runs"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("--paused") && stderr(&output).contains("aif stop"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("cannot enable raw mode"),
        "the command must stop before it starts the terminal UI: {}",
        stderr(&output)
    );

    drop(listener);
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn aif_paused_forms_pass_the_flag_to_the_daemon_start_command() {
    let dir = temp_dir("paused-start-argv");
    let bin_dir = dir.join("bin");
    let runtime_dir = dir.join("runtime");
    let marker = dir.join("systemd-argv");
    fs::create_dir_all(&bin_dir).expect("the fake binary directory must be creatable");
    fs::create_dir_all(&runtime_dir).expect("the runtime directory must be creatable");
    let systemd_run = bin_dir.join("systemd-run");
    fs::write(
        &systemd_run,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$AIF_TEST_MARKER\"\nexit 42\n",
    )
    .expect("the fake systemd-run must be writable");
    fs::set_permissions(&systemd_run, fs::Permissions::from_mode(0o755))
        .expect("the fake systemd-run must be executable");

    for arguments in [&["--paused"][..], &["tui", "--paused"][..]] {
        let output = run_with_env(
            env!("CARGO_BIN_EXE_aif"),
            arguments,
            &[
                ("AIF_TEST_MARKER", &marker),
                ("PATH", &bin_dir),
                ("XDG_RUNTIME_DIR", &runtime_dir),
            ],
        );

        assert_eq!(output.status.code(), Some(1));
        let argv = fs::read_to_string(&marker).expect("the start arguments must be readable");
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(
            &lines[lines.len() - 2..],
            ["run", "--paused"],
            "arguments: {lines:?}"
        );
    }

    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn aifd_run_help_lists_the_config_option() {
    let output = run(env!("CARGO_BIN_EXE_aifd"), &["run", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("--config <CONFIG>"));
    assert!(stdout(&output).contains("--paused"));
}

#[test]
fn aifd_run_with_a_missing_config_fails_and_names_the_file() {
    let output = run(
        env!("CARGO_BIN_EXE_aifd"),
        &["run", "--config", "/test/factory.toml"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no config file at /test/factory.toml"),
        "stderr: {}",
        stderr(&output)
    );
}
