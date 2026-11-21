//! `ano feed`: read the feed, and rate items or mark them done, without
//! the model.

use crate::{
    config::AppConfig,
    infrastructure::{feed::Feed, fs::display_path},
};
use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct FeedArgs {
    #[command(subcommand)]
    command: Option<FeedCommand>,
}

#[derive(Debug, Subcommand)]
enum FeedCommand {
    /// List the items, newest first (the default).
    List {
        /// Also items marked done.
        #[arg(long)]
        all: bool,
    },
    /// Rate an item: up, down, or none.
    Rate { id: String, rating: String },
    /// Mark an item done.
    Done { id: String },
}

pub(super) fn run(config: &AppConfig, user: &str, args: FeedArgs) -> Result<()> {
    let feed = Feed::new(config.feed.directory()?);
    match args.command.unwrap_or(FeedCommand::List { all: false }) {
        FeedCommand::List { all } => {
            if !config.feed.enabled {
                eprintln!(
                    "(the feed is disabled; set [feed] enabled = true to let runs post to it)"
                );
            }
            println!("Feed in {}:", display_path(&feed.path(user)?));
            let items: Vec<_> = feed
                .items(user)?
                .into_iter()
                .filter(|item| all || !item.done)
                .collect();
            if items.is_empty() {
                println!("  (none)");
            }
            for item in items {
                let rating = match item.rating {
                    Some(1) => " 👍",
                    Some(-1) => " 👎",
                    _ => "",
                };
                println!(
                    "  [{}] {} {}{rating}",
                    item.id,
                    item.kind.name(),
                    item.title
                );
                if let Some(url) = &item.url {
                    println!("      {url}");
                }
                if let Some(action) = &item.action {
                    println!("      → {action}");
                }
            }
        }
        FeedCommand::Rate { id, rating } => {
            let rating = match rating.as_str() {
                "up" => 1,
                "down" => -1,
                "none" => 0,
                _ => anyhow::bail!("rating must be up, down, or none"),
            };
            let item = feed.rate(user, &id, rating)?;
            println!("Rated [{}] {}", item.id, item.title);
        }
        FeedCommand::Done { id } => {
            let item = feed.mark_done(user, &id, true)?;
            println!("Done [{}] {}", item.id, item.title);
        }
    }
    Ok(())
}
