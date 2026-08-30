use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::graph::Graph;

pub fn one_item_prompt(node: &str, kind: &str) -> String {
    match (node, kind) {
        ("refiner", _) => format!(
            "Refine GitHub {kind} {{{{gh_ticket_no}}}} only.\n\n\
             1. Read the {kind} with `gh`.\n\
             2. Rewrite its description as an implementation plan for a mid-level developer.\n\
             3. Ask the owner questions only when the {kind} lacks critical information.\n\
             4. List which parts can run in parallel.\n\
             5. Add the `refined` label and remove the `to-refine` label.\n"
        ),
        ("implementer", _) => format!(
            "Implement GitHub {kind} {{{{gh_ticket_no}}}} only.\n\n\
             1. Verify the ticket dependencies are met.\n\
             2. Work inside the worktree provided by the factory; do not create another one.\n\
             3. Implement, test, and commit.\n\
             4. Open a draft PR that mentions the ticket and the worktree location.\n"
        ),
        ("reviewer", "pr") => "{gh_ticket_no}\n\n0. Review this pull request only.\n1. Check quality, ticket coverage, and simpler alternatives.\n2. Post the review on the PR.\n3. Remove the draft flag when the review passes.\n".to_owned(),
        ("releaser", "pr") => "{gh_ticket_no}\n\n0. Release this pull request only.\n1. Confirm the review passed and tests are green.\n2. Merge the PR and clean up the branch.\n3. Deploy or tag according to the repository process.\n".to_owned(),
        (other, _) => format!("Work on GitHub {kind} {{{{gh_ticket_no}}}} ({other}).\n"),
    }
}

