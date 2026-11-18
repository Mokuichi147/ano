//! `ano objectives`: list, add, change, and track the user's long-term
//! objectives without the model.

use crate::{
    config::AppConfig,
    domain::objective::{format_number, ObjectiveChange, Status, Target},
    infrastructure::{fs::display_path, objectives::ObjectiveBook},
};
use anyhow::Result;
use chrono::{Local, TimeZone};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct ObjectivesArgs {
    #[command(subcommand)]
    command: Option<ObjectivesCommand>,
}

#[derive(Debug, Subcommand)]
enum ObjectivesCommand {
    /// List the objectives (the default). Finished ones only with --all.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Show one objective with its progress log.
    Show { id: String },
    /// Add an objective.
    Add {
        title: String,
        #[command(flatten)]
        fields: Fields,
    },
    /// Change the fields given of an objective.
    Set {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[command(flatten)]
        fields: Fields,
        /// active, paused, done, or dropped.
        #[arg(long)]
        status: Option<String>,
    },
    /// Record progress.
    Log {
        id: String,
        note: String,
        /// The measured value now, for an objective with a target.
        #[arg(long)]
        value: Option<f64>,
    },
    /// Remove an objective with its log.
    Remove { id: String },
}

#[derive(Debug, Args)]
struct Fields {
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    why: Option<String>,
    #[arg(long)]
    next_step: Option<String>,
    /// A date to aim for, such as 2026-12-31.
    #[arg(long)]
    due: Option<String>,
    /// A number to reach, such as 100000.
    #[arg(long, requires = "unit")]
    target: Option<f64>,
    /// The unit of --target, such as yen.
    #[arg(long, requires = "target")]
    unit: Option<String>,
}

impl Fields {
    fn into_change(self, title: Option<String>, status: Option<Status>) -> ObjectiveChange {
        ObjectiveChange {
            title,
            category: self.category,
            why: self.why,
            status,
            next_step: self.next_step,
            due: self.due,
            target: self
                .target
                .zip(self.unit)
                .map(|(value, unit)| Some(Target { value, unit })),
        }
    }
}

pub(super) fn run(config: &AppConfig, user: &str, args: ObjectivesArgs) -> Result<()> {
    let book = ObjectiveBook::new(config.objectives.directory()?);
    match args
        .command
        .unwrap_or(ObjectivesCommand::List { all: false })
    {
        ObjectivesCommand::List { all } => {
            if !config.objectives.enabled {
                eprintln!(
                    "(objectives are disabled; set [objectives] enabled = true to use them in runs)"
                );
            }
            let objectives = book.list(user)?;
            println!("Objectives in {}:", display_path(&book.path(user)?));
            let shown: Vec<_> = objectives
                .iter()
                .filter(|objective| {
                    all || matches!(objective.status, Status::Active | Status::Paused)
                })
                .collect();
            if shown.is_empty() {
                println!("  (none)");
            }
            for objective in shown {
                println!("  {:<7} {}", objective.status.name(), objective.summary());
            }
        }
        ObjectivesCommand::Show { id } => {
            let objective = book.get(user, &id)?;
            println!("{} [{}]", objective.summary(), objective.status.name());
            if !objective.why.is_empty() {
                println!("why: {}", objective.why);
            }
            for entry in &objective.log {
                let when = Local
                    .timestamp_opt(entry.at as i64, 0)
                    .single()
                    .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                match entry.value {
                    Some(value) => println!("  {when}  {} ({})", entry.note, format_number(value)),
                    None => println!("  {when}  {}", entry.note),
                }
            }
        }
        ObjectivesCommand::Add { title, fields } => {
            let objective = book.create(user, &fields.into_change(Some(title), None))?;
            println!("Added {}", objective.summary());
        }
        ObjectivesCommand::Set {
            id,
            title,
            fields,
            status,
        } => {
            let status = status.map(|status| Status::parse(&status)).transpose()?;
            let change = fields.into_change(title, status);
            if change.is_empty() {
                anyhow::bail!("give at least one field to change");
            }
            let objective = book.change(user, &id, &change)?;
            println!("Updated {}", objective.summary());
        }
        ObjectivesCommand::Log { id, note, value } => {
            let objective = book.log(user, &id, &note, value)?;
            println!("Recorded. {}", objective.summary());
        }
        ObjectivesCommand::Remove { id } => {
            let objective = book.remove(user, &id)?;
            println!("Removed {}", objective.summary());
        }
    }
    Ok(())
}
