//! `ano routines`: list, set up, and remove the user's reminders and
//! recurring tasks without the model.

use crate::{
    application::clock::unix_now,
    config::AppConfig,
    domain::routine::RoutineKind,
    infrastructure::{
        automation::schedule::local_time,
        fs::display_path,
        routines::{next_time, parse_time, RoutineBook},
    },
};
use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct RoutinesArgs {
    #[command(subcommand)]
    command: Option<RoutinesCommand>,
}

#[derive(Debug, Subcommand)]
enum RoutinesCommand {
    /// List the routines with their next times (the default).
    List,
    /// Set up a reminder or a recurring task.
    Add {
        /// reminder or task.
        #[arg(long, default_value = "reminder")]
        kind: String,
        /// A short name.
        title: String,
        /// What to remind of, or the work to do; defaults to the title.
        #[arg(long)]
        task: Option<String>,
        /// To repeat: five cron fields in local time, such as "0 17 * * FRI".
        #[arg(long, conflicts_with = "at", required_unless_present = "at")]
        cron: Option<String>,
        /// To happen once: a local time such as "2026-11-20 09:00".
        #[arg(long)]
        at: Option<String>,
    },
    /// Remove a routine.
    Remove { id: String },
}

pub(super) fn run(config: &AppConfig, user: &str, args: RoutinesArgs) -> Result<()> {
    let book = RoutineBook::new(config.routines.directory()?);
    match args.command.unwrap_or(RoutinesCommand::List) {
        RoutinesCommand::List => {
            if !config.routines.enabled {
                eprintln!("(routines are disabled; set [routines] enabled = true to set them up in conversations)");
            }
            println!("Routines in {}:", display_path(&book.path(user)?));
            let now = unix_now();
            let routines = book.list(user)?;
            if routines.is_empty() {
                println!("  (none)");
            }
            for routine in routines {
                let when = match next_time(&routine, now) {
                    Some(at) => format!("next {}", local_time(at)),
                    None => "passed".into(),
                };
                let repeat = routine
                    .cron
                    .as_deref()
                    .map(|pattern| format!(" \"{pattern}\""))
                    .unwrap_or_default();
                println!(
                    "  [{}] {} {}{repeat} — {when}",
                    routine.id,
                    routine.kind.name(),
                    routine.title
                );
            }
        }
        RoutinesCommand::Add {
            kind,
            title,
            task,
            cron,
            at,
        } => {
            let at = at.as_deref().map(parse_time).transpose()?;
            let routine = book.create(
                user,
                RoutineKind::parse(&kind)?,
                &title,
                task.as_deref().unwrap_or(&title),
                cron.as_deref(),
                at,
            )?;
            let next = next_time(&routine, unix_now())
                .map(local_time)
                .unwrap_or_default();
            println!("Set up [{}] {} — next {next}", routine.id, routine.title);
        }
        RoutinesCommand::Remove { id } => {
            let routine = book.delete(user, &id)?;
            println!("Removed [{}] {}", routine.id, routine.title);
        }
    }
    Ok(())
}
