use std::path::Path;
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};

static EMPTY_CONTAINER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"pane split_direction="[a-z]+" \{\s*\}\n?"#).unwrap());
static TAB_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"tab name="[^"]+""#).unwrap());

const SKIPPABLE: &[&str] = &[
    "planner",
    "factory",
    "refiner",
    "reviewer",
    "implementer",
    "releaser",
];

pub fn render(
    template: &str,
    root: &Path,
    repo: &str,
    prompts_dir: &Path,
    skip: &[String],
) -> Result<String> {
    let mut skipset: Vec<String> = Vec::new();
    for token in skip {
        let token = token.trim().to_lowercase();
        if token.is_empty() {
            continue;
        }
        if !SKIPPABLE.contains(&token.as_str()) {
            eprintln!("aif: unknown skip item '{token}' - ignored");
            continue;
        }
        skipset.push(token);
    }

    let mut text = template.to_owned();

    let roles: Vec<&str> = ["refiner", "reviewer", "implementer", "releaser"]
        .iter()
        .filter(|r| skipset.contains(&r.to_string()))
        .copied()
        .collect();
    if !roles.is_empty() {
        let pattern = RegexBuilder::new(&format!(r#"pane name="(?:{})""#, roles.join("|")))
            .case_insensitive(true)
            .build()
            .unwrap();
        text = drop_nodes(&text, &pattern);
    }
    if skipset.iter().any(|s| s == "planner") {
        let pattern = RegexBuilder::new(r#"tab name="Planner"#)
            .case_insensitive(true)
            .build()
            .unwrap();
        text = drop_nodes(&text, &pattern);
    }
    if skipset.iter().any(|s| s == "factory") {
        let pattern = RegexBuilder::new(r#"tab name="AI factory"#)
            .case_insensitive(true)
            .build()
            .unwrap();
        text = drop_nodes(&text, &pattern);
    }

    while let Some(found) = EMPTY_CONTAINER_REGEX.find(&text) {
        text.replace_range(found.range(), "");
    }

    loop {
        let mut removed = false;
        for found in TAB_NAME_REGEX.find_iter(&text) {
            let (start, end) = block(&text, found.start());
            if !text[start..end].contains("pane") {
                text.replace_range(start..end, "");
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }

    if !text.contains("tab name=") {
        bail!("every tab was skipped - nothing to start");
    }

    Ok(text
        .replace("{{CWD}}", &root.display().to_string())
        .replace("{{REPO}}", repo)
        .replace("{{PROMPTS}}", &prompts_dir.display().to_string()))
}

fn drop_nodes(text: &str, pattern: &Regex) -> String {
    let mut owned = text.to_owned();
    loop {
        let Some(found) = pattern.find(&owned) else {
            return owned;
        };
        let (start, end) = block(&owned, found.start());
        owned.replace_range(start..end, "");
    }
}

fn block(text: &str, kw_start: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = kw_start;
    let mut in_str = false;
    while i < n {
        let c = text[i..].chars().next().unwrap();
        if in_str {
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '{' {
            break;
        }
        i += c.len_utf8();
    }
    let mut depth = 0;
    let mut j = i;
    let mut in_str = false;
    while j < n {
        let c = text[j..].chars().next().unwrap();
        if in_str {
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        j += c.len_utf8();
    }
    let mut end = j + 1;
    if end < n && bytes[end] == b'\n' {
        end += 1;
    }
    let line_start = text[..kw_start].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let start = if text[line_start..kw_start].trim().is_empty() {
        line_start
    } else {
        kw_start
    };
    (start, end)
}

pub fn installed_template() -> Result<String> {
    let zdir = std::env::var("ZELLIJ_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let config = std::env::var("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    std::path::PathBuf::from(home).join(".config")
                });
            config.join("zellij")
        });
    let path = zdir.join("layouts").join("ai-factory.kdl");
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read layout template {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../zellij/layouts/ai-factory.kdl"
        );
        std::fs::read_to_string(path).expect("layout template exists in repo")
    }

    fn skip(items: &[&str]) -> Result<String> {
        let skip: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        render(
            &template(),
            Path::new("/repo/borsuk"),
            "borsuk",
            Path::new("/prompts"),
            &skip,
        )
    }

    #[test]
    fn full_render_keeps_everything() {
        let text = skip(&[]).unwrap();
        assert_eq!(text.matches("tab name=").count(), 2);
        assert_eq!(text.matches("pane name=").count(), 5);
        assert!(text.contains("Planner - borsuk"));
        assert!(text.contains("AI factory - borsuk"));
        assert!(text.contains("/prompts/refiner.md"));
    }

    #[test]
    fn skip_planner_drops_tab_one() {
        let text = skip(&["planner"]).unwrap();
        assert!(!text.contains("Planner - borsuk"));
        assert!(text.contains("AI factory - borsuk"));
        assert_eq!(text.matches("pane name=").count(), 4);
    }

    #[test]
    fn skip_two_panes_collapses_containers() {
        let text = skip(&["refiner", "reviewer"]).unwrap();
        assert!(!text.contains(r#"pane name="Refiner""#));
        assert!(!text.contains(r#"pane name="Reviewer""#));
        assert!(text.contains(r#"pane name="Implementer""#));
        assert!(text.contains(r#"pane name="Releaser""#));
        assert_eq!(text.matches("split_direction").count(), 2);
    }

    #[test]
    fn skip_everything_fails() {
        assert!(skip(&["planner", "factory"]).is_err());
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        let text = skip(&["bogus", "planner"]).unwrap();
        assert!(!text.contains("Planner - borsuk"));
    }
}
