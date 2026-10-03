# ano

OpenAI Responses API を使う、Rust 製の自律型 AI エージェントです。モデルが返した function call を自動で実行し、結果を次のリクエストへ返すループで、ファイルの調査・編集・検証のような複数ステップの作業を進めます。CLI・ブラウザの Web UI・Webhook サーバー・Rust ライブラリのいずれとしても使えます。

## 特長

- **自律的な作業。** 作業計画（`task_plan`）と完了判定、ゴール（`--goal` / `/goal`）の達成まで続ける実行、サブエージェント（`delegate_task`）、別の会話でのレビュー（`review_changes`）
- **workspace に閉じた tool。** ファイルの一覧・検索・読み書き・競合検出付きの編集、検証コマンドの実行、承認付きのシェルコマンド実行（`workspace_exec`）と Web ページの取得（`web_fetch`）
- **MCP。** リモート MCP（Responses API 経由）と直接接続（stdio / Streamable HTTP、OAuth 対応）、tool の遅延公開（`tool_search`）
- **承認とポリシー。** ユーザー・実行環境ごとの tool の allowlist / denylist と、確認・判定用モデルによる自動審査・許可・拒否を選べる承認モード
- **接続先。** OpenAI、ChatGPT サブスクリプション（実験的）、LM Studio・Ollama などのローカル AI（`/v1/responses` と `/v1/chat/completions`）。名前付きの接続先・フォールバック・プリセットで切り替え
- **長い作業への対応。** 保存・再開できる会話セッション、長い会話の自動圧縮、トークン上限、chronotope への履歴の保存、手順をスキル（`SKILL.md`）として保存
- **入出力。** 対話モード（`ano chat`）、回答のストリーミング表示、画像・音声入力、JSON 出力、プロジェクト指示（`AGENTS.md`）の自動読み込み
- **連携。** Web UI（`ano web`）、署名付き Webhook と非同期ジョブ API（`ano serve`）、GitHub MCP と組み合わせた Issue の解決から Pull Request の作成まで

## クイックスタート

