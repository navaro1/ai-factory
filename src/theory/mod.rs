//! The theory governor: the operator's model of one repository.

pub mod model;
pub mod records;
pub mod skills;
pub mod verify;

#[cfg(test)]
mod stance {
    const VOCABULARY: &[&str] = &[
        "governor",
        "window",
        "theory",
        "sweep",
        "cards",
        "interview",
        "theory-short",
        "theory-full",
        "delta-open",
        "event-open",
        "model-pr",
        "ladder-1",
        "ladder-2",
        "ladder-3",
        "MAP",
        "DELTAS",
        "LADDER",
        "AREAS",
        "verify-skill",
        "browser",
        "dom",
        "http",
        "terminal",
        "none",
        "surface",
        "run skill",
        "driver",
        "tier",
        "floor",
        "feature",
        "fast command",
        "fast check",
        "Before / After line",
        "acceptance criterion",
        "trace",
        "lever",
        "teach",
    ];

    fn vocabulary_table() -> String {
        let doc = include_str!("../../docs/STANCE.md");
        let start = doc
            .find("## Vocabulary")
            .expect("docs/STANCE.md carries a vocabulary section");
        let rest = &doc[start + "## Vocabulary".len()..];
        let end = rest.find("\n## ").map_or(rest.len(), |at| at + 1);
        rest[..end]
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn vocabulary_table_names_every_term() {
        let table = vocabulary_table();
        for term in VOCABULARY {
            assert!(
                table.contains(term),
                "the vocabulary table of docs/STANCE.md must name {term}"
            );
        }
    }

    #[test]
    fn readme_first_section_links_the_stance() {
        let readme = include_str!("../../README.md");
        let first = readme.split("\n## ").next().unwrap_or(readme);
        assert!(
            first.contains("](docs/STANCE.md)"),
            "the first section of the README must link docs/STANCE.md"
        );
    }
}
