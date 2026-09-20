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
    assert!(stdout(&output).contains("--audit <ALIAS>"));
    assert!(!stdout(&output).contains("--paused"));
}

#[test]
fn aif_doctor_audit_without_a_daemon_fails_with_a_clear_message() {
    let dir = temp_dir("no-daemon-audit");
    let result = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["doctor", "--audit", "borsuk"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );

    assert_eq!(result.status.code(), Some(1));
    assert!(
        stderr(&result).contains("no daemon is listening"),
        "stderr: {}",
        stderr(&result)
    );
}

#[test]
fn aif_stop_help_does_not_offer_the_start_paused_flag() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["stop", "--help"]);

    assert!(output.status.success());
    assert!(!stdout(&output).contains("--paused"));
}

#[test]
fn measure_lever_help_lists_the_area_option_and_the_root_row() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &["measure", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("--area <AREA>"));

    let output = run(env!("CARGO_BIN_EXE_aif"), &["--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("\n  measure "));
}

/// A fake daemon that reads one measure action and answers one push.
///
/// The helper binds the socket of `dir` before it returns, and the
/// thread reads the action line and answers `reply` with the request of
/// the action. `None` closes the stream without a reply. The thread
/// gives up after a bounded wait, so a client that never connects fails
/// the test instead of hanging it.
fn measure_lever_fake_daemon(dir: &Path, reply: Option<String>) -> std::thread::JoinHandle<()> {
    use std::time::{Duration, Instant};

    let socket_dir = dir.join("aif");
    fs::create_dir_all(&socket_dir).expect("the socket directory must be creatable");
    let listener = UnixListener::bind(socket_dir.join("daemon.sock"))
        .expect("the fake daemon must bind the socket");
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        listener
            .set_nonblocking(true)
            .expect("the fake listener must turn non-blocking");
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "the client never connected to the fake daemon"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("the fake daemon must accept one client: {error}"),
            }
        };
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .expect("the fake daemon must read the action line");
        let Some(template) = reply else {
            return;
        };
        let action: serde_json::Value =
            serde_json::from_str(line.trim()).expect("the action line must parse");
        let request = action["request"]
            .as_str()
            .expect("the action must carry one request")
            .to_string();
        let text = template.replace("__REQUEST__", &request);
        stream
            .write_all(text.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .expect("the fake daemon must write the reply");
    })
}

