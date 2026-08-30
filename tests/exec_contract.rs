//! Verifies that external crate tests can use the scripted command executor.

use std::path::{Path, PathBuf};

use aif::exec::{CmdOut, Exec, ScriptExec};

#[test]
fn script_exec_enforces_order_and_records_calls_outside_the_crate() {
    let exec = ScriptExec::new()
        .expect(
            |call| call.program == "git" && call.argv() == ["status", "--short"],
            CmdOut::ok("clean\n"),
        )
        .expect(
            |call| call.program == "gh" && call.argv() == ["api", "repos/o/r"],
            CmdOut::ok("{}\n"),
        );

    let error = exec
        .run("gh", &["api", "repos/o/r"], None)
        .expect_err("a command must not skip the first script step");
    assert!(error.to_string().contains("unexpected command"));

    let first = exec
        .run("git", &["status", "--short"], Some(Path::new("/repo")))
        .expect("the first script step must match");
    let second = exec
        .run("gh", &["api", "repos/o/r"], None)
        .expect("the second script step must match");
    assert_eq!(first.stdout, "clean\n");
    assert_eq!(second.stdout, "{}\n");

    let calls = exec.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].program, "gh");
    assert_eq!(calls[0].argv(), ["api", "repos/o/r"]);
    assert_eq!(calls[1].program, "git");
    assert_eq!(calls[1].argv(), ["status", "--short"]);
    assert_eq!(calls[1].cwd, Some(PathBuf::from("/repo")));
    assert_eq!(calls[2].program, "gh");
    assert_eq!(calls[2].argv(), ["api", "repos/o/r"]);
}
