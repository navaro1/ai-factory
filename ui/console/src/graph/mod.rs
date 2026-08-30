pub mod conditions;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::graph::conditions::Condition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Opencode,
    Codex,
    Claude,
}

impl Agent {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "opencode" => Ok(Agent::Opencode),
            "codex" => Ok(Agent::Codex),
            "claude" => Ok(Agent::Claude),
            other => {
                bail!("unknown agent {other:?}; expected \"opencode\", \"codex\" or \"claude\"")
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Opencode => "opencode",
            Agent::Codex => "codex",
            Agent::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exec {
    Supervised,
    Auto,
}

impl Exec {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "supervised" => Ok(Exec::Supervised),
            "auto" => Ok(Exec::Auto),
            other => bail!("unknown exec {other:?}; expected \"supervised\" or \"auto\""),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Exec::Supervised => "supervised",
            Exec::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retrigger {
    #[default]
    Gate,
    HeadSha,
}

impl Retrigger {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "gate" => Ok(Retrigger::Gate),
            "head-sha" => Ok(Retrigger::HeadSha),
            other => bail!("unknown retrigger {other:?}; expected \"gate\" or \"head-sha\""),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Retrigger::Gate => "gate",
            Retrigger::HeadSha => "head-sha",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub name: String,
    pub agent: Agent,
    pub model: String,
    pub exec: Exec,
    pub when: Option<Condition>,
    pub prompt: Option<PathBuf>,
    pub limit: Option<usize>,
    pub retrigger: Retrigger,
}

#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub from: String,
    pub to: String,
    pub on: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub version: u32,
    pub tick_secs: u64,
    pub limit: usize,
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
}

pub const DEFAULT_GRAPH_PATH: &str = ".aif/graph.kdl";
pub const SUPPORTED_VERSIONS: [u32; 2] = [3, 4];

impl Graph {
    pub fn load(path: &Path) -> Result<Graph> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read graph file {}", path.display()))?;
        let graph = Self::parse(&raw)
            .with_context(|| format!("failed to parse graph file {}", path.display()))?;
        graph.validate()?;
        Ok(graph)
    }

    pub fn parse(raw: &str) -> Result<Graph> {
        let doc = kdl::KdlDocument::parse(raw).map_err(format_kdl_error)?;
        let graph_node = doc
            .nodes()
            .iter()
            .find(|n| n.name().value() == "graph")
            .ok_or_else(|| anyhow::anyhow!("missing top-level `graph` node"))?;

        let version = field_u64(graph_node, "version")
            .map(|v| v as u32)
            .unwrap_or(3);
        if !SUPPORTED_VERSIONS.contains(&version) {
            bail!(
                "unsupported graph version {version}; supported: {}",
                SUPPORTED_VERSIONS
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let tick_secs = match field_string(graph_node, "tick") {
            Some(raw) => parse_duration(&raw)?,
            None if version >= 4 => 600,
            None => bail!("missing `tick`, for example tick \"30m\""),
        };
        let mut graph = Graph {
            version,
            tick_secs,
            limit: field_usize(graph_node, "limit").unwrap_or(3),
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        if graph.tick_secs == 0 {
            bail!("tick must be a positive duration, for example \"30m\"");
        }
        if graph.limit == 0 {
            bail!("limit must be at least 1");
        }

        let children = graph_node
            .children()
            .ok_or_else(|| anyhow::anyhow!("`graph` has no children"))?;
        for node in children.nodes() {
            match node.name().value() {
                "node" => {
                    let name = arg_string(node, 0)
                        .ok_or_else(|| anyhow::anyhow!("`node` needs a name argument"))?;
                    let agent = Agent::parse(
                        &field_string(node, "agent")
                            .ok_or_else(|| anyhow::anyhow!("node {name}: missing `agent`"))?,
                    )
                    .with_context(|| format!("node {name}"))?;
                    let model = field_string(node, "model")
                        .ok_or_else(|| anyhow::anyhow!("node {name}: missing `model`"))?;
                    let exec = Exec::parse(
                        &field_string(node, "exec").unwrap_or_else(|| "supervised".into()),
                    )
                    .with_context(|| format!("node {name}"))?;
                    let when = field_string(node, "when")
                        .map(|raw| {
                            Condition::parse(&raw)
                                .with_context(|| format!("node {name}: invalid `when`"))
                        })
                        .transpose()?;
                    let prompt = field_string(node, "prompt").map(PathBuf::from);
                    let node_limit = field_usize(node, "limit");
                    let retrigger = match field_string(node, "retrigger") {
                        Some(raw) => Retrigger::parse(&raw)
                            .with_context(|| format!("node {name}: invalid `retrigger`"))?,
                        None => Retrigger::Gate,
                    };
                    if let Some(limit) = node_limit {
                        if version < 4 {
                            bail!("node {name}: `limit` requires graph version 4");
                        }
                        if limit == 0 {
                            bail!("node {name}: `limit` must be at least 1");
                        }
                    }
                    if retrigger != Retrigger::Gate && version < 4 {
                        bail!("node {name}: `retrigger` requires graph version 4");
                    }
                    if version >= 4 && agent == Agent::Claude && exec == Exec::Auto {
                        bail!(
                            "node {name}: claude cannot run with exec \"auto\"; keep it supervised"
                        );
                    }
                    graph.nodes.push(NodeSpec {
                        name,
                        agent,
                        model,
                        exec,
                        when,
                        prompt,
                        limit: node_limit,
                        retrigger,
                    });
                }
                "edge" => {
                    let from = prop_string(node, "from")
                        .ok_or_else(|| anyhow::anyhow!("`edge` needs a `from` property"))?;
                    let to = prop_string(node, "to")
                        .ok_or_else(|| anyhow::anyhow!("`edge` needs a `to` property"))?;
                    let on = prop_string(node, "on");
                    graph.edges.push(EdgeSpec { from, to, on });
                }
                "tick" | "limit" => {}
                other => bail!("unknown node `{other}` in graph file"),
            }
        }

        if graph.nodes.is_empty() {
            bail!("graph file declares no nodes");
        }
        Ok(graph)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for node in &self.nodes {
            if !seen.insert(node.name.clone()) {
                bail!("duplicate node {}", node.name);
            }
            if node.when.is_some() && node.prompt.is_none() {
                bail!(
                    "node {} has a `when` but no `prompt`; the dispatcher needs a prompt file",
                    node.name
                );
            }
        }
        for edge in &self.edges {
            for end in [&edge.from, &edge.to] {
                if !seen.contains(end) {
                    bail!("edge references unknown node {end}");
                }
            }
        }
        Ok(())
    }

    pub fn node(&self, name: &str) -> Option<&NodeSpec> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn to_dot(&self) -> String {
        let mut out = String::from("digraph factory {\n    rankdir=LR;\n");
        for node in &self.nodes {
            let shape = if node.when.is_some() {
                "box"
            } else {
                "ellipse"
            };
            let label = format!("{}\\n{} · {}", node.name, node.agent.as_str(), node.model);
            out.push_str(&format!(
                "    \"{}\" [label=\"{label}\", shape={shape}];\n",
                node.name
            ));
        }
        for edge in &self.edges {
            let label = edge.on.as_deref().unwrap_or("");
            out.push_str(&format!(
                "    \"{}\" -> \"{}\" [label=\"{label}\"];\n",
                edge.from, edge.to
            ));
        }
        out.push_str("}\n");
        out
    }

    pub fn template(prompts_dir: &std::path::Path) -> String {
        let p = |name: &str| prompts_dir.join(format!("{name}.md")).display().to_string();
        format!(
            r#"graph {{
    tick "30m"
    limit 3

    node "planner" {{
        agent "claude"
        model "claude-fable-5[1m]"
        exec "supervised"
    }}
    node "refiner" {{
        agent "codex"
        model "gpt-5.6-sol"
        exec "supervised"
        when "issue has label 'to-refine'"
        prompt "{refiner}"
    }}
    node "implementer" {{
        agent "opencode"
        model "zai-coding-plan/glm-5.3-flash"
        exec "supervised"
        when "issue has label 'refined' and dependencies met"
        prompt "{implementer}"
    }}
    node "reviewer" {{
        agent "codex"
        model "gpt-5.6-sol"
        exec "supervised"
        when "pr is draft"
        prompt "{reviewer}"
    }}
    node "releaser" {{
        agent "claude"
        model "claude-opus-5[1m]"
        exec "supervised"
        when "pr is open and not draft"
        prompt "{releaser}"
    }}

    edge from="refiner" to="implementer" on="label flips to refined"
    edge from="implementer" to="reviewer" on="draft pr opens"
    edge from="reviewer" to="releaser" on="pr leaves draft"
}}
"#,
            refiner = p("refiner"),
            implementer = p("implementer"),
            reviewer = p("reviewer"),
            releaser = p("releaser"),
        )
    }
}

pub fn parse_duration(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let invalid =
        || anyhow::anyhow!("invalid duration {raw:?}; use forms like \"30s\", \"10m\", \"1h\"");
    if raw.is_empty() {
        return Err(invalid());
    }
    let (digits, unit) = raw.split_at(raw.len().saturating_sub(1));
    let value: u64 = digits.parse().map_err(|_| invalid())?;
    match unit {
        "s" => Ok(value),
        "m" => Ok(value.saturating_mul(60)),
        "h" => Ok(value.saturating_mul(3600)),
        _ => Err(invalid()),
    }
}

fn format_kdl_error(err: kdl::KdlError) -> anyhow::Error {
    let mut msg = String::from("KDL parse failed");
    for diag in &err.diagnostics {
        let offset = diag.span.offset();
        let line = err.input[..offset.min(err.input.len())]
            .matches('\n')
            .count()
            + 1;
        let text = diag
            .message
            .clone()
            .unwrap_or_else(|| "syntax error".into());
        msg.push_str(&format!("\n  line {line}: {text}"));
    }
    anyhow::anyhow!(msg)
}

fn arg_string(node: &kdl::KdlNode, index: usize) -> Option<String> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .nth(index)
        .and_then(|e| e.value().as_string())
        .map(str::to_owned)
}

fn field_string(node: &kdl::KdlNode, name: &str) -> Option<String> {
    if let Some(value) = node.get(name).and_then(|v| v.as_string()) {
        return Some(value.to_owned());
    }
    let child = node.children()?.get(name)?;
    arg_string(child, 0)
}

fn field_usize(node: &kdl::KdlNode, name: &str) -> Option<usize> {
    if let Some(value) = node.get(name).and_then(|v| v.as_integer()) {
        return usize::try_from(value).ok();
    }
    let child = node.children()?.get(name)?;
    arg_usize(child, 0)
}

fn field_u64(node: &kdl::KdlNode, name: &str) -> Option<u64> {
    if let Some(value) = node.get(name).and_then(|v| v.as_integer()) {
        return u64::try_from(value).ok();
    }
    let child = node.children()?.get(name)?;
    node_u64(child)
}

fn node_u64(node: &kdl::KdlNode) -> Option<u64> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_integer())
        .and_then(|v| u64::try_from(v).ok())
}