#[test]
fn measure_lever_prints_the_table_and_exits_0_on_a_pass() {
    let dir = temp_dir("measure-pass");
    let daemon = measure_lever_fake_daemon(
        &dir,
        Some(
            concat!(
                "{\"type\":\"measure_result\",\"request\":\"__REQUEST__\",",
                "\"text\":\"AREA web-checkout\\npoll_p95  12 \\u2192 12  ms  unchanged\",",
                "\"pass\":true}",
            )
            .to_string(),
        ),
    );

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["measure"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );
    daemon.join().expect("the fake daemon must finish");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout(&output).contains("AREA web-checkout"),
        "stdout: {}",
        stdout(&output)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn measure_lever_exits_1_on_a_failed_verdict() {
    let dir = temp_dir("measure-fail");
    let daemon = measure_lever_fake_daemon(
        &dir,
        Some(
            concat!(
                "{\"type\":\"measure_result\",\"request\":\"__REQUEST__\",",
                "\"text\":\"AREA web-checkout\\npoll_p95  12 \\u2192 14  ms  worsened\",",
                "\"pass\":false}",
            )
            .to_string(),
        ),
    );

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["measure"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );
    daemon.join().expect("the fake daemon must finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).contains("worsened"),
        "stdout: {}",
        stdout(&output)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn measure_lever_exits_3_on_an_error_reply() {
    let dir = temp_dir("measure-error");
    let daemon = measure_lever_fake_daemon(
        &dir,
        Some(
            concat!(
                "{\"type\":\"measure_result\",\"request\":\"__REQUEST__\",",
                "\"text\":\"error: the governor of borsuk is off\",",
                "\"pass\":false}",
            )
            .to_string(),
        ),
    );

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["measure"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );
    daemon.join().expect("the fake daemon must finish");

    assert_eq!(output.status.code(), Some(3));
    assert!(
        stdout(&output).contains("error: the governor of borsuk is off"),
        "stdout: {}",
        stdout(&output)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn measure_lever_exits_3_when_the_stream_closes_before_the_reply() {
    let dir = temp_dir("measure-closed");
    let daemon = measure_lever_fake_daemon(&dir, None);

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["measure"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );
    daemon.join().expect("the fake daemon must finish");

    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output).contains("closed the connection"),
        "stderr: {}",
        stderr(&output)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}

#[test]
fn measure_lever_exits_3_when_no_daemon_listens() {
    let dir = temp_dir("measure-no-daemon");

    let output = run_with_env(
        env!("CARGO_BIN_EXE_aif"),
        &["measure"],
        &[("XDG_RUNTIME_DIR", &dir)],
    );

    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output).contains("no daemon is listening"),
        "stderr: {}",
        stderr(&output)
    );
    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
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

/// A git checkout with an `origin` remote, for the daemon start.
fn git_checkout(dir: &Path) -> PathBuf {
    let repo = dir.join("borsuk");
    for arguments in [
        vec!["init", "-q", repo.to_str().unwrap()],
        vec![
            "-C",
            repo.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "git@github.com:acme/borsuk.git",
        ],
    ] {
        let outcome = Command::new("git")
            .args(&arguments)
            .output()
            .expect("git must run");
        assert!(
            outcome.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
    }
    repo
}

/// Wait until the path exists, or panic after the timeout.
fn wait_for(path: &Path, timeout: std::time::Duration, what: &str) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timeout while waiting for {what}: {}", path.display());
}

#[test]
fn a_sigterm_stops_the_daemon_cleanly_and_saves_the_paused_runtime() {
    let dir = temp_dir("sigterm");
    let config_home = dir.join("config");
    let state_home = dir.join("state");
    let runtime_dir = dir.join("runtime");
    let bin_dir = dir.join("bin");
    for path in [&config_home, &state_home, &runtime_dir, &bin_dir] {
        fs::create_dir_all(path).expect("the test directories must be creatable");
    }
    let stderr_log = dir.join("aifd.stderr");
    let repo = git_checkout(&dir);

    // A stub `gh` first on PATH keeps the poll offline.
    let gh = bin_dir.join("gh");
    fs::write(&gh, "#!/bin/sh\nexit 0\n").expect("the gh stub must be writable");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))
        .expect("the gh stub must be executable");

    let config_dir = config_home.join("aif");
    fs::create_dir_all(&config_dir).expect("the config directory must be creatable");
    let config_path = config_dir.join("factory.toml");
    fs::write(
        &config_path,
        format!(
            "schema_version = 1\n\
             \n[stage.refine]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 2\n\
             \n[stage.implement]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 1\n\
             \n[stage.review]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 2\n\
             \n[stage.release]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 1\n\
             \n[ticket.create]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[ticket.chat]\nharness = \"claude\"\nmodel = \"m\"\n\
             permission_mode = \"manual\"\npermission_handler = \"inbox\"\n\
             tools = [\"Read\", \"Glob\", \"Grep\"]\n\
             \n[repo.borsuk]\npath = \"{}\"\n",
            repo.display()
        ),
    )
    .expect("the config must be writable");

    let socket = runtime_dir.join("aif").join("daemon.sock");
    let state_path = state_home.join("aif").join("state.json");
    let stderr_file = fs::File::create(&stderr_log).expect("the stderr log must be creatable");
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifd"))
        .args(["run", "--config", config_path.to_str().unwrap(), "--paused"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("the daemon must start");

    wait_for(
        &socket,
        std::time::Duration::from_secs(30),
        "the daemon socket",
    );
    let stop = Command::new("kill")
        .args(["-TERM", child.id().to_string().as_str()])
        .status()
        .expect("kill must run");
    assert!(stop.success(), "the SIGTERM delivery must succeed");

    let status = child.wait().expect("the daemon must be waitable");
    assert_eq!(
        status.code(),
        Some(0),
        "the daemon must exit with code 0; see {}",
        stderr_log.display()
    );
    assert!(
        !socket.exists(),
        "the socket file must be gone after the exit"
    );
    let state = fs::read_to_string(&state_path).expect("the forced state write must exist");
    assert!(
        state.contains("\"runtime\":{\"paused\":{\"global\":true"),
        "the state file must hold the runtime pause marks: {state}"
    );

    fs::remove_dir_all(&dir).expect("the temp dir must be removable");
}
