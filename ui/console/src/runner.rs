use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::actions;
use crate::graph::{self, Graph};
use crate::probe;
use crate::scheduler::{self, Action, Event, PaneState, RunPaths};
use crate::status;

pub fn run_once(root: &Path, dry_run: bool) -> Result<()> {
    let graph_file = root.join(graph::DEFAULT_GRAPH_PATH);
    let graph = Graph::load(&graph_file)?;
    let session = status::session_name(root);
    let paths = RunPaths::new(&session);

    let snapshot = probe::probe(root)?;
    let tasks = scheduler::evaluate(&graph, &snapshot);

    let mut pane_states: BTreeMap<String, PaneState> = BTreeMap::new();
    let mut pane_by_role: BTreeMap<String, String> = BTreeMap::new();
    match status::report() {
        Ok(report) => {
            for pane in &report.panes {
                let state = match pane.class.state.as_str() {
                    "draft waiting" | "working" => PaneState::Busy,
                    "exited" | "needs trust" => PaneState::Missing,
                    _ => PaneState::Idle,
                };
                pane_states.insert(pane.class.role.to_lowercase(), state);
                pane_by_role.insert(pane.class.role.to_lowercase(), pane.pane.clone());
            }
        }
        Err(_) => {
            if !dry_run {
                bail!("factory session {session} is not running; start it with: aif start");
            }
        }
    }

    let mut ledger = paths.load_ledger();
    let live_keys: Vec<String> = tasks.iter().map(|t| t.key()).collect();
    ledger.prune_to(&live_keys);

    let decisions = scheduler::plan(&graph, &tasks, &ledger, &pane_states);
    for decision in &decisions {
        let task = &decision.task;
        match &decision.action {
            Action::Dispatch { .. } => {
                let role = task.node.to_lowercase();
                let Some(pane) = pane_by_role.get(&role) else {
                    continue;
                };
                if dry_run {
                    println!(
                        "{} {} \"{}\" -> would dispatch to {pane}",
                        task.node,
                        task.key(),
                        task.title
                    );
                    continue;
                }
                let node = graph
                    .node(&task.node)
                    .with_context(|| format!("unknown node {}", task.node))?;
                let prompt = scheduler::render_prompt(node, root, task)?;
                actions::paste_text(&session, pane, &prompt)?;
                ledger.mark(&task.key(), &task.node);
                paths.append_event(&Event {
                    ts: scheduler::now_iso(),
                    event: "dispatch".into(),
                    node: task.node.clone(),
                    item: task.key(),
                    detail: format!("pasted into {pane}"),
                })?;
                println!("{} {} -> dispatched to {pane}", task.node, task.key());
            }
            Action::Queue => {
                println!("{} {} \"{}\" -> queued", task.node, task.key(), task.title);
            }
            Action::Skip { reason } => {
                println!("{} {} -> skipped: {reason}", task.node, task.key());
                if !dry_run {
                    paths.append_event(&Event {
                        ts: scheduler::now_iso(),
                        event: "skip".into(),
                        node: task.node.clone(),
                        item: task.key(),
                        detail: reason.clone(),
                    })?;
                }
            }
        }
    }

    if !dry_run {
        paths.save_ledger(&ledger)?;
    }
    if decisions.is_empty() {
        println!("nothing ready for dispatch");
    }
    Ok(())
}

pub fn run_loop(root: &Path) -> Result<()> {
    let graph_file = root.join(graph::DEFAULT_GRAPH_PATH);
    let graph = Graph::load(&graph_file)?;
    println!(
        "aif run: tick {}s, limit {}; press Ctrl-C to stop",
        graph.tick_secs, graph.limit
    );
    loop {
        if let Err(err) = run_once(root, false) {
            eprintln!("aif: tick failed: {err:#}");
        }
        std::thread::sleep(std::time::Duration::from_secs(graph.tick_secs));
    }
}

pub fn print_events(root: &Path, last: usize) -> Result<()> {
    let session = status::session_name(root);
    let paths = RunPaths::new(&session);
    let events = paths.read_events()?;
    let start = events.len().saturating_sub(last);
    for event in &events[start..] {
        println!(
            "{} {} {} {} {}",
            event.ts, event.event, event.node, event.item, event.detail
        );
    }
    Ok(())
}
