//! `ano character`: show or change the user's character without the model.

use crate::{
    config::AppConfig,
    domain::character::CharacterUpdate,
    infrastructure::{character::CharacterStore, fs::display_path},
};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub(super) struct CharacterArgs {
    #[command(subcommand)]
    command: Option<CharacterCommand>,
}

#[derive(Debug, Subcommand)]
enum CharacterCommand {
    /// Show the character and where it is kept (the default).
    Show,
    /// Create the character, or change the parts given.
    Set {
        /// The name the agent goes by.
        #[arg(long)]
        name: Option<String>,
        /// One line that sums the character up; "" clears it.
        #[arg(long)]
        tagline: Option<String>,
        /// The personality and tone in Markdown; "" clears it.
        #[arg(long, conflicts_with = "soul_file")]
        soul: Option<String>,
        /// Read the soul from this file instead.
        #[arg(long, value_name = "PATH")]
        soul_file: Option<PathBuf>,
    },
}

pub(super) fn run(config: &AppConfig, user: &str, args: CharacterArgs) -> Result<()> {
    let store = CharacterStore::new(config.character.directory()?);
    let path = store.path(user)?;
    match args.command.unwrap_or(CharacterCommand::Show) {
        CharacterCommand::Show => {
            if !config.character.enabled {
                eprintln!(
                    "(the character is disabled; set [character] enabled = true to use it in runs)"
                );
            }
            match store.read(user)? {
                Some(character) => {
                    println!("{}", character.name);
                    if !character.tagline.is_empty() {
                        println!("{}", character.tagline);
                    }
                    if !character.soul.is_empty() {
                        println!("\n{}", character.soul);
                    }
                    println!("\n({})", display_path(&path));
                }
                None => println!(
                    "No character yet. Create one with `ano character set --name NAME`, or write {}",
                    display_path(&path)
                ),
            }
        }
        CharacterCommand::Set {
            name,
            tagline,
            soul,
            soul_file,
        } => {
            let soul = match soul_file {
                Some(file) => {
                    let text = std::fs::read_to_string(&file)
                        .with_context(|| format!("failed to read {}", file.display()))?;
                    // Editors on Windows may start the file with a byte order mark.
                    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
                }
                None => soul,
            };
            let (character, saved) = store.update(
                user,
                &CharacterUpdate {
                    name,
                    tagline,
                    soul,
                },
            )?;
            println!(
                "{} {} ({})",
                if saved.created { "Created" } else { "Updated" },
                character.name,
                display_path(&saved.path)
            );
        }
    }
    Ok(())
}
