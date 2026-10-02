# ano

OpenAI Responses API を使う、Rust 製の自律型 AI エージェントです。モデルが返した function call を自動で実行し、結果を次のリクエストへ返すループで、ファイルの調査・編集・検証のような複数ステップの作業を進めます。CLI・Webhook サーバー・Rust ライブラリのいずれとしても使えます。

- [特長](#特長)
- [クイックスタート](#クイックスタート)
- [CLI の使い方](#cli-の使い方)
- [設定ファイル](#設定ファイル)
- [組み込み tool](#組み込み-tool)
- [ライブラリとして使う](#ライブラリとして使う)
- [ドキュメント](#ドキュメント)
- [開発](#開発)
- [セキュリティ上の注意](#セキュリティ上の注意)

## 特長

**エージェント実行**
- Responses API の function call を自動実行し、複数の呼び出しを並行処理
- 工程と進捗を記録する作業計画（`task_plan`）と、未完了工程の継続・完了判定
- ゴールの指定（`--goal` / `/goal`）。モデルがゴールから完了条件を定め、すべての達成を確認するまで作業を続ける
- 登録した tool・MCP を検索で必要な分だけ公開する遅延公開（`tool_search`）
- 調査や独立した作業を新しい会話に切り出すサブエージェント（`delegate_task`）
- 保存して別プロセスから再開できる会話セッションと、複数ターンの対話モード（`ano chat`）
- 生成中の回答を端末に逐次表示するストリーミング（Markdown はブロックごとに整形）
- リポジトリの `AGENTS.md` などをプロジェクト固有の指示として自動で読み込み
- 推論モデルの `reasoning.effort` 指定と、推論の要約の進捗表示
- 長い会話の自動圧縮（OpenAI の `/responses/compact`、またはローカル AI でも使えるモデルによる要約）と、使用トークン数に応じた実行停止
- 大きすぎる tool 出力の切り詰め（先頭と末尾を残す）
- chronotope への原文会話・ツール履歴の保存、再送、`history_*` 参照 tool（`[history]` で有効化）
- 上手くいった手順をスキル（Agent Skills 形式の `SKILL.md`）として承認付きで保存し、以後の依頼で参照（`[skills]` で有効化。[スキル](docs/agent-runtime.md#スキルskill_read--skill_save)）

**tool と MCP**
- workspace 内に閉じたファイル一覧・パス名検索（グロブ）・分割読み取り（バイト位置・行番号）・全文検索（正規表現対応、`.gitignore` 対応）・書き込み・移動・削除
- 競合検出付きの正確なファイル編集と、設定で登録した検証コマンド（ビルド・テスト）の実行
- 承認付きのシェルコマンド実行（`workspace_exec`、opt-in）
- 承認付きの Web ページ取得と Markdown 変換（`web_fetch`、opt-in。公開アドレスのみ）
- リモート MCP（Responses API 経由、Secure MCP Tunnel 対応）と、ano からの直接接続（stdio / Streamable HTTP。OAuth 認証・ステートレス server 対応）
- ユーザー・実行環境ごとの tool allowlist / denylist（ワイルドカード対応）と MCP 承認フロー

**入出力と連携**
- テキスト・画像・音声入力（音声は文字起こしせず native `input_audio` として送信）
- GitHub MCP と組み合わせた、Issue の解決から Pull Request 作成・レビューまでの対話的な作業（別の新しい会話でのレビュー `review_changes` を push 前に必須とし、指定ファイルだけをコミットして push する `git_commit_push`）
- 名前付き実行環境を選べる署名付き Webhook と、実行中の進捗確認・中止・タイムアウトに対応した非同期ジョブ API
- LM Studio などの OpenAI 互換 `/v1/responses` endpoint と、`/v1/chat/completions` だけを提供するサーバー（`wire_api = "chat_completions"`）
- 名前付きの複数の接続先（`[providers]`）と、実行ごと・環境ごと・対話の途中（`/provider`・`/model`）での接続先とモデルの切り替え
- 接続先・モデル・推論の強さをまとめて切り替えるプリセット（`[presets]`）と、サブエージェント・レビュー担当・承認の判定用モデルごとのプリセット指定
- ChatGPT サブスクリプションの OAuth 認証による Codex 接続（実験的）
- JSON 出力とログ量の切り替え

## クイックスタート

Rust 1.89 以降と、OpenAI API キー、[ChatGPT サブスクリプション](docs/chatgpt-subscription.md)、または[ローカル AI](#ローカル-ailm-studioollama-など)（LM Studio・Ollama など。API キー不要）が必要です。

```sh
cargo install --path .
# macOS
mkdir -p ~/Library/Application\ Support/ano && cp config.example.toml ~/Library/Application\ Support/ano/config.toml
# Linux
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/ano" && cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/ano/config.toml"
export OPENAI_API_KEY="sk-..."
ano run --environment default "README とソースを読み、実装の概要を説明して"
```

設定ファイルは OS 標準の設定ディレクトリに置きます（[設定ファイル](#設定ファイル)）。インストールせずに `cargo run -- run ...` でも実行できます。Windows（PowerShell）では `New-Item -ItemType Directory -Force $env:APPDATA\ano\config; Copy-Item config.example.toml $env:APPDATA\ano\config\config.toml`、`$env:OPENAI_API_KEY = "sk-..."` のように読み替えてください。

ChatGPT の利用枠を使う場合は `ano auth login` でログインし、`config.toml` の `[api]` に `auth = "chatgpt"` を追加してください。`[agent].model` には契約プランで利用できる Codex モデルを指定します。認証情報は ano 専用のファイルに保存されます。設定・制約は [ChatGPT サブスクリプション](docs/chatgpt-subscription.md) を参照してください。

`config.example.toml` の `default` 環境は、カレントディレクトリを読み取り専用で調べる設定です。ファイルを編集させる場合は[実行環境](#実行環境)で `allow_writes = true` の環境を用意します。

## CLI の使い方

| コマンド | 内容 |
| --- | --- |
| `ano run [PROMPT]` | タスクを実行します。PROMPT を省略すると stdin から読みます |
| `ano chat` | 同じ会話で複数ターンのやり取りをします（[対話モード](#対話モード)） |
| `ano tools` | 利用可能な tool と MCP server を、ポリシーを適用して表示します |
| `ano session PATH` | 保存済みセッションの状態・計画・使用量を表示します（`--json` で全内容） |
| `ano web` | ブラウザで対話する Web UI を起動します。セッションごとに作業フォルダ・権限・モデルを選べます（[docs/web.md](docs/web.md)） |
| `ano serve` | Webhook サーバーを起動します（[docs/webhook.md](docs/webhook.md)） |
| `ano history status/sync/search/get/context/conversations` | chronotope のローカル履歴キューを確認・再送し、原文を参照します（[履歴](docs/chronotope-history.md)） |
| `ano skills [NAME]` | 保存済みのスキルを一覧表示し、NAME を指定するとその内容を表示します（[スキル](docs/agent-runtime.md#スキルskill_read--skill_save)） |
| `ano mcp tools [LABEL]` | MCP server に接続して提供される tool をすべて表示し、設定で有効なものに印を付けます（[tool の確認と有効化](docs/mcp.md#tool-の確認と有効化)） |
| `ano mcp edit LABEL` | MCP server の tool をチェックリストで有効化・無効化し、設定ファイルに保存します（`ano mcp enable/disable LABEL TOOL...` でも可） |
| `ano provider list` | 接続先の一覧（有効・無効、URL、既定モデル、モデルの絞り込み、フォールバック先）を表示します（[接続先の管理](#接続先の管理)） |
| `ano provider add/set/remove/enable/disable NAME` | 設定ファイルを直接編集せずに、接続先を追加・変更・削除・有効化・無効化します |
| `ano preset list/add/set/remove NAME` | 接続先・モデル・推論の強さの組（プリセット）を管理します（[プリセット](#プリセットとロール)） |
| `ano preset edit [NAME]` | プリセットの接続先・モデル・推論の強さを一覧から選んで変更し、設定ファイルに保存します |
| `ano preset role ROLE NAME` | メインのエージェントの既定（`default`）・サブエージェント（`delegate`）・レビュー担当（`review`）・承認の判定（`approval`）に使うプリセットを選びます |
| `ano model list [--provider NAME]` | 接続先に接続して提供されるモデル（と登録したモデル）をすべて表示し、設定で有効なものに印を付けます（[モデルの有効化](#接続先の管理)） |
| `ano model add/remove MODEL... [--provider NAME]` | モデル一覧を返さない接続先（ChatGPT サブスクリプションなど）に、使うモデル名を登録・削除します |
| `ano model edit [--provider NAME]` | 接続先のモデルをチェックリストで有効化・無効化し、設定ファイルに保存します（`ano model enable/disable MODEL... [--provider NAME]` でも可） |
| `ano mcp login LABEL` | OAuth が必要な MCP server を認可し、トークンを保存します（`ano mcp logout LABEL` で削除。[OAuth 認証](docs/mcp.md#oauth-認証)） |

共通オプションは `--config PATH`（設定ファイル）と `--user NAME`（`[users]` のユーザー、既定 `default`）です。

### `ano run` / `ano chat` の主なオプション

| オプション | 内容 |
| --- | --- |
| `--environment NAME` | 設定済みの実行環境（workspace・書き込み権限・MCP 承認方針・model・instructions・検証コマンド）を使う |
| `--workspace PATH` / `--allow-writes` | 環境を指定しない場合の workspace（既定はカレントディレクトリ）と書き込み許可 |
| `--allow-exec` | 環境を指定しない場合に、`workspace_exec` でのコマンド実行を許可（[コマンド実行](#コマンド実行workspace_exec)） |
| `--preset NAME` | `[presets]` のプリセット（接続先・モデル・推論の強さ）を使う。`default` は `[agent]` の設定。`--provider`・`--model`・`--reasoning-effort` はその上に重なる（[プリセット](#プリセットとロール)） |
| `--provider NAME` | `[providers]` の接続先を使う（省略時は既定の接続先、`api` は `[api]`）。接続先に `model` があれば、そのモデルに切り替わる（[接続先の切り替え](#複数の接続先を切り替える)） |
| `--model NAME` | モデルを変更（選んだ接続先でのモデル名） |
| `--reasoning-effort LEVEL` | 推論の深さ（`none`・`minimal`・`low`・`medium`・`high`・`xhigh`・`max`・`ultra`。対応範囲はモデルによる。`max`・`ultra` は ChatGPT サブスクリプションの Codex モデルなど） |
| `--goal TEXT` | ゴール（最終的にどうなっていればよいか。満たすべき条件も文中に書ける）を指定し、達成が確認されるまで作業を続ける。プロンプトは省略可（`run` のみ。chat では `/goal`。[ゴールと完了条件](docs/agent-runtime.md#ゴールと完了条件)） |
| `--image PATH` / `--audio PATH` | 画像・音声を入力に追加（複数指定可、`run` のみ） |
| `--disable-tool NAME` | この実行だけ tool を無効化（複数指定可） |
| `--session PATH` / `--recover-session` | 会話を保存・再開（[セッション](docs/agent-runtime.md#会話セッション)） |
| `--compact-threshold-bytes N` / `--max-total-tokens N` | 履歴の圧縮とトークン上限（[圧縮と上限](docs/agent-runtime.md#履歴の圧縮)） |
| `--max-tool-rounds N` | この実行で送る Responses 要求の回数の上限（`agent.max_tool_rounds` を上書き。既定は 100。最後の1回は tool を使わない報告に充てる）。暴走を止める歯止めで、費用の予算には `--max-total-tokens` を使う |
| `--approval-mode MODE` | MCP 呼び出しの承認方法。`ask`（確認）・`auto`（判定用モデルが審査し、迷うものだけ確認。既定）・`allow`・`deny`（[承認モード](docs/mcp.md#承認モード)） |
| `--auto-approve-mcp` / `--non-interactive` | `--approval-mode allow` / `deny` と同じ |
| `--json` | 結果を1つの JSON オブジェクトとして stdout へ出力（`run` のみ） |
| `--quiet` / `--verbose` | 進捗ログを省略 / 途中経過もすべて残し、引数と結果を含めて詳しく表示 |
| `--raw` | 回答の Markdown を整形せずにそのまま出力。stdout が端末のときは、既定で回答を生成しながら表示し、見出し・太字・リスト・表を端末向けに整形します（パイプ先や `--json` では完了後にそのまま出力。`NO_COLOR` を設定すると色と文字装飾を省略。[ストリーミング](docs/agent-runtime.md#回答のストリーミング)） |

```sh
ano run "この画像を説明して" --image ./diagram.png
ano run "この音声の内容に答えて" --audio ./instruction.wav --model gpt-audio-1.5
ano run --disable-tool echo "echo は使わずに答えて"
ano tools --environment default
```

### 対話モード

`ano chat` は1つの会話を保ったまま、続けて指示を出せるモードです。前のターンの調査結果や作業計画を引き継ぎます。

```sh
ano chat --environment coding
ano chat --environment coding --session .ano/review.json   # 終了後も会話を保存し、あとで再開
```

| 入力 | 動作 |
| --- | --- |
| テキスト | エージェントへの指示として送信 |
| `/plan` / `/usage` | 作業計画 / この会話のトークン使用量を表示 |
| `/model [NAME]` | 現在のモデルを表示 / 今の接続先のまま NAME に切り替える |
| `/models` | 今の接続先が提供するモデルの一覧（使用中と無効なものに印を付ける） |
| `/provider [NAME [MODEL]]` | 接続先の一覧を表示 / 接続先 NAME（とモデル MODEL）に切り替えて会話を続ける（[接続先の切り替え](#複数の接続先を切り替える)） |
| `/preset [NAME]` | プリセットの一覧を表示（今の設定と一致するものに印） / プリセット NAME の接続先・モデル・推論の強さに切り替える。`/preset default` で設定の既定に戻る（[プリセット](#プリセットとロール)） |
| `/effort [LEVEL]` | 推論の強さを表示 / LEVEL に変える（モデルはそのまま） |
| `/goal TEXT` | ゴールを指定して作業を始める。`/goal` で表示、`/goal clear` で解除 |
| `/skill [観点]` | この会話で上手くいった手順をスキルとして保存するよう依頼する（`[skills]` 有効時。保存は承認モードで確認） |
| `/compact` | 会話を今すぐ圧縮する（要約などで履歴を小さくし、コンテキストを空ける。[履歴の圧縮](docs/agent-runtime.md#履歴の圧縮)） |
| `/clear` | 新しい会話を始める（`--session` 指定時は使えません） |
| `/help` | コマンド一覧 |
| ↑ / ↓ | 以前の入力を呼び出す |
| `/exit`、Ctrl+D | 終了 |
| Ctrl+C | 実行中のターンだけを中断し、会話は続ける（完了した操作は巻き戻しません） |

承認モードの既定は `auto` です。判定用モデルが依頼の範囲内で危険の少ない呼び出しを自動で承認し、影響の大きい操作だけを確認します（[自動承認](docs/mcp.md#自動承認auto)）。すべてを自分で確認するには `--approval-mode ask` か、設定の `[agent] approval_mode = "ask"` を使います。端末では全角文字の表示幅を考慮する行編集を使うため、IME での日本語入力や削除も正しく表示されます。`--session` を付けない場合、会話はプロセス内のメモリだけに保持されます。MCP の承認は同じ端末で確認します。stdin をパイプで渡すと、1行ずつ指示として処理します（承認は拒否されます）。

### 画像・音声入力

音声入力には `input_audio` に対応したモデル（例: `gpt-audio-1.5`）を指定してください。画像と音声を同じリクエストで使う場合は、両方に対応したモデルが必要です。

### 実行環境

`--environment` を指定すると、Webhook と同じ環境設定で実行します。ユーザーと環境の両方が許可した tool だけが使えます。`workspace` を省略した環境は、CLI ではカレントディレクトリを workspace にします（Webhook のジョブでは workspace なし）。環境の権限を CLI から広げられないよう、`--workspace`・`--allow-writes`・`--allow-exec`・`--approval-mode`・`--auto-approve-mcp` との併用はエラーになります。`--preset`・`--provider`・`--model`・`--reasoning-effort` は併用でき、`--non-interactive` で環境の MCP 自動承認も無効にできます。環境の `preset`・`provider`・`model`・`reasoning_effort` で、その環境の既定の接続先・モデル・推論の強さを指定できます（Webhook のジョブもこれに従います）。

```toml
[environments.coding]
workspace = "/path/to/my-repository"
allowed_tools = ["workspace_*"]
allow_writes = true
allow_exec = true          # workspace_exec を使う（コマンドごとに承認モードで判定）
approval_mode = "auto"

[environments.coding.checks.test]
program = "cargo"
args = ["test", "--locked"]
timeout_secs = 900
```

### 出力とログ

進捗ログ（tool 名と状態、モデルの途中経過、作業計画の更新）は stderr に出力します。stdout が端末なら、回答は生成しながら stdout に表示します。`--json` の出力は次のフィールドを持ちます。

| フィールド | 内容 |
| --- | --- |
| `text` | 最終回答 |
| `response_id` | 最後の応答の ID |
| `events` | tool 呼び出しなどのイベント（引数と結果を含む） |
| `outcome` | 作業計画から見た完了状態（`completed` / `blocked` / `incomplete`） |
| `plan` | 作業計画（ゴールがあれば `plan.goal` に完了条件ごとの状態と根拠） |
| `usage` | この実行のトークン使用量 |
| `stop_reason` | 停止理由（`final_answer` / `round_limit` / `token_limit` / `usage_unavailable` / `no_progress`） |

```sh
ano run --environment default --json --quiet "実装の概要を説明して" | jq .outcome
```

実行エラーは非ゼロの終了コードと stderr のメッセージで返ります。正常終了でも未完了の工程が残る場合があるため、自動処理では `outcome` も確認してください（[作業計画と完了判定](docs/agent-runtime.md#作業計画と完了判定)）。

## 設定ファイル

`--config` を省略すると、次の順に最初に見つかった `config.toml` を読みます。どちらも無ければ組み込みの既定値で動きます。全項目の例は [config.example.toml](config.example.toml) を参照してください。

1. カレントディレクトリの `config.toml`（プロジェクトごとの設定）
2. OS 標準の設定ディレクトリの `config.toml`

| OS | 設定ディレクトリ |
| --- | --- |
| macOS | `~/Library/Application Support/ano` |
| Linux | `$XDG_CONFIG_HOME/ano`（既定 `~/.config/ano`） |
| Windows | `%APPDATA%\ano\config` |

`ano provider`・`ano model`・`ano preset`・`ano mcp` は読み込んだ設定ファイルを書き換えます。どこにも無い場合、`ano provider`・`ano model`・`ano preset` は OS 標準の設定ディレクトリに作成します（`ano mcp` は設定済みの MCP サーバーが必要です）。

| セクション | 内容 | 詳細 |
| --- | --- | --- |
| `[api]` | endpoint・API キーの環境変数名・タイムアウト・リトライ・ストリーミング | [ローカル AI](#ローカル-ailm-studioollama-など) |
| `[providers.<name>]` | `[api]` とは別の名前付き接続先と、その既定モデル・使えるモデル・フォールバック先 | [接続先の切り替え](#複数の接続先を切り替える) |
| `[presets.<name>]` / `[agent.roles]` | 接続先・モデル・推論の強さの組と、メインのエージェント・サブエージェント・レビュー担当・承認の判定に使うプリセット | [プリセット](#プリセットとロール) |
| `[agent]` | モデル・instructions・推論設定・プロジェクト指示・承認モード・実行ラウンド数・並行数・tool 出力の上限・圧縮・トークン上限 | [docs/agent-runtime.md](docs/agent-runtime.md) |
| `[environments.<name>]` | workspace・許可する tool・書き込み・コマンド実行・承認モード・検証コマンド | [実行環境](#実行環境) |
| `[users.<id>]` | ユーザーごとの `allowed_tools` / `disabled_tools` | [ポリシーの名前空間](docs/mcp.md#ポリシーの名前空間) |
| `[[mcp_servers]]` | MCP server の接続方式・許可する tool・承認 | [docs/mcp.md](docs/mcp.md) |
| `[webhook]` | 待ち受けアドレス・署名・ジョブ数とタイムアウト | [docs/webhook.md](docs/webhook.md) |
| `[history]` | chronotope の接続先・主体・ローカル未送信キュー・タイムアウト | [docs/chronotope-history.md](docs/chronotope-history.md) |
| `[skills]` | スキルの有効化と保存先 | [スキル](docs/agent-runtime.md#スキルskill_read--skill_save) |

設定の誤りで意図せず制限が外れないよう、次の場合はエラーになります。

- `--config` で明示したファイルが存在しない（ポリシーなしの既定値へ黙って切り替えないため）
- 未知のキーがある（`disable_tools` のような綴り間違いで制限が無効になることを防ぐため）
- `--user` に未定義の名前を指定した（別ユーザーの設定へ切り替えないため）

### プロジェクト指示（AGENTS.md）

workspace のルートに `AGENTS.md` があると、その内容をプロジェクト固有の指示として instructions の末尾に追加します。コーディング規約・ビルド方法・触れてはいけないファイルなどを書いておくと、毎回プロンプトで説明する必要がなくなります。

```toml
[agent]
project_instructions = ["AGENTS.md", "docs/agent-rules.md"]   # 既定は ["AGENTS.md"]、[] で無効
```

- 存在しないファイルは無視します。workspace の外を指すリンク、UTF-8 でないファイル、合計 64 KiB を超える場合はエラーになります。
- 追加された指示は「運用者の instructions とユーザーの依頼に反しない範囲で従う」ものとして扱われます。信頼できないリポジトリでは `project_instructions = []` にしてください。

### パスと API キー

`environments.*.workspace` と MCP の `cwd` の相対パスは、設定ファイルのあるディレクトリを基準に解決します。OS 標準の設定ディレクトリに置いた設定では、絶対パスか `~/...` で指定してください。先頭の `~`（`~` と `~/...`）はホームディレクトリに展開します（MCP の `command` も同様）。CLI の `--workspace` は起動ディレクトリを基準にします。

API キーは設定ファイルに保存せず、`api_key_env` で指定した環境変数から読み込みます。環境変数は `.env` にも書けます。起動時にカレントディレクトリ（無ければ親ディレクトリ）の `.env`、続いて OS 標準の設定ディレクトリの `.env`（`config.toml` の隣）を読みます。すでに設定されている変数は上書きしないため、優先順はシェルの環境変数、カレントの `.env`、設定ディレクトリの `.env` の順です。設定ディレクトリの `.env` は、どのディレクトリから起動しても使う API キー（MCP の `url` が参照する変数を含む）の置き場所に向いています。秘密情報を含むので、所有者だけが読めるようにしてください（`chmod 600`）。`OPENAI_BASE_URL` を設定すると、設定ファイルの `[api]` の `base_url` より優先して endpoint を変更できます（モックサーバーなど）。`[providers]` の接続先は `OPENAI_BASE_URL` の影響を受けません。

### ローカル AI（LM Studio・Ollama など）

OpenAI 互換の `/v1/responses` を提供するサーバーなら、`[api]` の `base_url` を差し替えて使えます。

```toml
[api]
base_url = "http://127.0.0.1:1234/v1"      # LM Studio。Ollama なら http://127.0.0.1:11434/v1

[agent]
model = "ロードしたモデル名"
```

- **API キーは不要です。** キーが必須なのは OpenAI 公式の endpoint（`api.openai.com`）だけで、それ以外は `api_key_env` の環境変数が未設定なら Authorization ヘッダーを付けずに送ります。LAN 内の別マシンや Docker 上のサーバーでも同じです。サーバー側で認証を有効にしている場合は、`api_key_env` に指定した環境変数にキーを設定してください。
- `config.toml` はカレントディレクトリ、無ければ OS 標準の設定ディレクトリから読みます（[設定ファイル](#設定ファイル)）。どこからでも同じ接続先を使うには OS 標準の設定ディレクトリに置くか、環境変数 `OPENAI_BASE_URL` で endpoint を指定してください。設定が読まれていないと既定の OpenAI endpoint に接続しようとして、API キーがないというエラーになります。
- LM Studio で Remote MCP を使う場合は、Server Settings で MCP 利用を有効にします。
- 回答はストリーミングで表示します（`stream: true` に対応していない server でも動きます）。
- 長い会話の圧縮は、`/responses/compact` の代わりにモデル自身が書く要約で行います（`[agent] compaction = "auto"` の既定動作）。LM Studio が返す読み込み中のコンテキスト長に近づいたところで圧縮します。llama.cpp の server も `/v1/models` の `meta.n_ctx` から取得します。ほかのサーバーでは `context_window` を指定してください（[履歴の圧縮](docs/agent-runtime.md#履歴の圧縮)）。

`/v1/responses` がなく `/v1/chat/completions` だけを提供するサーバー（llama.cpp・vLLM の一部の構成や、Chat Completions 互換のプロキシなど）には、`wire_api = "chat_completions"` を指定します（`ano provider add NAME --base-url URL --wire-api chat-completions` でも設定できます）。

```toml
[providers.chat]
base_url = "http://192.168.1.10:8080/v1"
wire_api = "chat_completions"
model = "ロードしたモデル名"
```

- 要求と応答を Responses API の形式との間で変換するため、tool の実行・ストリーミング表示・セッション・要約による圧縮はそのまま使えます。サーバーは応答を保存しないので、毎回履歴全体を送ります。
- 推論（`reasoning_content` または `reasoning`）は進捗として表示し、次の要求で assistant メッセージの `reasoning_content` として返します。推論の強さは `reasoning_effort` として送ります。
- 使えるのは function tool（ano の tool と、stdio / Streamable HTTP で直接接続した MCP）だけです。Responses API 管理方式の MCP（既定の `transport`）は使えません。

ロードしたモデル名は `agent.model` または `--model` で指定します。tool calling の品質はモデルの tool use 対応に依存します（native tool use 対応モデルを推奨）。URL 形式の MCP は `url` で登録できますが、Secure MCP Tunnel（`tunnel_id`）は OpenAI Responses API の機能で、LM Studio では使えません。

### 複数の接続先を切り替える

`[api]` のほかに、名前付きの接続先を `[providers.<name>]` に登録できます。`--provider` を付けない実行では、`[agent].provider` に指定した接続先（既定の接続先）を使います。指定がなければ `[api]` に接続し、この接続先は `api` という名前で選べます。

```toml
[agent]
provider = "lan"                             # 既定の接続先（`ano provider use lan` でも設定できる）
```

```toml
[providers.lan]
base_url = "http://192.168.1.10:1234/v1"    # LAN の LM Studio
model = "qwen/qwen3-coder-30b"               # この接続先へ切り替えたときのモデル（省略可）

[providers.codex]
auth = "chatgpt"                             # ChatGPT サブスクリプション（base_url は指定不可）
model = "gpt-5-codex"

[environments.review]
provider = "codex"                           # この環境の既定の接続先
```

```sh
ano run --provider lan "テストを実行して結果をまとめて"
ano run --provider lan --model qwen3:30b "..."   # 接続先とモデルを両方指定
ano chat --environment review                    # 環境の provider（codex）を使う
```

- `[api]` を使っていない場合（既定の接続先でもなく、environment やフォールバック先からも参照されていない場合）、一覧には `api` を表示しません。`[api]` の接続設定を名前付きの接続先に移すには `ano provider rename api NAME` を使います（下記）。
- 指定できる項目は `[api]` と同じ（`auth`・`chatgpt_auth_file`・`base_url`・`api_key_env`・`wire_api`・`timeout_secs`・`max_retries`・`stream`・`context_window`）に加え、`model` と `approval_model`（`approval_mode = "auto"` の判定用モデル）です。`timeout_secs`・`max_retries`・`stream` を省略すると `[api]` の値を引き継ぎます。`base_url`・`api_key_env`・`wire_api`・`context_window` は引き継がず、`base_url` と `api_key_env` の省略時は OpenAI の既定値になります。`context_window` はモデルが1回の要求で受け取れるトークン数で、履歴をその手前で圧縮します（LM Studio は自動で取得。[履歴の圧縮](docs/agent-runtime.md#履歴の圧縮)）。
- モデルは「環境の指定 → `--preset`/`--provider`/`--model` → 対話中の `/preset`/`/provider`/`/model`」の順に上書きします（[プリセット](#プリセットとロール)）。接続先だけを選んだ場合は、その接続先の `model` に切り替わります。接続先に `model` がなければ、それまでのモデルをそのまま使います。
- `[agent].model` は `[api]` のモデルで、`model` のない既定の接続先でも使います。`[agent].approval_model` は `[api]` 用です。ほかの接続先では、その接続先の `approval_model`（省略時は使用中のモデル）で審査します。
- `ano chat` では `/provider lan` や `/model qwen3:30b` で、会話を保ったまま切り替えられます。接続先を変えると、それまでの会話が新しい接続先へ送られます。元の接続先でしか読めない暗号化された推論は履歴から除きます。OpenAI の `/responses/compact` で圧縮済みの会話は、ほかの接続先では読めないため移せません（`/clear` か新しいセッションで始めてください）。
- `--session` の会話は、切り替えた接続先・モデル・推論の強さを記録し、次に `--preset`・`--provider`・`--model` を付けずに再開したときもそれを使います（`--reasoning-effort` だけを付けた場合は、記録した接続先とモデルのまま推論の強さだけを変えます）。保存時と異なる接続先で再開するには `--provider` を明示してください。明示しない限り、別の接続先へ会話を送ることはありません。
- `ano serve` は、環境とロールが参照する接続先へ起動時に接続を確認し、ジョブごとに環境の接続先・モデル・推論の強さを使います。無効にした接続先やモデルを選ぶ環境は起動を止めず、その環境のジョブがエラーになります（あとで有効にすれば再起動は不要です）。

#### 接続先の管理

`ano provider` で接続先を、`ano model` で接続先ごとのモデルを、設定ファイルを直接編集せずに管理できます。設定ファイルのコメントや書式は保ったまま該当箇所だけを書き換えます。書き換え後の設定が不正になる場合は保存しません。設定ファイルがなければ OS 標準の設定ディレクトリに作成します（[設定ファイル](#設定ファイル)）。

```sh
ano provider add lan --base-url http://192.168.1.10:1234/v1 --model qwen/qwen3-coder-30b --fallback codex
ano provider add codex --auth chatgpt --model gpt-5-codex
ano provider use lan                                       # 既定の接続先にする（[agent].provider）
ano provider set lan --timeout-secs 120 --unset fallback   # 変更と削除（api は [api] と [agent].model）
ano provider list                                          # 既定の接続先に (default)
ano provider rename lan desktop                            # 改名（environment・フォールバック先・既定の指定も追従）
ano provider remove codex                                  # environment やフォールバック先が参照していれば拒否
```

`[api]` に接続先（ChatGPT サブスクリプションなど）を設定している場合は、`ano provider rename api chatgpt` で名前付きの接続先に移せます。`[api]` の接続設定（`auth`・`base_url`・`models`・`fallback` など）を `[providers.chatgpt]` に移し、`[agent].model` をそのモデルとして引き継ぎます。`[api]` が既定だった場合は、`chatgpt` が既定の接続先になります。`timeout_secs`・`max_retries`・`stream` は、全接続先で共通の既定値として `[api]` に残します。

tool と同じように、接続先とモデルを有効化・無効化できます。無効にした接続先やモデルは、`--provider`・`--model`・environment・`/provider`・`/model` のどこから選んでもエラーになり、フォールバック先からも外れます。`ano model` の `--provider` は、`ano run`・`ano chat` と同じく接続先を選びます。省略すると、`list` はすべての有効な接続先、それ以外は既定の接続先が対象です。

```sh
ano model list                                   # すべての接続先の /v1/models を取得し、有効なものに [x]
ano model list --provider lan
ano model edit --provider lan                    # チェックリストで使うモデルを選ぶ
ano model disable qwen/qwen3-4b --provider lan
ano model enable 'qwen/*' --provider lan         # 末尾の * で前方一致
ano model add gpt-5.6-luna gpt-5.6-mini --provider codex   # 一覧を返さない接続先にモデル名を登録
ano provider disable codex                       # 接続先そのものを無効化（設定は残る）
```

- 設定ファイルでは `allowed_models`（許可リスト。指定時はこれに一致するモデルだけ）と `disabled_models`（拒否リスト）で表します。MCP の `allowed_tools`・`disabled_tools` と同じ考え方で、`[api]` にも書けます。接続先そのものの無効化は `enabled = false` です。
- モデル一覧を返さない接続先（ChatGPT サブスクリプションや、`/v1/models` のないサーバー）では、`ano model add` で登録したモデル名（設定の `models`）を一覧として使います。一覧を返す接続先でも、一覧にないモデル名を登録して並べられます。
- `enable`・`disable` は、モデル名を「接続先の一覧＋登録したモデル」と照合します。未起動のサーバーで照合を省くには `--no-verify` を付けてください。

#### フォールバック

`fallback` に接続先の名前を並べると、接続できない、タイムアウトした、または過負荷（408・429・5xx。リトライ後）の場合に、次の接続先で同じ要求を続けます。

```toml
[providers.lan]
base_url = "http://192.168.1.10:1234/v1"
model = "qwen/qwen3-coder-30b"
fallback = ["lan2", "codex"]      # lan が使えなければ lan2、次に codex
```

- 切り替え先では、その接続先の `model` を使います（`api` なら `[agent].model`）。`model` がない接続先では、要求と同じモデル名を使います。
- 400 などの要求そのものの誤りでは切り替えません。ストリーミングで回答の表示が始まった後も、表示の重複を避けるため切り替えません。
- 失敗した接続先は5分間、ほかの接続先の後に回します。毎回接続のタイムアウトを待つことはありません。
- 会話は元の接続先に紐づいたままです。切り替え先には、元の接続先でしか読めない暗号化された推論を除いた履歴を送ります。切り替え先の推論も履歴には残しません。OpenAI の `/responses/compact` で圧縮済みの会話と `/responses/compact` 自体は、元の接続先でしか扱えないため切り替えません。
- フォールバック先は、設定した順に1段だけたどります（フォールバック先の `fallback` はたどりません）。無効な接続先や、使うモデルが無効な接続先は飛ばします。起動時にログインや API キーの不足で使えない接続先は、警告を出して飛ばします。
- 会話の内容が別の接続先へ送られるため、送り先として問題のない接続先だけを `fallback` に並べてください。

### プリセットとロール

接続先・モデル・推論の強さ（`reasoning_effort`）は、組にして `[presets.<name>]` に登録できます。調査は LAN のローカルモデルで低く、設計の判断は Codex で高く、のように用途ごとの組を名前で切り替えられます。

```toml
[presets.quick]
provider = "lan"
model = "qwen/qwen3-4b"
reasoning_effort = "low"
description = "範囲の狭い調査"          # 一覧に表示（省略可）

[presets.deep]
provider = "codex"
model = "gpt-5-codex"
reasoning_effort = "high"

[presets.lighter]
reasoning_effort = "minimal"             # 推論の強さだけ（接続先とモデルはそのまま）

[agent.roles]                            # ロールごとのプリセット（`ano preset role ROLE NAME` でも設定できる）
default = "quick"                        # メインのエージェントの既定（--preset などで選ばない実行）
delegate = "quick"                       # delegate_task のサブエージェント
review = "deep"                          # review_changes のレビュー担当
approval = "lighter"                     # approval_mode = "auto" の判定用モデル

[environments.review]
preset = "deep"                          # この環境の既定（provider・model・reasoning_effort で上書き可）
```

```sh
ano run --preset deep "この設計の問題点を洗い出して"
ano run --preset quick --reasoning-effort medium "..."   # プリセットの上に個別の指定を重ねる
ano preset add deep --provider codex --model gpt-5-codex --reasoning-effort high
ano preset edit deep                                     # 接続先・モデル・推論の強さを一覧から選び直す
ano preset set deep --reasoning-effort xhigh             # 項目を指定して変更（--unset FIELD で削除）
ano preset role review deep                              # レビュー担当のプリセット（--unset で解除）
ano preset role default quick                            # 既定のプリセット（`default` で [agent] の設定に戻す）
ano preset list
```

- プリセットは、下の層で選んだ接続先・モデル・推論の強さの上に、書いた項目だけを重ねます。接続先だけを書いたプリセットはその接続先の `model` に切り替わり、`reasoning_effort` だけを書いたプリセットはモデルを変えずに推論の強さだけを変えます。どれも書かないプリセットはエラーです。
- `default` は予約された名前で、`[agent]`（`provider`・`model`・`reasoning_effort` と、ロール `default` のプリセット）と実行環境の指定から決まる既定の組を表します。`--preset default` や `/preset default` で、セッションに記録した組や対話中の切り替えから既定に戻せます。`[presets.default]` は定義できません。
- ロール `default` は、`--preset` などで選ばない実行でメインのエージェントが使うプリセットで、`[agent]` の設定の上に重なります。
- それ以外のロールを指定しないと、サブエージェントとレビュー担当はメインのエージェントと同じ接続先・モデル・推論の強さで動き、承認の判定は従来どおり `approval_model` で行います。指定すると、そのプリセットをメインのエージェントの**現在の**組の上に重ねたもので動きます。`/preset` などで切り替えると、ロールもそれに合わせて決め直します。これらのロールにプリセット `default` を指定すると、対話中の切り替えに関わらず既定の組を使います。
- 自分の差分を同じモデルに確認させると見落としも同じになりがちなので、`review` だけ別のモデルにする使い方が効果的です。レビュー担当やサブエージェントを別の接続先にすると、その接続先へ作業の内容（依頼・差分・ファイルの内容）が送られる点に注意してください。
- `ano preset edit` は、接続先を有効な接続先から、モデルをその接続先が提供する有効なモデル（と登録したモデル）から選びます。接続先を「not set」にした場合は `[agent]` の接続先のモデルを並べます。どの一覧にも、そのプリセットの今の値を含めます（Enter だけで今の値のまま進めるように、無効にした接続先やモデルでも表示し、保存後に使えない旨を警告します）。一覧にないモデル名は入力でき、「not set」を選んだ項目は設定から外します。端末でない場合は `ano preset set` を使ってください。
- `ano preset remove` は、ロール・環境が参照しているプリセットを削除しません。`ano provider rename` はプリセットの `provider` も書き換えます。既定のプリセットが接続先を指定している間は、`ano provider use` は使えません（`ano preset role default NAME` で既定のプリセットを変えてください）。

## 組み込み tool

| tool | 内容 | 条件 |
| --- | --- | --- |
| `workspace_list` | ディレクトリ直下を名前順に取得（既定100件、最大1000件）。`next_after` を次の `after` に渡すと続きを取得 | workspace |
| `workspace_find` | パスのグロブ（`*.rs`、`src/**/*.ts`、`*.{toml,md}`）でファイル・ディレクトリを再帰的に探す（既定200件、最大1000件）。`.gitignore` の対象は `include_ignored:true` のときだけ含める | workspace |
| `workspace_read` | UTF-8 ファイルを `offset`（バイト位置）と `max_bytes`（既定 64 KiB、最大 10 MiB）で分割して読む。`next_offset` で続きを読み、文字の途中では分割しない。`start_line`（1始まり）と `max_lines` を指定すると行単位で読み、`next_line`・`total_lines` と全体の `sha256` を返す | workspace |
| `workspace_search` | UTF-8 テキストを再帰的に検索し、パス・行番号・列番号・抜粋を返す。既定は大文字小文字を区別する文字列検索で、`regex:true`（Rust の正規表現）、`ignore_case:true`、`include`（対象ファイルのグロブ）、`include_ignored`（`.gitignore` の対象も検索）を指定できる（既定100件、最大1000件） | workspace |
| `workspace_edit` | 置換対象がちょうど1回だけ出現することと、必要なら `expected_sha256` の一致を確認してから原子的に書き込む。`dry_run:true` で差分とハッシュを確認できる | `allow_writes` |
| `workspace_write` | UTF-8 テキストをファイルへ書き込む | `allow_writes` |
| `workspace_move` | ファイル・ディレクトリを移動（リネーム）。既存ファイルの上書きは `overwrite:true` のときだけ | `allow_writes` |
| `workspace_delete` | ファイル・リンク・空ディレクトリを削除。中身のあるディレクトリは `recursive:true` が必要。取り消しはできない | `allow_writes` |
| `workspace_check` | 環境の `checks` に登録した検証コマンドを実行。`name:null` で一覧 | `checks` |
| `workspace_exec` | シェルコマンドを workspace で実行し、終了コードと出力を返す。呼び出しごとに承認が必要（[詳細](#コマンド実行workspace_exec)） | `allow_exec` |
| `web_fetch` | 公開 Web ページを取得し、HTML を Markdown に変換して返す。`offset`・`max_bytes` で分割して読む。呼び出しごとに承認が必要（[詳細](#web-ページの取得web_fetch)） | 常時（承認で判定） |
| `git_diff` | 未コミットの変更（新規ファイルを含む）の差分と、ファイルごとの状態と sha256（内容と実行ビットから計算）を返す | workspace |
| `git_commit_push` | 指定したファイルだけをコミットし、ブランチを remote へ push する。既定ブランチには直接コミットしない。`review_changes` を受けた内容のファイルだけをコミットできる。呼び出しごとに承認が必要（[詳細](#github-の-issue-と-pull-request)） | `allow_writes` |
| `skill_read` | 保存済みスキルの手順を名前で読む（[詳細](docs/agent-runtime.md#スキルskill_read--skill_save)） | `[skills]` |
| `skill_save` | 上手くいった手順をスキルとして保存・更新する。呼び出しごとに承認が必要 | `[skills]` |
| `task_plan` | 作業計画の読み書き（[詳細](docs/agent-runtime.md#作業計画と完了判定)） | 常時 |
| `tool_search` | 登録済み tool・MCP の検索（[詳細](docs/agent-runtime.md#tool-の遅延公開tool_search)） | 常時 |
| `delegate_task` | 作業をサブエージェントに任せ、報告を受け取る（[詳細](docs/agent-runtime.md#サブエージェントdelegate_task)） | 常時 |
| `review_changes` | 未コミットの変更を、新しい会話の読み取り専用のレビュー担当に確認させ、指摘を受け取る（[詳細](docs/agent-runtime.md#変更のレビューreview_changes)） | `allow_writes` |
| `echo` / `unix_time` | 動作確認用 | なし |

実際に使える tool は、ユーザーと環境の `allowed_tools` / `disabled_tools` で決まります。読み取り専用の環境で検索を使うには `allowed_tools` に `workspace_search`・`workspace_find` を加えてください。`workspace_*` は移動・削除・コマンド実行（`allow_exec` のとき）も許可する点に注意してください。

- **workspace の外には出ません。** 絶対パスや `..` を拒否し、既存の親ディレクトリを1階層ずつ正規化して workspace 内であることを確認します。シンボリックリンクを経由した書き込みも拒否し、リンクの削除・移動ではリンク先に触れません。
- **`.git` の中は書き込み・移動・削除できません。** hook や `.git/config` の書き換えで、次の git 操作時にコマンドが実行されるのを防ぐためです。workspace ルートも移動・削除できません。
- **検索量に上限があります。** `workspace_search`・`workspace_find` は 10,000 エントリ（検索はさらに 32 MiB）までを走査し、リンク・バイナリ・10 MiB 超のファイルと、`.git`・`node_modules`・`target` などの生成物ディレクトリを省略します。上限に達したら範囲を狭めて再検索します。
- **`.gitignore` に従います。** workspace 内の各ディレクトリの `.gitignore` と `.git/info/exclude` に一致するファイル・ディレクトリは検索しません（Git リポジトリでなくても適用）。`path` で明示したディレクトリは、それ自体が無視対象でも検索します。省いた数は結果の `ignored` / `skipped_ignored` に入ります。
- **検証コマンドは設定で固定されます。** `workspace_check` のコマンドと引数は設定ファイルで決まり、workspace を作業ディレクトリとして実行し、出力は上限付きで返します。検証ごとの `timeout_secs` を優先し、タイムアウト時も取得済みの出力を返します。Webhook のジョブ全体の制限は引き続き適用されます。

### コマンド実行（workspace_exec）

`git`・ビルド・個別のテスト・プロジェクトのスクリプトなど、他の tool で扱えない操作のために、シェルコマンドを実行できます。既定では無効で、環境の `allow_exec = true` か、環境を指定しない CLI 実行の `--allow-exec` で有効になります。

```sh
ano chat --allow-writes --allow-exec                          # コマンドごとに [y/N] で確認
ano run --allow-exec --approval-mode auto "テストを実行して失敗を直して"  # 判定用モデルが審査
```

- **コマンドごとに承認が必要です。** MCP と同じ[承認モード](docs/mcp.md#承認モード)（`ask`・`auto`・`allow`・`deny`）で判定し、拒否されたコマンドは実行しません。環境の既定は `deny` なので、Webhook などで使う場合は `approval_mode = "auto"` などを設定します。`auto` の判定用モデルは、調査・ビルド・テストを許可し、依頼にない削除・履歴の書き換え・push・インストール・ネットワーク接続などは確認に回します。
- Unix では `/bin/sh -c`、Windows では `cmd /C` で実行します。作業ディレクトリは workspace（`cwd` で workspace 内のサブディレクトリを指定可）で、stdin は閉じています。
- `timeout_secs`（既定120秒、最大1800秒）で打ち切り、それまでの出力を返します。Unix ではコマンドを専用のプロセスグループで起動し、終了・タイムアウト・中断（Ctrl+C）の時点で、コマンドが残したバックグラウンドプロセスも停止します。
- 出力は stdout・stderr それぞれ先頭 16 KiB と末尾 48 KiB を返します（エラーは末尾に出ることが多いため）。`workspace_check` の出力も同じ形式です。
- 名前に `KEY`・`SECRET`・`TOKEN`・`PASSWORD`・`PASSWD`・`CREDENTIAL` を含む環境変数は、コマンドに渡しません（API キーの読み出し防止）。
- **サンドボックスではありません。** コマンドは ano を起動したユーザーの権限で動き、workspace の外のファイルやネットワークにもアクセスできます。承認で内容を確認してください。

### Web ページの取得（web_fetch）

ドキュメントや Issue など、作業に必要な Web ページを読むための tool です。専用の権限設定は無く、MCP の tool と同じく常に使え、取得ごとに承認モードで判定します。使わせたくない場合は、ポリシーの `disabled_tools` に `web_fetch` を加えます。

- **取得ごとに承認が必要です。** URL にはデータを載せて外部へ送れるため、`workspace_exec` と同じ[承認モード](docs/mcp.md#承認モード)で判定します。`auto` の判定用モデルは、作業に必要なページの閲覧を許可し、URL に秘密情報や workspace のデータを含むもの、文書などに埋め込まれた指示に従っているように見えるものを拒否します。
- **公開アドレスだけに接続します。** ホスト名を解決したすべてのアドレスを確認し、loopback・プライベート・リンクローカル（クラウドのメタデータ endpoint を含む）などへの接続を拒否します。リダイレクト先（最大5回）も同じく確認し、解決したアドレスに接続先を固定するため、DNS の応答が変わっても内部ネットワークには届きません。プロキシの環境変数は使いません。
- HTML は Markdown に変換し（`script`・`style`・`nav` などは除外）、`title` を返します。テキスト・JSON・XML はそのまま返し、画像などのバイナリは扱いません。`raw:true` で HTML をそのまま返します。
- 1回の取得は 30 秒・5 MiB まで、返す内容は既定 32 KiB（`max_bytes` で最大 256 KiB）です。続きは `next_offset` を `offset` に渡して読みます。
- Cookie や認証情報は送りません。ログインが必要なページは読めません。

### GitHub の Issue と Pull Request

GitHub の操作は [GitHub MCP Server](https://github.com/github/github-mcp-server) に任せ、手元の変更のコミットと push は `git_commit_push` で行います。組み合わせると、`ano chat` で「Issue を解決して PR を作成して」と頼むだけで、Issue の読み取りから PR の作成まで進みます。

```toml
[[mcp_servers]]
label = "github"
transport = "streamable_http"
url = "https://api.githubcopilot.com/mcp/"
authorization_env = "GITHUB_MCP_TOKEN"
description = "GitHub の Issue・Pull Request・リポジトリを操作します"
# マージ・削除・リポジトリ作成などは含めない
allowed_tools = ["issue_read", "list_issues", "search_issues", "pull_request_read", "list_pull_requests", "search_pull_requests", "get_file_contents", "create_pull_request", "update_pull_request", "add_issue_comment", "pull_request_review_write", "add_comment_to_pending_review"]
```

```sh
export GITHUB_MCP_TOKEN="$(gh auth token)"   # または Personal Access Token
cd /path/to/clone
ano chat --allow-writes
> https://github.com/OWNER/REPO/issues/6 を解決して PR を作成して
> PR #8 をレビューして、気になる点をコメントして
```

モデルは `issue_read` で Issue を読み、workspace を編集し、`review_changes` で別の新しい会話のレビュー担当にレビューさせ、指摘を判断して対処してから、`git_commit_push` で push し、`create_pull_request` で PR を作ります。レビューは必須で、レビュー後にファイルを変えた場合は再度レビューを受けるまで push できません（[変更のレビュー](docs/agent-runtime.md#変更のレビューreview_changes)）。push と PR 作成はそれぞれ承認を求めます（既定の `auto` では判定用モデルが審査し、迷うものだけ `[y/N]` で確認）。第三者の Issue を扱う場合は、`--approval-mode ask` ですべてを自分で確認することもできます。利用できる tool は `ano mcp tools github` で確認できます（`ano mcp edit github` で選択）。

`git_commit_push` の動作は次のとおりです。

- `files` に挙げたファイル（追加・変更・削除）だけをコミットします。ほかの未コミットの変更には触れません。ディレクトリは指定できません。
- `review_changes` でレビューを受けた時点と同じ内容のファイルだけをコミットできます（[変更のレビュー](docs/agent-runtime.md#変更のレビューreview_changes)）。
- remote の既定ブランチには直接コミットしません。既定ブランチにいるときは `branch` の名前で新しいブランチを現在のコミットから作り、未コミットの変更ごと移ります。それ以外のブランチにいるときはそのブランチに追加でコミットするため、レビューを受けた修正も同じ PR に積めます。
- push 先は `origin`（無ければ最初の remote）です。git コマンドを使うため、git の認証設定（credential helper・SSH 鍵）で push します。結果として、ブランチ・既定ブランチ（PR の向き先）・`OWNER/REPO`・コミットを返し、MCP の `create_pull_request` にそのまま渡せます。
- 既定ブランチは毎回 push 先の remote に問い合わせます（clone 時に記録された `origin/HEAD` は、既定ブランチの変更で古くなるため）。確かめられない場合と、remote に同名のブランチが既にある場合は、何も変更せずに中止します。
- push に失敗してもコミットは残り、同じブランチで再度呼び出すと push だけをやり直します。
- リポジトリの hook（`core.hooksPath` を含む）と `core.fsmonitor` は実行しません。一方、利用者の git 設定にある clean/smudge filter（git-lfs など）とコミット署名（`commit.gpgSign`）は、コミットの正しさに関わるためそのまま使います。そのため、これらに設定した外部プログラム（filter のコマンド、gpg・ssh などの署名プログラム）は `git_commit_push` から実行されます。対話が必要な署名（パスフレーズの入力など）は失敗することがあります。これらは `.git/config` か利用者の設定にしか定義できず、`.git` はエージェントから書き換えられません。hook はモデルが書き込めるファイルのため、実行すると `allow_writes` だけでコマンドを実行できてしまうからです。コミット前の検証は `workspace_check` で行ってください。
- push は外部への公開になるため、呼び出しごとに承認が必要です（`workspace_exec` と同じ[承認モード](docs/mcp.md#承認モード)）。`allow_exec` は不要です。

## ライブラリとして使う

`ToolRegistry` に JSON Schema と async handler を登録し、`Agent` に渡します。動く例は [examples/embed.rs](examples/embed.rs) にあります。

```rust
use ano::{register_builtin_tools, ToolDefinition, ToolRegistry};
use serde_json::json;

let registry = ToolRegistry::new();
register_builtin_tools(&registry)?; // echo・unix_time・workspace_*
registry.register(
    ToolDefinition::new(
        "lookup_customer",
        "Look up a customer by id.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false
        }),
    ),
    |arguments| async move {
        let id = arguments["id"].as_str().unwrap_or_default();
        Ok(json!({"id": id, "status": "example"}))
    },
)?;
```

- `Agent::with_event_listener` で tool 呼び出しなどのイベントを、`Agent::with_text_listener` で生成中の回答の差分を受け取れます。
- 実行環境（user・environment・workspace・書き込み許可）を受け取る tool は `register_contextual` で登録し、`ToolContext` から参照します。
- tool 名は Responses API の関数名規則に合わせて ASCII 英数字・`_`・`-` の64文字以内です。`tool_search`・`task_plan`・`delegate_task` と `mcp__` で始まる名前は予約されています。
- `register_builtin_tools` の `git_commit_push` は、レビューを受けた状態のファイルだけをコミットします。その照合と `review_changes` は拡張 `ReviewGate` が担うため、組み込みの tool を使う `Agent` には `.with_extension(Arc::new(ReviewGate::new()))` を付けてください（`Agent` ごとに1つ）。付けない場合、コミットは常に `review_required` で拒否されます。
- 影響の大きい tool は `ToolDefinition::new(...).with_approval()` で登録すると、呼び出しごとに `ApprovalHandler` へ確認します（`McpApprovalRequest::source` が `ApprovalSource::LocalTool` になります）。
- 直接接続の MCP を使う場合は `McpPool` を1つ作り、`Arc` で各 `Agent` に渡すと接続を共有できます。終了時に `shutdown().await` を呼んでください。
- `Agent` は外部依存をトレイト（`ResponsesApi`・`McpGateway`・`ConversationStore`・`ApprovalHandler`）で受け取るため、別の API クライアントや保存先に差し替えられます。構成は [docs/architecture.md](docs/architecture.md) を参照してください。
- `Agent` の実行ループは特定の tool を前提としません。複数の tool にまたがる規則や、サブエージェントを起動するランタイム tool を加える場合は `AgentExtension` を実装して `with_extension` で渡します（`ReviewGate` が実装例です）。`ano` と同じ構成（組み込み tool・既定の instructions・AGENTS.md・スキル・承認モード・ロール）で組み立てる場合は `Harness` を使います。

## ドキュメント

| ドキュメント | 内容 |
| --- | --- |
| [docs/chatgpt-subscription.md](docs/chatgpt-subscription.md) | ChatGPT ログイン、利用枠による接続、認証情報の保存、対応範囲 |
| [docs/agent-runtime.md](docs/agent-runtime.md) | 実行ループの上限・並行実行、セッション、圧縮、トークン上限、作業計画、tool の遅延公開、サブエージェント、スキル |
| [docs/mcp.md](docs/mcp.md) | MCP の接続方式、OAuth 認証、接続の再利用、tool の確認と有効化、承認、ポリシーの名前空間、検索カタログ |
| [docs/web.md](docs/web.md) | Web UI の起動とトークン、セッション、API、naui による画面のビルド |
| [docs/webhook.md](docs/webhook.md) | Webhook の API、署名方法（curl / PowerShell）、ジョブの状態と中止 |
| [docs/architecture.md](docs/architecture.md) | レイヤー構成、ポート、ディレクトリ構成、設計上の判断 |

## 開発

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

統合テスト（`tests/`）はモックの Responses API を立て、ビルドした `ano` バイナリを実際に起動して検証します。Web UI の画面（`web-ui/`）を変えた場合は `web-ui/build.sh` でビルドし直します（[画面の実装](docs/web.md#画面の実装naui)）。ソースの構成は [docs/architecture.md](docs/architecture.md) を参照してください。

## セキュリティ上の注意

- ano は起動したユーザーの OS 権限で動きます。`allow_writes`・`allow_exec`・検証コマンド・stdio MCP server は、信頼する workspace とコマンドにだけ設定してください。
- `allow_exec` と `approval_mode = "allow"` を併用すると、モデルが任意のコマンドを確認なしで実行できます。使い捨てのコンテナなど、壊れても復元できる環境に限ってください。
- リモート MCP server は外部へデータを送信できます。信頼できる server だけを登録し、`require_approval = "never"` と `approval_mode = "allow"` は信頼済みの server に限ってください。`auto` モードの判定は補助的な安全策で、完全ではありません。
- `ano web` は、この PC のブラウザからはトークンなしで、ほかの端末からは起動時に表示されるトークンで使えます。同じ PC のほかのユーザーも操作できる点に注意してください。トークン付きの URL は他人に渡さないでください。`--bind` でループバック以外のアドレスにすると、トークンと会話は暗号化されない HTTP でネットワークを流れます。`--no-auth` ではネットワーク上の誰でも操作できます。どちらも信頼できるネットワークでだけ使ってください（[docs/web.md](docs/web.md#セキュリティ)）。
- Webhook は必ず secret を設定して公開します。未認証での起動は loopback アドレスに限られます（[docs/webhook.md](docs/webhook.md#セキュリティ)）。
- `web_fetch` は取得のたびに URL を外部へ送ります。`approval_mode = "allow"` では、ページに埋め込まれた指示でモデルがデータを URL に載せて送る可能性を確認なしに許すことになります。
- `workspace_delete` による削除は取り消せません。書き込みを許可する環境は、Git などで復元できる workspace にしてください。
- `AGENTS.md` はモデルへの指示として送られます。信頼できないリポジトリを扱う環境では `project_instructions = []` にしてください。
- スキルは以後のすべての実行で指示として参照されます。`skill_save` と `approval_mode = "allow"` を併用すると、Web ページなどに埋め込まれた指示がスキルとして確認なしに残る可能性があります。保存された `SKILL.md` は `ano skills` で確認し、不要なものはディレクトリごと削除してください。
- Issue・PR・コメントは第三者が書けます。GitHub MCP で読んだ文面に埋め込まれた指示でモデルが動く可能性があるため、公開リポジトリでは `approval_mode = "allow"` を避け、GitHub MCP の `allowed_tools` を必要な操作に絞ってください。作られた PR の差分はマージ前に確認してください。
- 中止・タイムアウト・トークン上限による停止は、完了済みのファイル書き込みや外部操作を巻き戻しません。
