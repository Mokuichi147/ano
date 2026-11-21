//! `ano memory`: list, add, and forget the facts kept about the user,
//! without the model.

use crate::{
    config::AppConfig,
    infrastructure::{
        fs::display_path,
        user_memory::{MemoryBook, Saved},
    },
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct MemoryArgs {
    #[command(subcommand)]
    command: Option<MemoryCommand>,
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// List the remembered facts with their numbers (the default).
    List,
    /// Remember a fact.
    Add {
        /// One short fact, such as "Prefers concise answers".
        fact: String,
    },
    /// Forget facts by the numbers `ano memory` lists.
    Forget {
        #[arg(required = true)]
        numbers: Vec<usize>,
    },
}

pub(super) fn run(config: &AppConfig, user: &str, args: MemoryArgs) -> Result<()> {
    let book = MemoryBook::new(config.memory.directory()?);
    let path = book.path(user)?;
    match args.command.unwrap_or(MemoryCommand::List) {
        MemoryCommand::List => {
            if !config.memory.enabled {
                eprintln!("(memory is disabled; set [memory] enabled = true to use it in runs)");
            }
            let facts = book.facts(user)?;
            println!("Memory in {}:", display_path(&path));
            if facts.is_empty() {
                println!("  (none)");
            }
            for (number, fact) in facts.iter().enumerate() {
                println!("  {}. {fact}", number + 1);
            }
        }
        MemoryCommand::Add { fact } => match book.save(user, &fact)? {
            Saved::Added => println!("Remembered."),
            Saved::Unchanged => println!("Already remembered."),
        },
        MemoryCommand::Forget { numbers } => {
            let facts = book.facts(user)?;
            let mut chosen = Vec::new();
            for number in numbers {
                match number.checked_sub(1).and_then(|index| facts.get(index)) {
                    Some(fact) => chosen.push(fact.clone()),
                    None => bail!(
                        "no fact numbered {number}; `ano memory` lists {} facts",
                        facts.len()
                    ),
                }
            }
            let missing = book.forget(user, &chosen)?;
            println!("Forgot {} facts.", chosen.len() - missing.len());
        }
    }
    Ok(())
}
