//! `ano spaces`: list and remove the user's spaces without the model.

use crate::{config::AppConfig, infrastructure::spaces::Spaces};
use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct SpacesArgs {
    #[command(subcommand)]
    command: Option<SpacesCommand>,
}

#[derive(Debug, Subcommand)]
enum SpacesCommand {
    /// List the spaces (the default).
    List,
    /// Remove a space with its page and data.
    Remove { name: String },
}

pub(super) fn run(config: &AppConfig, user: &str, args: SpacesArgs) -> Result<()> {
    let spaces = Spaces::new(config.spaces.directory()?);
    match args.command.unwrap_or(SpacesCommand::List) {
        SpacesCommand::List => {
            if !config.spaces.enabled {
                eprintln!("(spaces are disabled; set [spaces] enabled = true to build them in conversations)");
            }
            let list = spaces.list(user)?;
            if list.is_empty() {
                println!("No spaces.");
            }
            for space in list {
                println!(
                    "  {}  {}  (ano web: /spaces/{})",
                    space.name, space.title, space.name
                );
            }
        }
        SpacesCommand::Remove { name } => {
            spaces.remove(user, &name)?;
            println!("Removed {name}");
        }
    }
    Ok(())
}
