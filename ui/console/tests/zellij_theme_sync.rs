use aif::theme::Tokens;

#[test]
fn committed_zellij_theme_matches_tokens() {
    let repo_theme = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../zellij/themes/retro-future.kdl"
    );
    let committed = std::fs::read_to_string(repo_theme).expect("zellij theme file exists in repo");
    let tokens = Tokens::embedded().expect("embedded tokens");
    let rendered = tokens.zellij_kdl().expect("render zellij theme");
    assert_eq!(
        committed, rendered,
        "zellij/themes/retro-future.kdl differs from ui/tokens/tokens.json; regenerate it"
    );
}