pub fn v4_graph_text(graph: &Graph, auto_workers: bool) -> String {
    let mut out = String::from("graph version=4 {\n");
    out.push_str(&format!("    limit {}\n", graph.limit));
    for node in &graph.nodes {
        let exec = if auto_workers
            && node.agent != crate::graph::Agent::Claude
            && node.exec == crate::graph::Exec::Supervised
        {
            "auto"
        } else {
            node.exec.as_str()
        };
        let retrigger = if auto_workers
            && node.agent != crate::graph::Agent::Claude
            && node
                .when
                .as_ref()
                .map(|w| w.render().contains("draft"))
                .unwrap_or(false)
            && node.retrigger == crate::graph::Retrigger::Gate
        {
            " retrigger=\"head-sha\""
        } else {
            ""
        };
        out.push_str(&format!(
            "\n    node \"{}\" limit=1{retrigger} {{\n",
            node.name
        ));
        out.push_str(&format!("        agent \"{}\"\n", node.agent.as_str()));
        out.push_str(&format!("        model \"{}\"\n", node.model));
        out.push_str(&format!("        exec \"{exec}\"\n"));
        if let Some(when) = &node.when {
            out.push_str(&format!("        when \"{}\"\n", when.render()));
        }
        if node.when.is_some() {
            out.push_str(&format!(
                "        prompt \".aif/prompts/{}.md\"\n",
                node.name
            ));
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    out
}

pub struct MigrationPlan {
    pub graph_text: String,
    pub prompts: Vec<(String, String)>,
    pub changes: Vec<String>,
}

pub fn plan(graph: &Graph, auto_workers: bool) -> MigrationPlan {
    let graph_text = v4_graph_text(graph, auto_workers);
    let mut prompts = Vec::new();
    let mut changes = vec!["graph: version=4, per-node limit 1, repo-local prompts".to_owned()];
    if auto_workers {
        changes.push(
            "exec: codex and opencode workers switch to auto (claude stays supervised)".into(),
        );
        changes.push("reviewer: retrigger on head-sha changes".into());
    }
    for node in &graph.nodes {
        if node.when.is_some() {
            let kind = match node
                .when
                .as_ref()
                .map(|w| w.render().contains("pr"))
                .unwrap_or(false)
            {
                true => "pr",
                false => "issue",
            };
            prompts.push((node.name.clone(), one_item_prompt(&node.name, kind)));
        }
    }
    if !prompts.is_empty() {
        changes.push(format!(
            "prompts: {} one-item prompt files under .aif/prompts",
            prompts.len()
        ));
    }
    MigrationPlan {
        graph_text,
        prompts,
        changes,
    }
}

pub fn legacy_session_running(repo: &str) -> bool {
    let legacy = format!("{repo}-factory");
    crate::zellij::list_sessions()
        .map(|lines| {
            lines.iter().any(|line| {
                line.split_whitespace().next() == Some(legacy.as_str()) && !line.contains("EXITED")
            })
        })
        .unwrap_or(false)
}

pub fn migrate(root: &Path, write: bool, auto_workers: bool) -> Result<()> {
    let graph_path = root.join(crate::graph::DEFAULT_GRAPH_PATH);
    let graph = Graph::load(&graph_path)?;
    if graph.version >= 4 {
        bail!("graph is already version 4");
    }
    let plan = plan(&graph, auto_workers);
    println!("proposed migration:");
    for change in &plan.changes {
        println!("  - {change}");
    }
    if !write {
        println!("\npreview only; rerun with --write to apply");
        return Ok(());
    }
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    if legacy_session_running(&repo) {
        bail!(
            "legacy zellij session {repo}-factory is running; stop it before migrating \
             (`zellij delete-session {repo}-factory`)"
        );
    }
    let backup = root.join(".aif/graph.kdl.v3.bak");
    if !backup.exists() {
        std::fs::copy(&graph_path, &backup)
            .with_context(|| format!("backup to {}", backup.display()))?;
        println!("backup: {}", backup.display());
    }
    std::fs::write(&graph_path, &plan.graph_text)?;
    let prompts_dir = root.join(".aif/prompts");
    std::fs::create_dir_all(&prompts_dir)?;
    for (name, body) in &plan.prompts {
        let path = prompts_dir.join(format!("{name}.md"));
        std::fs::write(&path, body)?;
        println!("prompt: {}", path.display());
    }
    Graph::load(&graph_path)?;
    println!("migration written");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3() -> Graph {
        Graph::parse(
            r#"
graph {
    tick "30m"
    limit 3
    node "planner" {
        agent "claude"
        model "claude-fable-5[1m]"
        exec "supervised"
    }
    node "refiner" {
        agent "codex"
        model "gpt-5.6-sol"
        exec "supervised"
        when "issue has label 'to-refine'"
        prompt "zellij/prompts/refiner.md"
    }
    node "reviewer" {
        agent "codex"
        model "gpt-5.6-sol"
        exec "supervised"
        when "pr is draft"
        prompt "zellij/prompts/reviewer.md"
    }
    node "releaser" {
        agent "claude"
        model "claude-opus-5[1m]"
        exec "supervised"
        when "pr is open and not draft"
        prompt "zellij/prompts/releaser.md"
    }
}
"#,
        )
        .unwrap()
    }

    #[test]
    fn plan_preserves_exec_modes_by_default() {
        let plan = plan(&v3(), false);
        assert!(plan.graph_text.contains("version=4"));
        assert!(plan.graph_text.contains("exec \"supervised\""));
        assert!(!plan.graph_text.contains("exec \"auto\""));
        assert!(!plan.graph_text.contains("tick"));
    }

    #[test]
    fn auto_workers_flips_only_non_claude() {
        let plan = plan(&v3(), true);
        let text = &plan.graph_text;
        assert!(text.contains("exec \"auto\""));
        let releaser = text.split("node \"releaser\"").nth(1).unwrap();
        assert!(releaser.contains("exec \"supervised\""));
        assert!(!releaser.contains("retrigger"), "supervised claude keeps gate retrigger");
        assert!(text.contains("retrigger=\"head-sha\""));
    }

    #[test]
    fn migrated_graph_parses_as_v4() {
        let plan = plan(&v3(), true);
        let graph = Graph::parse(&plan.graph_text).unwrap();
        assert_eq!(graph.version, 4);
        assert_eq!(graph.nodes.len(), 4);
    }

    #[test]
    fn prompts_are_one_item_and_repo_local() {
        let plan = plan(&v3(), false);
        let refiner = plan.prompts.iter().find(|(n, _)| n == "refiner").unwrap();
        assert!(refiner.1.contains("{gh_ticket_no}"));
        assert!(!refiner.1.contains("/loop"));
        assert!(!refiner.1.contains("get all tickets"));
        assert!(plan
            .graph_text
            .contains("prompt \".aif/prompts/refiner.md\""));
    }

    #[test]
    fn migration_is_idempotent_after_write() {
        let dir = std::env::temp_dir().join(format!("aif-migrate-{}", crate::ids::new_id()));
        std::fs::create_dir_all(dir.join(".aif")).unwrap();
        std::fs::write(
            dir.join(".aif/graph.kdl"),
            "graph {\n    tick \"30m\"\n    node \"refiner\" {\n        agent \"codex\"\n        model \"m\"\n        exec \"supervised\"\n        when \"issue has label 'to-refine'\"\n        prompt \"p.md\"\n    }\n}\n",
        )
        .unwrap();
        migrate(&dir, true, false).unwrap();
        let first = std::fs::read_to_string(dir.join(".aif/graph.kdl")).unwrap();
        let graph = Graph::load(&dir.join(".aif/graph.kdl")).unwrap();
        assert_eq!(graph.version, 4);
        migrate(&dir, false, false).unwrap_err();
        assert_eq!(
            first,
            std::fs::read_to_string(dir.join(".aif/graph.kdl")).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
