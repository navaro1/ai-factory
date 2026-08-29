use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use ratatui::style::Color;
use serde::Deserialize;

pub const TOKENS_JSON: &str = include_str!("../../tokens/tokens.json");

const ZELLIJ_ORDER: [&str; 11] = [
    "bg", "fg", "black", "white", "red", "green", "yellow", "blue", "magenta", "cyan", "orange",
];

#[derive(Debug, Deserialize)]
pub struct Tokens {
    pub name: String,
    pub colors: BTreeMap<String, String>,
    pub ui: UiTokens,
}

#[derive(Debug, Deserialize)]
pub struct UiTokens {
    pub surface: String,
    pub surface_raised: String,
    pub accent: String,
    pub warn: String,
    pub error: String,
    pub dim: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(hex: &str) -> Result<Self> {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid hex color: {hex:?}");
        }
        let value = u32::from_str_radix(hex, 16).context("invalid hex color")?;
        Ok(Rgb(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        ))
    }

    pub fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

impl Tokens {
    pub fn embedded() -> Result<Self> {
        serde_json::from_str(TOKENS_JSON).context("embedded tokens.json is invalid")
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read tokens file {}", path.display()))?;
        let tokens: Tokens = serde_json::from_str(&raw).context("tokens file is invalid")?;
        tokens.validate()?;
        Ok(tokens)
    }

    pub fn validate(&self) -> Result<()> {
        for key in ZELLIJ_ORDER {
            if !self.colors.contains_key(key) {
                bail!("tokens.json is missing the {key:?} color");
            }
        }
        for hex in self.colors.values() {
            Rgb::parse(hex)?;
        }
        Ok(())
    }

    pub fn rgb(&self, key: &str) -> Result<Rgb> {
        let hex = self
            .colors
            .get(key)
            .with_context(|| format!("unknown color token {key:?}"))?;
        Rgb::parse(hex)
    }

    pub fn zellij_kdl(&self) -> Result<String> {
        self.validate()?;
        let mut out = String::new();
        out.push_str("themes {\n");
        out.push_str(&format!("    {} {{\n", self.name));
        for key in ZELLIJ_ORDER {
            let hex = &self.colors[key];
            out.push_str(&format!("        {key} \"{hex}\"\n"));
        }
        out.push_str("    }\n");
        out.push_str("}\n");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tokens_are_valid() {
        let tokens = Tokens::embedded().expect("embedded tokens parse");
        tokens.validate().expect("embedded tokens validate");
    }

    #[test]
    fn hex_colors_parse() {
        assert_eq!(Rgb::parse("#0d0b1e").unwrap(), Rgb(13, 11, 30));
        assert_eq!(Rgb::parse("ff2975").unwrap(), Rgb(255, 41, 117));
        assert!(Rgb::parse("#12345").is_err());
        assert!(Rgb::parse("purple").is_err());
    }

    #[test]
    fn zellij_theme_lists_all_colors() {
        let tokens = Tokens::embedded().unwrap();
        let kdl = tokens.zellij_kdl().unwrap();
        for key in ZELLIJ_ORDER {
            assert!(kdl.contains(&format!("{key} \"#")));
        }
        assert!(kdl.starts_with("themes {"));
        assert!(kdl.ends_with("}\n"));
    }
}
