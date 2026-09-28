# ChatGPT サブスクリプションでの接続

ChatGPT の OAuth 認証を使い、Codex のサブスクリプション用接続先にリクエストを送信する実験的な機能です。ano 自身が会話管理とツール実行を行います。Codex CLI のインストールは不要です。

この方式は [OpenHands のサブスクリプション連携](https://docs.openhands.dev/sdk/guides/llm-subscriptions) を参考にしています。専用接続先の仕様変更に追従が必要になる場合があります。利用には Codex を使える ChatGPT アカウントが必要です。利用枠・モデルの提供範囲は契約プランとワークスペース設定に依存します。通常の OpenAI API の利用料金とは別の枠です。

## 設定

```sh
ano auth login
```

表示された URL をブラウザーで開き、確認コードを入力して認証します。サーバー上で実行する場合も、手元のブラウザーで完了できます。待機時間は最大15分で、Ctrl+C で中止できます。アカウント側でデバイス認証が無効なら、ChatGPT のセキュリティ設定またはワークスペース管理者の設定を確認してください。

`config.toml` の該当セクションを設定します。

```toml
[api]
auth = "chatgpt"

[agent]
# 契約プランで利用できるモデルを指定してください。
model = "gpt-6-sol"
```

```sh
ano auth status
ano run "このリポジトリを調べて概要を説明して"
ano chat --session .ano/chatgpt-session.json
```

`--config` を使う場合は、認証コマンドにも同じファイルを指定してください。ログインコマンドは設定ファイルを変更しません。

```sh
ano --config ./my-config.toml auth login
ano --config ./my-config.toml auth status --json
```

`auth status` は保存済み認証情報の有無と期限だけを調べ、通信しません。サーバー上での有効性や残量を確認するコマンドではありません。

## 認証情報

既定の保存先は OS 標準のアプリ用データディレクトリの `auth/chatgpt.json` です。

| OS | 既定の保存先 |
| --- | --- |
| macOS | `~/Library/Application Support/ano/auth/chatgpt.json` |
| Linux | `$XDG_DATA_HOME/ano/auth/chatgpt.json`（既定 `~/.local/share/ano/auth/chatgpt.json`） |
| Windows | `%APPDATA%\ano\data\auth\chatgpt.json` |

以前の版の保存先 `~/.ano/auth/chatgpt.json` にだけ認証情報がある場合は、初回の利用時に新しい保存先へ移し、古いファイルを削除します（再ログインは不要です）。`ano auth logout` は古い保存先に残ったコピーも削除します。`chatgpt_auth_file` を指定した場合は移行しません。

必要なら `[api]` の `chatgpt_auth_file` で変更できます。相対パスは設定ファイルがあるディレクトリを基準に解決し、先頭の `~` はホームディレクトリへ展開します。

```toml
[api]
auth = "chatgpt"
chatgpt_auth_file = "~/secrets/work-chatgpt.json"
```

- アクセストークンと更新用トークンを保存し、有効期限直前に自動更新します。401 応答の場合も1回だけ更新して再送します。
- ファイルを原子的に置き換え、複数の ano プロセスによる更新をファイルロックで直列化します。
- Unix では認証ファイルを所有者だけが読み書きできる `0600` で保存し、新しく作る保存ディレクトリを `0700` にします。
- Codex CLI や他アプリの認証情報は読み取り・変更しません。
- 認証ファイルには秘密情報が含まれます。リポジトリへコミットしないでください。

```sh
ano auth logout
```

ログアウトは ano の認証ファイルを削除します。ChatGPT のブラウザーセッションの終了や、サーバー側でのトークン失効は行いません。ロック用の `.lock` ファイルは再利用のため残ります。

## 通常の API 接続との差

| 項目 | ChatGPT 接続での動作 |
| --- | --- |
| API キー | 使用しません。`OPENAI_API_KEY` があっても選択されません |
| 接続先 | Codex 専用 URL に固定。`base_url` / `OPENAI_BASE_URL` のカスタム指定はエラーにします |
| 会話履歴 | 毎回履歴全体を送信。`store = false`、暗号化された推論情報も再送 |
| ストリーミング | 通信では常に有効。`api.stream = false` は画面への逐次表示のみを無効化 |
| 出力設定 | `max_output_tokens`、`temperature`、`top_p` は送信しません |
| 会話圧縮 | `auto` はモデルによる要約を選択。`remote` は未対応としてエラー |
| ツール | ano の function tool と直接接続 MCP（`stdio` / `streamable_http`）に対応 |
| サーバー実行ツール | `transport = "responses"` の MCP などは未対応としてエラー |
| 入力 | テキスト・画像。音声入力は未対応としてエラー |

`previous_response_id` による履歴参照は使えません。ライブラリから直接呼ぶ場合も `input` に履歴全体を渡してください。既存の API 用セッションとは接続先が異なるため、新しいセッションを作成してください。

レート制限・一時的なサーバーエラーは `api.max_retries` の範囲で再試行します。利用上限の解消や追加課金は自動では行わず、API キー接続への自動切り替えも行いません。

API キー接続へ戻す場合は `auth = "api_key"` に変更します。`auth` を省略した既存の設定もこれまでどおり API キー接続になります。

## Rust ライブラリ

`AppConfig::load` で設定を読み込み、`ano::create_client(&config.api)` が返す `Arc<dyn ResponsesApi>` を `Agent::new` に渡してください。API キーと ChatGPT の両方に対応します。`OpenAiClient::from_api_settings` は API キー接続専用です。

## 検証範囲

ローカルのモックサーバーで、デバイス認証、トークン更新と競合防止、認証エラー、ストリーミング、ツール結果と会話履歴の再送を検証しています。実際のアカウントで利用できるモデルや接続の成否は、ログイン後に確認してください。

参考: [OpenAI の認証案内](https://learn.chatgpt.com/docs/auth)、[利用枠の案内](https://learn.chatgpt.com/docs/pricing)。
