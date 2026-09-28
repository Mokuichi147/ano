use crate::infrastructure::{chatgpt::auth::ChatGptAuth, openai::ApiSettings};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub(super) struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// 確認コードを使い、ブラウザーで ChatGPT にログインする。
    Login,
    /// 保存済み認証情報の有無と有効期限の状態を表示する（通信しない）。
    Status {
        #[arg(long)]
        json: bool,
    },
    /// ano が保存した ChatGPT 認証情報を削除する。
    Logout,
}

pub(super) async fn run(settings: &ApiSettings, args: AuthArgs) -> Result<()> {
    let auth = ChatGptAuth::new(settings.chatgpt_auth_file.as_deref())?;
    match args.command {
        AuthCommand::Login => {
            tokio::select! {
                result = auth.login(|url, code| {
                    eprintln!("ブラウザーで {url} を開き、次の確認コードを入力してください。\n\n{code}\n\n認証の完了を待っています（Ctrl+C で中止）。");
                }) => result?,
                _ = tokio::signal::ctrl_c() => bail!("ChatGPT ログインを中止しました"),
            }
            println!("ChatGPT にログインしました。config.toml の [api] に auth = \"chatgpt\" を設定すると利用できます。");
        }
        AuthCommand::Status { json } => {
            let status = auth.status()?;
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else if !status.logged_in {
                println!("ChatGPT: 未ログイン。ano auth login を実行してください。");
            } else if status.needs_refresh {
                println!("ChatGPT: 認証情報あり。次回リクエスト時にトークンを更新します。");
            } else {
                println!("ChatGPT: 認証情報あり（サーバーでの有効性は未確認）。");
            }
        }
        AuthCommand::Logout => {
            if auth.logout().await? {
                println!("ano の ChatGPT 認証情報を削除しました。");
            } else {
                println!("ano の ChatGPT 認証情報は保存されていません。");
            }
        }
    }
    Ok(())
}
