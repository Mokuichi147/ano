//! `ano skills`: list the saved skills, or show one, without the model.

use crate::{
    config::AppConfig,
    infrastructure::skills::{display_path, SkillLibrary},
};
use anyhow::{Context, Result};
use clap::Args;

#[derive(Debug, Args)]
pub(super) struct SkillsArgs {
    /// Show this skill instead of listing them all.
    name: Option<String>,
}

pub(super) fn run(config: &AppConfig, user: &str, args: SkillsArgs) -> Result<()> {
    let library = SkillLibrary::new(config.skills.directory()?);
    let directory = library.user_dir(user)?;
    if let Some(name) = args.name {
        let skill = library
            .read(user, &name)?
            .with_context(|| format!("no skill named {name:?} in {}", display_path(&directory)))?;
        println!("{}\n{}\n\n{}", skill.name, skill.description, skill.body);
        return Ok(());
    }
    if !config.skills.enabled {
        eprintln!("(skills are disabled; set [skills] enabled = true to use them in runs)");
    }
    let listing = library.list(user)?;
    println!("Skills in {}:", display_path(&directory));
    if listing.skills.is_empty() {
        println!("  (none)");
    }
    for skill in &listing.skills {
        println!("  {} - {}", skill.name, skill.description);
    }
    for problem in &listing.problems {
        eprintln!("warning: skipped skill {problem}");
    }
    Ok(())
}
