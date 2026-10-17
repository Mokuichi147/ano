//! `ano automation`: check the automation of `ano serve` and send a test
//! notification, without starting the server.

use crate::{
    application::automation::{AutomationRegistry, Notification},
    config::AppConfig,
    domain::automation::{Priority, Trust},
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct AutomationArgs {
    #[command(subcommand)]
    command: AutomationCommand,
}

#[derive(Debug, Subcommand)]
enum AutomationCommand {
    /// Build the sources and notifiers of [automation] as `ano serve`
    /// would, and list them with the rules and the available kinds.
    List,
    /// Send a test notification through a notifier.
    Notify {
        /// The notifier in [automation.notifiers].
        notifier: String,
        /// The message; a test message when omitted.
        message: Option<String>,
        #[arg(long, default_value = "ano")]
        title: String,
        #[arg(long, value_parser = ["low", "normal", "high"], default_value = "normal")]
        priority: String,
    },
}

pub(super) async fn run(
    config: &AppConfig,
    registry: &AutomationRegistry,
    args: AutomationArgs,
) -> Result<()> {
    let automation = &config.automation;
    match args.command {
        AutomationCommand::List => {
            if !automation.is_enabled() {
                println!("No automation sources are configured; add [automation.sources] to the config file.");
            }
            let mut problems = 0;
            if !automation.sources.is_empty() {
                println!("Sources:");
            }
            for (name, source) in &automation.sources {
                match source.build(name, registry) {
                    Ok(built) => {
                        let trust = match built.trust() {
                            Trust::Trusted => "trusted",
                            Trust::Untrusted => "untrusted",
                        };
                        println!("  {name}: {} — {} ({trust})", source.kind, built.describe());
                    }
                    Err(error) => {
                        problems += 1;
                        println!("  {name}: {} — error: {error:#}", source.kind);
                    }
                }
            }
            if !automation.rules.is_empty() {
                println!("Rules (the first that matches an event applies):");
            }
            for rule in &automation.rules {
                let list = |items: &[String]| {
                    if items.is_empty() {
                        "any".to_string()
                    } else {
                        items.join(", ")
                    }
                };
                println!(
                    "  {}: sources {}, kinds {}, action {}{}{}",
                    rule.name,
                    list(&rule.sources),
                    list(&rule.kinds),
                    serde_json::to_value(rule.action)?.as_str().unwrap_or("?"),
                    rule.environment
                        .as_deref()
                        .map(|environment| format!(", runs in {environment}"))
                        .unwrap_or_default(),
                    if rule.notify.is_empty() {
                        String::new()
                    } else {
                        format!(", notifies {}", rule.notify.join(", "))
                    },
                );
            }
            if !automation.notifiers.is_empty() {
                println!("Notifiers:");
            }
            for (name, notifier) in &automation.notifiers {
                match notifier.build(name, registry) {
                    Ok(_) => println!("  {name}: {}", notifier.kind),
                    Err(error) => {
                        problems += 1;
                        println!("  {name}: {} — error: {error:#}", notifier.kind);
                    }
                }
            }
            if problems == 0 && automation.is_enabled() {
                // The rules are checked against what the sources are.
                if let Err(error) = automation.build(registry, &config.environments) {
                    problems += 1;
                    println!("error: {error:#}");
                }
            }
            println!(
                "Kinds: sources {}; notifiers {}",
                registry.source_kinds().collect::<Vec<_>>().join(", "),
                registry.notifier_kinds().collect::<Vec<_>>().join(", ")
            );
            if problems > 0 {
                bail!("{problems} problem(s) in [automation]");
            }
            Ok(())
        }
        AutomationCommand::Notify {
            notifier,
            message,
            title,
            priority,
        } => {
            let Some(settings) = automation.notifiers.get(&notifier) else {
                bail!(
                    "unknown notifier '{notifier}'; configured: {}",
                    if automation.notifiers.is_empty() {
                        "none".to_string()
                    } else {
                        automation
                            .notifiers
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
            };
            let sender = settings.build(&notifier, registry)?;
            sender
                .notifier
                .send(&Notification {
                    title,
                    message: message.unwrap_or_else(|| {
                        "This is a test notification from ano automation.".into()
                    }),
                    priority: Priority::parse(&priority).unwrap_or_default(),
                    url: None,
                    rule: None,
                })
                .await?;
            println!("Sent a notification through '{notifier}'.");
            Ok(())
        }
    }
}
