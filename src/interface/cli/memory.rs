//! `ano memory`: list, add, and forget the facts kept about the user
//! without the model, and consolidate them with it.

use crate::{
    config::AppConfig,
    domain::user_memory::recent_facts,
    harness::{
        memory_consolidation::MemoryConsolidator,
        models::{Connections, RunModels},
    },
    infrastructure::{
        fs::display_path,
        user_memory::{MemoryBook, Saved},
    },
};
use anyhow::{bail, Context, Result};
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
    /// Merge facts that repeat or belong to each other, as the model
    /// proposes. The file as it was is kept as MEMORY.md.bak.
    Consolidate {
        /// Show the merges without changing the memory.
        #[arg(long)]
        dry_run: bool,
    },
}

pub(super) async fn run(config: &AppConfig, user: &str, args: MemoryArgs) -> Result<()> {
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
            let (_, omitted) = recent_facts(&facts);
            if omitted > 0 {
                println!("The {omitted} oldest facts are not sent with runs; `ano memory consolidate` merges facts that repeat or belong to each other.");
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
        MemoryCommand::Consolidate { dry_run } => {
            let selection = config.select_model(&[])?;
            let models = RunModels::resolve(config, &[], &selection, &mut Connections::default())
                .context("failed to connect the provider of the model")?;
            let report = MemoryConsolidator::new(models.main)
                .run(&book, user, !dry_run)
                .await?;
            for merge in &report.outcome.applied {
                println!("Merge:");
                for &member in &merge.members {
                    println!("  {}. {}", member + 1, report.before[member]);
                }
                println!("  -> {}", merge.into);
            }
            for (merge, reason) in &report.outcome.skipped {
                let numbers: Vec<String> = merge
                    .members
                    .iter()
                    .map(|member| match member.checked_add(1) {
                        Some(number) => number.to_string(),
                        None => "0".into(),
                    })
                    .collect();
                eprintln!(
                    "skipped a proposed merge of {}: {reason}",
                    numbers.join(", ")
                );
            }
            if report.not_shown > 0 {
                println!(
                    "The {} newest facts were not looked at: the memory is too long to show the model at once.",
                    report.not_shown
                );
            }
            let (before, after) = (report.before.len(), report.outcome.remaining().len());
            match (&report.backup, report.outcome.applied.is_empty()) {
                (_, true) => println!("Nothing to merge in {before} facts."),
                (Some(backup), _) => println!(
                    "Consolidated {before} facts into {after}. The memory as it was: {}",
                    display_path(backup)
                ),
                (None, _) => println!(
                    "Would consolidate {before} facts into {after} (dry run; nothing was changed)."
                ),
            }
        }
    }
    Ok(())
}
