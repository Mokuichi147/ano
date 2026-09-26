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
- 登録した tool・MCP を検索で必要な分だけ公開する遅延公開（`tool_search`）
- 保存して別プロセスから再開できる会話セッションと、複数ターンの対話モード（`ano chat`）
- リポジトリの `AGENTS.md` などをプロジェクト固有の指示として自動で読み込み
- 推論モデルの `reasoning.effort` 指定と、推論の要約の進捗表示
- 長い会話の自動圧縮と、使用トークン数に応じた実行停止

**tool と MCP**
- workspace 内に閉じたファイル一覧・パス名検索（グロブ）・分割読み取り・全文検索（正規表現対応）・書き込み・移動・削除
- 競合検出付きの正確なファイル編集と、設定で登録した検証コマンド（ビルド・テスト）の実行
- リモート MCP（Responses API 経由、Secure MCP Tunnel 対応）と、ano からの直接接続（stdio / Streamable HTTP）
- ユーザー・実行環境ごとの tool allowlist / denylist（ワイルドカード対応）と MCP 承認フロー

**入出力と連携**
- テキスト・画像・音声入力（音声は文字起こしせず native `input_audio` として送信）
- 名前付き実行環境を選べる署名付き Webhook と、実行中の進捗確認・中止・タイムアウトに対応した非同期ジョブ API
- LM Studio などの OpenAI 互換 `/v1/responses` endpoint
- JSON 出力とログ量の切り替え

## クイックスタート

