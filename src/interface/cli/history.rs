//! モデルを呼ばずに原文履歴を再送・参照するコマンド。

use crate::{
    application::ports::HistoryBackend, config::AppConfig, infrastructure::chronotope::Chronotope,
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde_json::json;

#[derive(Debug, Args)]
pub(super) struct HistoryArgs {
    #[command(subcommand)]
    command: HistoryCommand,
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    /// 未送信件数を確認する。ネットワークへの接続は行わない。
    Status,
    /// 未送信の原文イベントを再送する。
    Sync,
    /// 会話本文を検索する。
    Search {
        text: String,
        #[arg(long)]
        exact: bool,
        #[arg(long)]
        human: bool,
        #[arg(long)]
        conversation: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// 発言 ID から原文のページを読む。
    Get {
        event: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 4096)]
        length: u64,
    },
    /// 発言の前後を読む。
    Context { event: String },
    /// 会話の一覧を取得する。
    Conversations,
}

pub(super) async fn run(config: &AppConfig, user: &str, args: HistoryArgs) -> Result<()> {
    if !config.history.enabled {
        bail!("[history] enabled = true を設定してください");
    }
    let history = Chronotope::new(config.history.clone())?;
    let value = match args.command {
        HistoryCommand::Status => history.status(user)?,
        HistoryCommand::Sync => history.sync(user).await?,
        HistoryCommand::Search {
            text,
            exact,
            human,
            conversation,
            cursor,
        } => {
            let mut args = json!({"text":text,"exact":exact});
            if human {
                args["origins"] = json!(["human"]);
            }
            if let Some(conversation) = conversation {
                args["conversation"] = json!(conversation);
            }
            if let Some(cursor) = cursor {
                args["cursor"] = json!(cursor);
            }
            history.query(user, "history_search", args).await?
        }
        HistoryCommand::Get {
            event,
            offset,
            length,
        } => {
            history
                .query(
                    user,
                    "history_get",
                    json!({"event":event,"offset":offset,"length":length}),
                )
                .await?
        }
        HistoryCommand::Context { event } => {
            history
                .query(user, "history_context", json!({"event":event}))
                .await?
        }
        HistoryCommand::Conversations => {
            history
                .query(user, "history_conversations", json!({}))
                .await?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
