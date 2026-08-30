//! The one indirection for every external command, so no test ever runs a
//! real tool. Production code uses [`RealExec`]; tests use [`ScriptExec`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use anyhow::{anyhow, Context};

/// The result of one finished external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOut {
    /// The exit status code. `-1` means the child died from a signal.
    pub status: i32,
    /// Everything the child wrote to stdout, lossily decoded as UTF-8.
    pub stdout: String,
    /// Everything the child wrote to stderr, lossily decoded as UTF-8.
    pub stderr: String,
}

impl CmdOut {
    /// A successful output that printed `stdout` and nothing else.
    pub fn ok(stdout: impl Into<String>) -> Self {
        CmdOut {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
}

/// The ability to run an external command.
///
/// Every module that would shell out takes `&dyn Exec` instead, so a test can
/// hand it a [`ScriptExec`] and never run a real tool.
pub trait Exec: Send + Sync {
    /// Run `program` with the argument vector `args`, optionally in `cwd`.
    ///
    /// The command never runs through a shell. A status other than zero is
    /// not an error here; the caller decides what a non-zero status means.
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<CmdOut>;
}

/// The production [`Exec`]: it spawns the real child process.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealExec;

impl Exec for RealExec {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<CmdOut> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let output = command
            .output()
            .with_context(|| format!("{} failed to start", describe(program, args)))?;
        Ok(CmdOut {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// One recorded call to a [`ScriptExec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The program the caller asked to run.
    pub program: String,
    /// The exact argument vector the caller passed.
    pub args: Vec<String>,
    /// The working directory the caller passed, if any.
    pub cwd: Option<PathBuf>,
}

impl Call {
    /// The argument vector as string slices, for assertions.
    pub fn argv(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }
}

type Matcher = Box<dyn Fn(&Call) -> bool + Send + Sync>;

struct Step {
    matches: Matcher,
    out: CmdOut,
}

/// The test double for [`Exec`].
///
/// Build it with scripted `(matcher, output)` steps. Each call must match the
/// next step. The executor records every call. [`ScriptExec::calls`] returns
/// the calls for exact argument vector checks. An unexpected call returns an
/// error.
pub struct ScriptExec {
    steps: Mutex<Vec<Step>>,
    calls: Mutex<Vec<Call>>,
}

impl ScriptExec {
    /// An empty script that rejects every call.
    pub fn new() -> Self {
        Self {
            steps: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Script one step: when a call satisfies `matches`, return `out`.
    pub fn expect(
        self,
        matches: impl Fn(&Call) -> bool + Send + Sync + 'static,
        out: CmdOut,
    ) -> Self {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Step {
                matches: Box::new(matches),
                out,
            });
        self
    }

    /// Every recorded call, in call order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Default for ScriptExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec for ScriptExec {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<CmdOut> {
        let call = Call {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(Path::to_path_buf),
        };
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call.clone());

        let mut steps = self.steps.lock().unwrap_or_else(PoisonError::into_inner);
        let next_matches = steps.first().is_some_and(|step| (step.matches)(&call));
        if next_matches {
            Ok(steps.remove(0).out)
        } else {
            Err(anyhow!(
                "unexpected command: {}; {} scripted steps remain",
                describe(&call.program, &call.argv()),
                steps.len()
            ))
        }
    }
}

/// Render a program and its arguments for an error message.
fn describe(program: &str, args: &[&str]) -> String {
    let mut text = String::from(program);
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_exec_reports_a_missing_program_as_an_error() {
        let out = RealExec.run("aif-no-such-tool-in-tests", &["--version"], None);
        assert!(out.is_err());
    }

    #[test]
    fn script_returns_steps_in_order() {
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "git" && call.argv() == ["status", "--porcelain"],
                CmdOut::ok("M file\n"),
            )
            .expect(
                |call| call.program == "gh" && call.argv() == ["api", "x"],
                CmdOut {
                    status: 0,
                    stdout: "[]\n".into(),
                    stderr: String::new(),
                },
            );

        let git = exec
            .run("git", &["status", "--porcelain"], Some(Path::new("/tmp")))
            .unwrap();
        assert_eq!(git.stdout, "M file\n");
        let gh = exec.run("gh", &["api", "x"], None).unwrap();
        assert_eq!(gh.stdout, "[]\n");
    }

    #[test]
    fn script_records_every_call_with_its_exact_argument_vector() {
        let exec = ScriptExec::new()
            .expect(
                |call| {
                    call.program == "git"
                        && call.argv() == ["-C", "/repo", "remote", "get-url", "origin"]
                },
                CmdOut::ok(""),
            )
            .expect(
                |call| {
                    call.program == "git"
                        && call.argv() == ["worktree", "list"]
                        && call.cwd == Some(PathBuf::from("/repo"))
                },
                CmdOut::ok(""),
            );
        exec.run("git", &["-C", "/repo", "remote", "get-url", "origin"], None)
            .unwrap();
        exec.run("git", &["worktree", "list"], Some(Path::new("/repo")))
            .unwrap();

        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "git");
        assert_eq!(
            calls[0].argv(),
            ["-C", "/repo", "remote", "get-url", "origin"]
        );
        assert_eq!(calls[0].cwd, None);
        assert_eq!(calls[1].argv(), ["worktree", "list"]);
        assert_eq!(calls[1].cwd, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn script_fails_on_an_unmatched_command() {
        let exec = ScriptExec::new().expect(|call| call.program == "git", CmdOut::ok(""));
        let err = exec.run("claude", &["-p"], None).unwrap_err();
        assert!(err.to_string().contains("unexpected command"));
        assert!(err.to_string().contains("claude -p"));
    }

    #[test]
    fn script_fails_once_its_steps_are_used_up() {
        let exec = ScriptExec::new().expect(|call| call.program == "gh", CmdOut::ok("one\n"));
        assert_eq!(exec.run("gh", &[], None).unwrap().stdout, "one\n");
        let err = exec.run("gh", &[], None).unwrap_err();
        assert!(err.to_string().contains("unexpected command"));
    }
}
