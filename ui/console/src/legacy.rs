use anyhow::{bail, Result};

use crate::status;

pub fn start_v3(skip: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let root = status::repo_root()?;
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let session = format!("{repo}-factory");

    for line in crate::zellij::list_sessions()? {
        if line.split_whitespace().next() == Some(session.as_str()) {
            if line.contains("EXITED") {
                eprintln!("aif: session {session} is dead; deleting it and starting fresh");
                let out = std::process::Command::new("zellij")
                    .args(["delete-session", &session])
                    .output()?;
                if !out.status.success() {
                    bail!("failed to delete dead session {session}");
                }
            } else {
                eprintln!("aif: session {session} already runs; attaching");
                let err = std::process::Command::new("zellij")
                    .args(["attach", &session])
                    .exec();
                bail!("failed to attach: {err}");
            }
        }
    }
    let skip_items: Vec<String> = skip
        .map(|raw| raw.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let zdir = std::env::var("ZELLIJ_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    std::path::PathBuf::from(home).join(".config")
                })
                .join("zellij")
        });
    let template = crate::layout::installed_template()?;
    let rendered =
        crate::layout::render(&template, &root, &repo, &zdir.join("prompts"), &skip_items)?;

    if !root.join(crate::graph::DEFAULT_GRAPH_PATH).exists() {
        eprintln!(
            "aif: no {} found; codex panes do not loop on their own - \
             run `aif graph init` and `aif run` for the 30m dispatch clock",
            crate::graph::DEFAULT_GRAPH_PATH
        );
    }

    let state_base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".local").join("state")
        });
    let registry = state_base.join("aif").join("registry").join(&session);
    let _ = std::fs::remove_dir_all(&registry);

    let layout_file =
        std::env::temp_dir().join(format!("aif-{session}-{}.kdl", std::process::id()));
    std::fs::write(&layout_file, &rendered)?;

    let layout_arg = layout_file.display().to_string();
    let isolated = std::process::Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    let err = if isolated {
        let memory_high = std::env::var("AIF_MEMORY_HIGH").unwrap_or_else(|_| "3G".into());
        eprintln!("aif: starting {session} in its own user scope aif-{session}");
        let mut cmd = std::process::Command::new("systemd-run");
        cmd.args(["--user", "--scope"])
            .arg(format!("--unit=aif-{session}"));
        if memory_high != "0" && !memory_high.eq_ignore_ascii_case("off") {
            cmd.arg(format!("--property=MemoryHigh={memory_high}"));
        }
        cmd.args([
            "zellij",
            "--new-session-with-layout",
            &layout_arg,
            "--session",
            &session,
        ]);
        cmd.exec()
    } else {
        std::process::Command::new("zellij")
            .arg("--new-session-with-layout")
            .arg(&layout_file)
            .arg("--session")
            .arg(&session)
            .exec()
    };
    bail!("failed to start zellij: {err}");
}
