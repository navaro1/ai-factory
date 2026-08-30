//! Verifies the command-line scaffold without external tools or network access.

use std::process::{Command, Output};

fn run(binary: &str, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(arguments)
        .output()
        .expect("the test binary must run")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("the test output must use UTF-8")
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
fn aif_without_a_subcommand_starts_the_tui_placeholder() {
    let output = run(env!("CARGO_BIN_EXE_aif"), &[]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "aif tui: not implemented yet\n");
}

#[test]
fn aif_subcommands_print_placeholders() {
    for command in ["tui", "doctor"] {
        let output = run(env!("CARGO_BIN_EXE_aif"), &[command]);

        assert!(output.status.success(), "{command} failed");
        assert!(
            stdout(&output).contains("not implemented yet"),
            "{command} did not print a placeholder"
        );
    }
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