Rust 1.89 以降と、OpenAI API キー、[ChatGPT サブスクリプション](docs/chatgpt-subscription.md)、または[ローカル AI](docs/configuration.md#ローカル-ailm-studioollama-など)（LM Studio・Ollama など。API キー不要）が必要です。

```sh
cargo install --path .
# macOS
mkdir -p ~/Library/Application\ Support/ano && cp config.example.toml ~/Library/Application\ Support/ano/config.toml
# Linux
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/ano" && cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/ano/config.toml"
export OPENAI_API_KEY="sk-..."
ano run --environment default "README とソースを読み、実装の概要を説明して"
```

- Windows（PowerShell）では `New-Item -ItemType Directory -Force $env:APPDATA\ano\config; Copy-Item config.example.toml $env:APPDATA\ano\config\config.toml`、`$env:OPENAI_API_KEY = "sk-..."` のように読み替えてください。インストールせずに `cargo run -- run ...` でも実行できます。
- ChatGPT の利用枠を使う場合は `ano auth login` でログインし、`[api]` に `auth = "chatgpt"` を追加します（[ChatGPT サブスクリプション](docs/chatgpt-subscription.md)）。
- `config.example.toml` の `default` 環境は、カレントディレクトリを読み取り専用で調べる設定です。ファイルを編集させる場合は `allow_writes = true` の[実行環境](docs/cli.md#実行環境)を用意します。

## 主なコマンド

| コマンド | 内容 |
| --- | --- |
| `ano run [PROMPT]` | タスクを実行します。PROMPT を省略すると stdin から読みます |
| `ano chat` | 同じ会話で複数ターンのやり取りをします（`/model`・`/preset`・`/goal`・`/compact` などのコマンドあり） |
| `ano web` | ブラウザで対話する Web UI を起動します（[docs/web.md](docs/web.md)） |
| `ano serve` | Webhook サーバーを起動します（[docs/webhook.md](docs/webhook.md)） |
| `ano tools` | 利用可能な tool と MCP server を、ポリシーを適用して表示します |
| `ano provider` / `ano model` / `ano preset` | 接続先・モデル・プリセットを、設定ファイルを直接編集せずに管理します（[docs/providers.md](docs/providers.md)） |
| `ano mcp` | MCP server の tool の確認・有効化と OAuth 認可（[docs/mcp.md](docs/mcp.md)） |
| `ano auth` / `ano session` / `ano history` / `ano skills` | ChatGPT へのログイン、保存済みセッション・履歴・スキルの確認 |

```sh
ano chat --environment coding --session .ano/review.json   # 会話を保存し、あとで再開
ano run --allow-writes --allow-exec --goal "cargo test がすべて通る" "失敗しているテストを直して"
ano run --json --quiet "実装の概要を説明して" | jq .outcome
```

全コマンドとオプション、対話モードのコマンド、JSON 出力の形式は [docs/cli.md](docs/cli.md) を参照してください。

## 設定

設定ファイル `config.toml` は、カレントディレクトリ、無ければ OS 標準の設定ディレクトリ（macOS は `~/Library/Application Support/ano`、Linux は `~/.config/ano`、Windows は `%APPDATA%\ano\config`）から読みます。全項目の例は [config.example.toml](config.example.toml)、読み込み順と各セクションは [docs/configuration.md](docs/configuration.md) を参照してください。API キーは設定ファイルに書かず、環境変数か `.env` から読み込みます。

ローカル AI は `[api]` の `base_url` を差し替えるだけで使えます。

```toml
[api]
base_url = "http://127.0.0.1:1234/v1"      # LM Studio。Ollama なら http://127.0.0.1:11434/v1

[agent]
model = "ロードしたモデル名"
```

## ドキュメント

| ドキュメント | 内容 |
| --- | --- |
| [docs/cli.md](docs/cli.md) | コマンドとオプション、対話モード、実行環境、出力とログ |
| [docs/configuration.md](docs/configuration.md) | 設定ファイルの場所とセクション、AGENTS.md、パスと API キー、ローカル AI |
| [docs/providers.md](docs/providers.md) | 複数の接続先、接続先とモデルの管理、フォールバック、プリセットとロール |
| [docs/tools.md](docs/tools.md) | 組み込み tool、コマンド実行、Web ページの取得、GitHub の Issue と Pull Request |
| [docs/agent-runtime.md](docs/agent-runtime.md) | 実行ループの上限・並行実行、セッション、圧縮、トークン上限、作業計画、tool の遅延公開、サブエージェント、スキル |
| [docs/mcp.md](docs/mcp.md) | MCP の接続方式、OAuth 認証、接続の再利用、tool の確認と有効化、承認、ポリシーの名前空間、検索カタログ |
| [docs/chatgpt-subscription.md](docs/chatgpt-subscription.md) | ChatGPT ログイン、利用枠による接続、認証情報の保存、対応範囲 |
| [docs/chronotope-history.md](docs/chronotope-history.md) | 原文会話・ツール履歴の保存と再送、ローカル未送信キュー、`history_*` tool と `ano history` |
| [docs/web.md](docs/web.md) | Web UI の起動とトークン、セッション、API、naui による画面のビルド |
| [docs/webhook.md](docs/webhook.md) | Webhook の API、署名方法（curl / PowerShell）、ジョブの状態と中止 |
| [docs/library.md](docs/library.md) | Rust ライブラリとしての使い方（tool の登録、拡張、`Harness`） |
| [docs/architecture.md](docs/architecture.md) | レイヤー構成、ポート、ディレクトリ構成、設計上の判断 |

## 開発

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

統合テスト（`tests/`）はモックの Responses API を立て、ビルドした `ano` バイナリを実際に起動して検証します。Web UI の画面（`web-ui/`）を変えた場合は `web-ui/build.sh` でビルドし直します（[画面の実装](docs/web.md#画面の実装naui)）。

## セキュリティ上の注意

- ano は起動したユーザーの OS 権限で動きます。`allow_writes`・`allow_exec`・検証コマンド・stdio MCP server は、信頼する workspace とコマンドにだけ設定してください。`workspace_exec` はサンドボックスではありません。
- `approval_mode = "allow"` は確認なしにすべてを許可します。`allow_exec` との併用は、使い捨てのコンテナなど壊れても復元できる環境に限ってください。`auto` モードの判定は補助的な安全策で、完全ではありません。
- リモート MCP server と `web_fetch` は外部へデータを送信できます。Web ページ・Issue・PR・コメントなど第三者が書いた文面に埋め込まれた指示でモデルが動く可能性があるため、公開リポジトリや信頼できない入力を扱う場合は `allow` を避け、tool を必要なものに絞ってください。
- `AGENTS.md` とスキルはモデルへの指示として使われます。信頼できないリポジトリでは `project_instructions = []` にし、保存された `SKILL.md` は `ano skills` で確認してください。
- `ano web` をループバック以外で待ち受けると、トークンと会話は暗号化されない HTTP で流れます（[docs/web.md](docs/web.md#セキュリティ)）。Webhook は必ず secret を設定して公開します（[docs/webhook.md](docs/webhook.md#セキュリティ)）。
- 削除や中止は取り消せず、中止・タイムアウト・トークン上限による停止は完了済みの操作を巻き戻しません。書き込みを許可する環境は、Git などで復元できる workspace にしてください。
