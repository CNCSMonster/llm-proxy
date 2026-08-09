use anyhow::{Context, Result};

const DOCS: &[(&str, &str)] = &[
    (
        "cli-guide",
        include_str!("../docs/user_guide/cli-guide.md"),
    ),
    (
        "tui-guide",
        include_str!("../docs/user_guide/tui-guide.md"),
    ),
    (
        "troubleshooting",
        include_str!("../docs/user_guide/troubleshooting.md"),
    ),
    (
        "oauth-accounts",
        include_str!("../docs/user_guide/user-guide-oauth-accounts.md"),
    ),
];

pub fn print_doc(list: bool, raw: bool, section: Option<String>) -> Result<()> {
    if list {
        for (name, _) in DOCS {
            println!("{name}");
        }
        return Ok(());
    }

    let name = section.as_deref().unwrap_or("cli-guide");
    let text = DOCS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, text)| *text)
        .with_context(|| format!("unknown doc section {name:?}; use `llm-proxy doc --list`"))?;

    let _ = raw;
    print!("{text}");
    Ok(())
}

#[cfg(test)]
pub fn ensure_valid_section(name: &str) -> Result<()> {
    if DOCS.iter().any(|(candidate, _)| *candidate == name) {
        Ok(())
    } else {
        anyhow::bail!("unknown doc section {name:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_doc_sections_are_embedded() {
        for (name, _) in DOCS {
            ensure_valid_section(name).expect("known section");
        }
    }
}