Rust 1.89 以降と、OpenAI API キーまたは[ローカル AI](#ローカル-ailm-studioollama-など)（LM Studio・Ollama など。API キー不要）が必要です。

```sh
cargo install --path .
cp config.example.toml config.toml
export OPENAI_API_KEY="sk-..."
ano run --environment default "README とソースを読み、実装の概要を説明して"
```

インストールせずに `cargo run -- run ...` でも実行できます。Windows（PowerShell）では `Copy-Item config.example.toml config.toml`、`$env:OPENAI_API_KEY = "sk-..."` のように読み替えてください。

`config.example.toml` の `default` 環境は、カレントディレクトリを読み取り専用で調べる設定です。ファイルを編集させる場合は[実行環境](#実行環境)で `allow_writes = true` の環境を用意します。

## CLI の使い方

| コマンド | 内容 |
| --- | --- |
| `ano run [PROMPT]` | タスクを実行します。PROMPT を省略すると stdin から読みます |
| `ano chat` | 同じ会話で複数ターンのやり取りをします（[対話モード](#対話モード)） |
| `ano tools` | 利用可能な tool と MCP server を、ポリシーを適用して表示します |
| `ano session PATH` | 保存済みセッションの状態・計画・使用量を表示します（`--json` で全内容） |
| `ano serve` | Webhook サーバーを起動します（[docs/webhook.md](docs/webhook.md)） |

共通オプションは `--config PATH`（設定ファイル）と `--user NAME`（`[users]` のユーザー、既定 `default`）です。

### `ano run` / `ano chat` の主なオプション

| オプション | 内容 |
| --- | --- |
| `--environment NAME` | 設定済みの実行環境（workspace・書き込み権限・MCP 承認方針・model・instructions・検証コマンド）を使う |
| `--workspace PATH` / `--allow-writes` | 環境を指定しない場合の workspace（既定はカレントディレクトリ）と書き込み許可 |
| `--model NAME` | モデルを変更 |
| `--reasoning-effort LEVEL` | 推論の深さ（`none`・`minimal`・`low`・`medium`・`high`・`xhigh`。対応範囲はモデルによる） |
| `--image PATH` / `--audio PATH` | 画像・音声を入力に追加（複数指定可、`run` のみ） |
| `--disable-tool NAME` | この実行だけ tool を無効化（複数指定可） |
| `--session PATH` / `--recover-session` | 会話を保存・再開（[セッション](docs/agent-runtime.md#会話セッション)） |
| `--compact-threshold-bytes N` / `--max-total-tokens N` | 履歴の圧縮とトークン上限（[圧縮と上限](docs/agent-runtime.md#履歴の圧縮)） |
| `--auto-approve-mcp` / `--non-interactive` | MCP 承認を自動承認 / 確認せず拒否 |
| `--json` | 結果を1つの JSON オブジェクトとして stdout へ出力（`run` のみ） |
| `--quiet` / `--verbose` | 進捗ログを省略 / 引数と結果を含めて詳しく表示 |

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
| `/help` | コマンド一覧 |
| `/exit`、Ctrl+D | 終了 |
| Ctrl+C | 実行中のターンだけを中断し、会話は続ける（完了した操作は巻き戻しません） |

`--session` を付けない場合、会話はプロセス内のメモリだけに保持されます。MCP の承認は同じ端末で確認します。stdin をパイプで渡すと、1行ずつ指示として処理します（承認は拒否されます）。

### 画像・音声入力

音声入力には `input_audio` に対応したモデル（例: `gpt-audio-1.5`）を指定してください。画像と音声を同じリクエストで使う場合は、両方に対応したモデルが必要です。

### 実行環境

`--environment` を指定すると、Webhook と同じ環境設定で実行します。ユーザーと環境の両方が許可した tool だけが使えます。環境の権限を CLI から広げられないよう、`--workspace`・`--allow-writes`・`--auto-approve-mcp` との併用はエラーになります。`--model` は併用でき、`--non-interactive` で環境の MCP 自動承認も無効にできます。

```toml
[environments.coding]
workspace = "/path/to/my-repository"
allowed_tools = ["workspace_*"]
allow_writes = true
auto_approve_mcp = false

[environments.coding.checks.test]
program = "cargo"
args = ["test", "--locked"]
timeout_secs = 900
```

### 出力とログ

進捗ログ（tool 名と状態、モデルの途中経過、作業計画の更新）は stderr に出力します。`--json` の出力は次のフィールドを持ちます。

| フィールド | 内容 |
| --- | --- |
| `text` | 最終回答 |
| `response_id` | 最後の応答の ID |
| `events` | tool 呼び出しなどのイベント（引数と結果を含む） |
| `outcome` | 作業計画から見た完了状態（`completed` / `blocked` / `incomplete`） |
| `plan` | 作業計画 |
| `usage` | この実行のトークン使用量 |
| `stop_reason` | 停止理由（`final_answer` / `round_limit` / `token_limit` / `usage_unavailable`） |

```sh
ano run --environment default --json --quiet "実装の概要を説明して" | jq .outcome
```

実行エラーは非ゼロの終了コードと stderr のメッセージで返ります。正常終了でも未完了の工程が残る場合があるため、自動処理では `outcome` も確認してください（[作業計画と完了判定](docs/agent-runtime.md#作業計画と完了判定)）。

## 設定ファイル

`--config` を省略するとカレントディレクトリの `config.toml` を読み、無ければ組み込みの既定値で動きます。全項目の例は [config.example.toml](config.example.toml) を参照してください。

| セクション | 内容 | 詳細 |
| --- | --- | --- |
| `[api]` | endpoint・API キーの環境変数名・タイムアウト・リトライ | [ローカル AI](#ローカル-ailm-studioollama-など) |
| `[agent]` | モデル・instructions・推論設定・プロジェクト指示・実行ラウンド数・並行数・圧縮・トークン上限 | [docs/agent-runtime.md](docs/agent-runtime.md) |
| `[environments.<name>]` | workspace・許可する tool・書き込み・MCP 自動承認・検証コマンド | [実行環境](#実行環境) |
| `[users.<id>]` | ユーザーごとの `allowed_tools` / `disabled_tools` | [ポリシーの名前空間](docs/mcp.md#ポリシーの名前空間) |
| `[[mcp_servers]]` | MCP server の接続方式・許可する tool・承認 | [docs/mcp.md](docs/mcp.md) |
| `[webhook]` | 待ち受けアドレス・署名・ジョブ数とタイムアウト | [docs/webhook.md](docs/webhook.md) |

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

`environments.*.workspace` と MCP の `cwd` の相対パスは、設定ファイルのあるディレクトリを基準に解決します。先頭の `~`（`~` と `~/...`）はホームディレクトリに展開します（MCP の `command` も同様）。CLI の `--workspace` は起動ディレクトリを基準にします。

API キーは設定ファイルに保存せず、`api_key_env` で指定した環境変数から読み込みます。`OPENAI_BASE_URL` を設定すると、設定ファイルの `base_url` より優先して endpoint を変更できます（モックサーバーなど）。

### ローカル AI（LM Studio・Ollama など）

OpenAI 互換の `/v1/responses` を提供するサーバーなら、`[api]` の `base_url` を差し替えて使えます。

```toml
[api]
base_url = "http://127.0.0.1:1234/v1"      # LM Studio。Ollama なら http://127.0.0.1:11434/v1

[agent]
model = "ロードしたモデル名"
```

- **API キーは不要です。** キーが必須なのは OpenAI 公式の endpoint（`api.openai.com`）だけで、それ以外は `api_key_env` の環境変数が未設定なら Authorization ヘッダーを付けずに送ります。LAN 内の別マシンや Docker 上のサーバーでも同じです。サーバー側で認証を有効にしている場合は、`api_key_env` に指定した環境変数にキーを設定してください。
- `config.toml` はカレントディレクトリから読みます。別のディレクトリで実行する場合は `--config` で指定するか、環境変数 `OPENAI_BASE_URL` で endpoint を指定してください。設定が読まれていないと既定の OpenAI endpoint に接続しようとして、API キーがないというエラーになります。
- LM Studio で Remote MCP を使う場合は、Server Settings で MCP 利用を有効にします。

ロードしたモデル名は `agent.model` または `--model` で指定します。tool calling の品質はモデルの tool use 対応に依存します（native tool use 対応モデルを推奨）。URL 形式の MCP は `url` で登録できますが、Secure MCP Tunnel（`tunnel_id`）は OpenAI Responses API の機能で、LM Studio では使えません。

## 組み込み tool

| tool | 内容 | 条件 |
| --- | --- | --- |
| `workspace_list` | ディレクトリ直下を名前順に取得（既定100件、最大1000件）。`next_after` を次の `after` に渡すと続きを取得 | workspace |
| `workspace_find` | パスのグロブ（`*.rs`、`src/**/*.ts`、`*.{toml,md}`）でファイル・ディレクトリを再帰的に探す（既定200件、最大1000件） | workspace |
| `workspace_read` | UTF-8 ファイルを `offset`（バイト位置）と `max_bytes`（既定 64 KiB、最大 10 MiB）で分割して読む。`next_offset` で続きを読み、文字の途中では分割しない | workspace |
| `workspace_search` | UTF-8 テキストを再帰的に検索し、パス・行番号・列番号・抜粋を返す。既定は大文字小文字を区別する文字列検索で、`regex:true`（Rust の正規表現）、`ignore_case:true`、`include`（対象ファイルのグロブ）を指定できる（既定100件、最大1000件） | workspace |
| `workspace_edit` | 置換対象がちょうど1回だけ出現することと、必要なら `expected_sha256` の一致を確認してから原子的に書き込む。`dry_run:true` で差分とハッシュを確認できる | `allow_writes` |
| `workspace_write` | UTF-8 テキストをファイルへ書き込む | `allow_writes` |
| `workspace_move` | ファイル・ディレクトリを移動（リネーム）。既存ファイルの上書きは `overwrite:true` のときだけ | `allow_writes` |
| `workspace_delete` | ファイル・リンク・空ディレクトリを削除。中身のあるディレクトリは `recursive:true` が必要。取り消しはできない | `allow_writes` |
| `workspace_check` | 環境の `checks` に登録した検証コマンドを実行。`name:null` で一覧 | `checks` |
| `task_plan` | 作業計画の読み書き（[詳細](docs/agent-runtime.md#作業計画と完了判定)） | 常時 |
| `tool_search` | 登録済み tool・MCP の検索（[詳細](docs/agent-runtime.md#tool-の遅延公開tool_search)） | 常時 |
| `echo` / `unix_time` | 動作確認用 | なし |

実際に使える tool は、ユーザーと環境の `allowed_tools` / `disabled_tools` で決まります。読み取り専用の環境で検索を使うには `allowed_tools` に `workspace_search`・`workspace_find` を加えてください。`workspace_*` は移動・削除も許可する点に注意してください。

- **workspace の外には出ません。** 絶対パスや `..` を拒否し、既存の親ディレクトリを1階層ずつ正規化して workspace 内であることを確認します。シンボリックリンクを経由した書き込みも拒否し、リンクの削除・移動ではリンク先に触れません。
- **`.git` と workspace ルートは移動・削除できません。**
- **検索量に上限があります。** `workspace_search`・`workspace_find` は 10,000 エントリ（検索はさらに 32 MiB）までを走査し、リンク・バイナリ・10 MiB 超のファイルと、`.git`・`node_modules`・`target` などの生成物ディレクトリを省略します。上限に達したら範囲を狭めて再検索します。
- **検証コマンドは設定で固定されます。** `workspace_check` のコマンドと引数は設定ファイルで決まり、workspace を作業ディレクトリとして実行し、出力は上限付きで返します。検証ごとの `timeout_secs` を優先し、タイムアウト時も取得済みの出力を返します。Webhook のジョブ全体の制限は引き続き適用されます。
- **shell 実行 tool はありません。** 必要な場合は、アプリケーション側でより狭い権限の tool を登録してください。

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

- 実行環境（user・environment・workspace・書き込み許可）を受け取る tool は `register_contextual` で登録し、`ToolContext` から参照します。
- tool 名は Responses API の関数名規則に合わせて ASCII 英数字・`_`・`-` の64文字以内です。`tool_search`・`task_plan` と `mcp__` で始まる名前は予約されています。
- 直接接続の MCP を使う場合は `McpPool` を1つ作り、`Arc` で各 `Agent` に渡すと接続を共有できます。終了時に `shutdown().await` を呼んでください。
- `Agent` は外部依存をトレイト（`ResponsesApi`・`McpGateway`・`ConversationStore`・`ApprovalHandler`）で受け取るため、別の API クライアントや保存先に差し替えられます。構成は [docs/architecture.md](docs/architecture.md) を参照してください。

## ドキュメント

| ドキュメント | 内容 |
| --- | --- |
| [docs/agent-runtime.md](docs/agent-runtime.md) | 実行ループの上限・並行実行、セッション、圧縮、トークン上限、作業計画、tool の遅延公開 |
| [docs/mcp.md](docs/mcp.md) | MCP の接続方式、接続の再利用、承認、ポリシーの名前空間、検索カタログ |
| [docs/webhook.md](docs/webhook.md) | Webhook の API、署名方法（curl / PowerShell）、ジョブの状態と中止 |
| [docs/architecture.md](docs/architecture.md) | レイヤー構成、ポート、ディレクトリ構成、設計上の判断 |

## 開発

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

統合テスト（`tests/`）はモックの Responses API を立て、ビルドした `ano` バイナリを実際に起動して検証します。ソースの構成は [docs/architecture.md](docs/architecture.md) を参照してください。

## セキュリティ上の注意

- ano は起動したユーザーの OS 権限で動きます。`allow_writes`・検証コマンド・stdio MCP server は、信頼する workspace とコマンドにだけ設定してください。
- リモート MCP server は外部へデータを送信できます。信頼できる server だけを登録し、`require_approval = "never"` は信頼済みの server に限ってください。
- Webhook は必ず secret を設定して公開します。未認証での起動は loopback アドレスに限られます（[docs/webhook.md](docs/webhook.md#セキュリティ)）。
- `workspace_delete` による削除は取り消せません。書き込みを許可する環境は、Git などで復元できる workspace にしてください。
- `AGENTS.md` はモデルへの指示として送られます。信頼できないリポジトリを扱う環境では `project_instructions = []` にしてください。
- 中止・タイムアウト・トークン上限による停止は、完了済みのファイル書き込みや外部操作を巻き戻しません。