fn arg_usize(node: &kdl::KdlNode, index: usize) -> Option<usize> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .nth(index)
        .and_then(|e| e.value().as_integer())
        .and_then(|v| usize::try_from(v).ok())
}

fn prop_string(node: &kdl::KdlNode, name: &str) -> Option<String> {
    node.get(name)
        .and_then(|value| value.as_string())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::conditions::Item;

    const SAMPLE: &str = r#"
graph {
    tick "30m"
    limit 3
    node "planner" {
        agent "claude"
        model "claude-fable-5[1m]"
    }
    node "refiner" {
        agent "opencode"
        model "openai/gpt-5.6-sol"
        exec "supervised"
        when "issue has label 'to-refine'"
        prompt "prompts/refiner.md"
    }
    node "releaser" {
        agent "claude"
        model "claude-opus-5[1m]"
        when "pr is open and not draft"
        prompt "prompts/releaser.md"
    }
    edge from="refiner" to="releaser" on="labels flip"
}
"#;

    #[test]
    fn parses_sample_graph() {
        let graph = Graph::parse(SAMPLE).unwrap();
        assert_eq!(graph.tick_secs, 1800);
        assert_eq!(graph.limit, 3);
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 1);
        let refiner = graph.node("refiner").unwrap();
        assert_eq!(refiner.agent, Agent::Opencode);
        assert_eq!(refiner.exec, Exec::Supervised);
        assert!(refiner.when.is_some());
        graph.validate().unwrap();
    }

    #[test]
    fn missing_tick_is_rejected() {
        let raw = r#"graph { node "a" { agent "claude"; model "m" } }"#;
        let err = Graph::parse(raw).unwrap_err().to_string();
        assert!(err.contains("tick"), "unexpected error: {err}");
    }

    #[test]
    fn syntax_error_reports_line() {
        let raw = "graph {\n  node \"a\" {\n";
        let err = Graph::parse(raw).unwrap_err().to_string();
        assert!(
            err.contains("line") || err.contains("column") || err.contains("3"),
            "error should carry position: {err}"
        );
    }

    #[test]
    fn rejects_unknown_edge_endpoint() {
        let raw = r#"
graph {
    tick "1m"
    node "a" { agent "claude"; model "m" }
    edge from="a" to="ghost"
}
"#;
        let graph = Graph::parse(raw).unwrap();
        assert!(graph.validate().is_err());
    }

    #[test]
    fn rejects_when_without_prompt() {
        let raw = r#"
graph {
    tick "1m"
    node "a" { agent "claude"; model "m"; when "pr is draft" }
}
"#;
        let graph = Graph::parse(raw).unwrap();
        assert!(graph.validate().is_err());
    }

    #[test]
    fn dot_output_lists_nodes_and_edges() {
        let graph = Graph::parse(SAMPLE).unwrap();
        let dot = graph.to_dot();
        assert!(dot.contains("\"refiner\" -> \"releaser\""));
        assert!(dot.contains("planner"));
    }

    #[test]
    fn template_parses_and_points_at_installed_prompts() {
        let text = Graph::template(std::path::Path::new("/home/x/.config/zellij/prompts"));
        let graph = Graph::parse(&text).unwrap();
        graph.validate().unwrap();
        assert_eq!(graph.tick_secs, 1800);
        assert_eq!(graph.limit, 3);
        assert_eq!(graph.nodes.len(), 5);
        assert_eq!(graph.edges.len(), 3);
        assert_eq!(graph.node("refiner").unwrap().agent, Agent::Codex);
        assert_eq!(
            graph.node("refiner").unwrap().prompt.as_deref(),
            Some(std::path::Path::new(
                "/home/x/.config/zellij/prompts/refiner.md"
            ))
        );
    }

    #[test]
    fn parses_codex_agent() {
        let raw = r#"
graph {
    tick "1m"
    node "reviewer" { agent "codex"; model "gpt-5.6-sol"; when "pr is draft"; prompt "p.md" }
}
"#;
        let graph = Graph::parse(raw).unwrap();
        assert_eq!(graph.node("reviewer").unwrap().agent, Agent::Codex);
        graph.validate().unwrap();
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("10m").unwrap(), 600);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn conditions_evaluate_against_items() {
        let graph = Graph::parse(SAMPLE).unwrap();
        let releaser_when = graph.node("releaser").unwrap().when.clone().unwrap();
        let labels = vec![];
        let blocked = vec![];
        let open_pr = Item {
            kind: conditions::ItemKind::PullRequest,
            number: 7,
            labels: &labels,
            open: true,
            draft: false,
            blocked_by: &blocked,
            blockers_open: &blocked,
        };
        let draft_pr = Item {
            draft: true,
            ..open_pr
        };
        assert!(releaser_when.evaluate(&open_pr));
        assert!(!releaser_when.evaluate(&draft_pr));

        let refiner_when = graph.node("refiner").unwrap().when.clone().unwrap();
        let to_refine = vec!["to-refine".to_owned()];
        let issue = Item {
            kind: conditions::ItemKind::Issue,
            number: 12,
            labels: &to_refine,
            open: true,
            draft: false,
            blocked_by: &blocked,
            blockers_open: &blocked,
        };
        assert!(refiner_when.evaluate(&issue));
        assert!(!refiner_when.evaluate(&open_pr));
    }
}
