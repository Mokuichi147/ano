# ano

OpenAI Responses API を使う、Rust 製の自律型 AI agent です。Responses API が返した function call を自動実行し、tool の結果を次の Responses リクエストへ返すループを持ちます。

## できること

- OpenAI Responses API 経由の自動 tool calling
- Rust の `ToolRegistry` へ登録したローカル function tool
- Responses API の `mcp` tool によるリモート MCP / Secure MCP Tunnel
- ユーザーごとの function denylist とワイルドカード
- テキスト、画像、音声入力
- 音声は Responses API の native `input_audio` としてそのままモデルへ渡す（文字起こし前処理なし）
- MCP の承認フロー（`always` / `never`、CLI では対話確認も可能）
- LM Studio の OpenAI 互換 `/v1/responses` endpoint
- 名前付き実行環境を選べる署名付き Webhook と、中止・タイムアウトに対応する非同期ジョブ API
- 実行環境に閉じたファイル一覧・分割読み取り・全文検索・書き込み
- CLI からの名前付き環境選択、JSON 出力、ログ量の切り替え
- tool / MCP は lazy discovery し、検索結果の少数だけを model request に公開

## 起動

PowerShell の例です。

```powershell
Copy-Item config.example.toml config.toml
$env:OPENAI_API_KEY = "sk-..."
cargo run -- run --environment default "READMEとソースを読み、実装の概要を説明して"
```

画像と音声はそれぞれ同じ agent の入力として扱えます。

```powershell
cargo run -- run "この画像を説明して" --image .\diagram.png
cargo run -- run "この音声の内容に答えて" --audio .\instruction.wav --model gpt-audio-1.5
```

利用可能な tool とユーザー単位のフィルタは次で確認できます。

```powershell
cargo run -- tools --user default
cargo run -- run --disable-tool echo "echo は使わずに答えて"
```

`--config` を省略するとカレントディレクトリの `config.toml` を読み、無ければ組み込みの既定値で動きます。`--config` で明示したファイルが存在しない場合はエラーになります（打ち間違いでポリシーなしの既定値へ黙って切り替わらないようにするため）。設定ファイルの未知のキーもエラーになるので、`disable_tools` のような綴り間違いで制限が無効になることはありません。

設定内の `environments.*.workspace` と MCP の `cwd` の相対パスは、設定ファイルのあるディレクトリを基準に解決します。CLI の `--workspace` は起動ディレクトリを基準にします。`--user` の未知の名前はエラーになり、別ユーザーの設定へ自動的に切り替わりません。

CLI でも Webhook と同じ環境設定を使えます。ユーザーと環境の両方が許可した tool だけが利用可能です。

```powershell
cargo run -- tools --environment default
cargo run -- run --environment default "src内でTODOを検索して整理して"
# スクリプトから読む場合。stdout は1つのJSONオブジェクトになります。
cargo run --quiet -- run --environment default --json --quiet "実装の概要を説明して"
```

`--environment` は workspace・書き込み権限・MCP承認方針・model・instructions を読み込みます。`--workspace`、`--allow-writes`、`--auto-approve-mcp` との併用はエラーです。`--model` でモデルを変更でき、`--non-interactive` で環境の自動承認も無効にできます。環境指定なしの場合は従来どおり `--workspace` と `--allow-writes` を使えます。

通常の進捗ログは tool 名と状態だけを stderr に出力します。引数・ファイル内容を含む完全なログが必要なら `--verbose`、進捗を省略するなら `--quiet` を指定します。`--json` の成功時出力は `text`、`response_id`、`events` を含みます（`events` には引数と結果も含まれます）。失敗時は非ゼロ終了コードと stderr のエラーを返します。

`OPENAI_BASE_URL` を設定すると、Responses API のモックサーバーなどへ向けられます。モデルは `config.toml` または `--model` で変更できます。音声入力を使う場合は、`input_audio` をサポートするモデル（例: `gpt-audio-1.5`）を指定してください。現在の公式モデル一覧では、一般的な画像対応モデルと音声専用モデルの対応範囲が異なるため、画像と音声を同一リクエストで使う場合は、両方をサポートするモデルを選んでください。

## LM Studio

LM Studio は OpenAI 互換の Responses API、function tool、Remote MCP を提供するため、`[api]` の endpoint を差し替えて利用できます。LM Studio 側で server を起動し、モデルをロードしてください。Remote MCP を使う場合は LM Studio の Server Settings で MCP 利用を有効にします。

```toml
[api]
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LM_STUDIO_API_KEY"
```

ローカル endpoint では `LM_STUDIO_API_KEY` が未設定でも動くよう、クライアントは `lm-studio` というダミーの Bearer 値を使います。認証を有効にした場合は環境変数へ実際のキーを設定してください。ロードしたモデル名を `agent.model` または `--model` で指定します。tool calling の品質はモデルの tool-use 対応（native tool use 対応モデルが推奨）に依存します。URL 形式の MCP は `url` で登録できますが、OpenAI Secure MCP Tunnel の `tunnel_id` は LM Studio ではなく OpenAI Responses API 側の機能です。

## Webhook から環境を指定して開始

`[environments.<name>]` にサーバー側の実行環境を登録し、`ano serve` を起動します。Webhook の JSON では `task`、`user`、`environment` と任意の inline 画像・音声を指定できますが、workspace パスや tool allowlist を外部から上書きできません。

```toml
[webhook]
bind = "127.0.0.1:8080"
path = "/webhook/tasks"
secret_env = "ANO_WEBHOOK_SECRET"

[environments.coding]
workspace = "C:/work/my-repository"
allowed_tools = ["workspace_*"]
allow_writes = true
auto_approve_mcp = false
```

```powershell
$env:ANO_WEBHOOK_SECRET = "replace-with-a-long-random-secret"
cargo run -- serve
```

署名は `X-Ano-Timestamp` に現在の Unix 秒を入れ、`"<timestamp>.<body>"` の HMAC-SHA256 を `X-Ano-Signature: sha256=<hex>` として送ります。タイムスタンプが `webhook.signature_tolerance_secs`（既定 300 秒）より古い・新しいリクエストと、一度受け付けた署名の再送（リプレイ）は `401` で拒否します。

```json
{
  "task": "リポジトリを確認して必要な修正を行って",
  "user": "default",
  "environment": "coding",
  "images": [{"data": "<base64>", "mime_type": "image/png"}],
  "audio": [{"data": "<base64>", "format": "wav"}]
}
```

`user` は `[users]` に定義済みの名前（または `default`）だけを受け付けます。未知のフィールド（`workspace` など）を含む body は `400` になります。`images` と `audio` は任意です。音声は Webhook でも文字起こしせず、native `input_audio` としてそのままモデルへ渡します。body 上限は `webhook.max_body_bytes` で設定します。

PowerShell から送る場合の署名例です。タイムスタンプと同じ `$body` のバイト列を署名してから POST します。

```powershell
$body = '{"task":"リポジトリを確認して必要な修正を行って","user":"default","environment":"coding"}'
$timestamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()
$key = [Text.Encoding]::UTF8.GetBytes($env:ANO_WEBHOOK_SECRET)
$bytes = [Text.Encoding]::UTF8.GetBytes("$timestamp.$body")
$hmac = [Security.Cryptography.HMACSHA256]::new($key)
$signature = [Convert]::ToHexString($hmac.ComputeHash($bytes)).ToLowerInvariant()
Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8080/webhook/tasks' `
  -Headers @{ 'X-Ano-Timestamp' = $timestamp; 'X-Ano-Signature' = "sha256=$signature" } `
  -ContentType 'application/json' -Body ([Text.Encoding]::UTF8.GetBytes($body))
```

成功時は `202 Accepted` と `job_id`、`status_url`、`cancel_url` が返り、`GET /jobs/<job_id>` で状態と結果を取得できます。状態取得にも同じ形式の `X-Ano-Timestamp` と `X-Ano-Signature` が必要で、署名対象は `"<timestamp>.<job_id>"` です。状態は `queued`、`running`、`completed`、`failed`、`cancelled`、`timed_out` のいずれかです。`started_at_unix` と `finished_at_unix` は開始前・終了前には `null` です。

同時に実行するジョブは `webhook.max_concurrent_jobs`（既定 2）までで、それを超えたジョブは `queued` のまま待ちます。待機中と実行中の合計が `webhook.max_pending_jobs`（既定 64）に達すると `503` を返します。`webhook.job_timeout_secs`（既定1800秒）は、待機時間を除いた実行全体の上限で、API応答待ちやtool実行時間も含みます。上限到達時は `timed_out` になり、空いた実行枠で次のジョブを開始します。終了済みのジョブだけを `webhook.max_retained_jobs`（既定1000）件まで保持し、終了するたびに古い結果から削除します。`0` を指定すると結果を保持しません。

Ctrl+C では新しいジョブの受付を止め、待機中・実行中のジョブへ中止を通知し、ジョブ終了を最大5秒待ってから残ったタスクを中止します。その後MCP接続を閉じます。toolのpanicもジョブの `failed` として記録し、次のジョブに実行枠を返します（プロセス全体をabortするpanic設定を除きます）。

secret を設定せずに起動する `--allow-unauthenticated` は loopback アドレスへの bind でだけ使え、secret の環境変数が空文字の場合は起動を拒否します。Webhook 実行は対話端末を持たないため、MCP の承認はデフォルトで拒否されます。信頼済み環境だけ `auto_approve_mcp = true` にしてください。`webhook.path` は固定パスを指定し、管理用の `/jobs` 以下と `/healthz` は使えません。

混雑による `503` や入力不備による `400` では署名を消費しません。署名の有効期限内なら同じ要求を再送できます。同時に同じ署名を送ってもジョブは一度しか登録されません。ジョブと結果はメモリ内に保持するため、サーバー再起動をまたいだ復元には対応していません。

### ジョブの中止

`POST /jobs/<job_id>/cancel` は待機中・実行中のジョブを中止します。署名対象は `"<timestamp>.cancel:<job_id>"` です。状態取得用の署名では中止できません。未終了なら `202` と `cancellation_requested: true` を返し、その後 `GET /jobs/<job_id>` で `cancelled` への遷移を確認できます。すでに終了済みなら `200` で既存の結果を返すため、中止要求は再送できます。未知・削除済みのジョブは `404` です。

```powershell
$jobId = '<job_id>'
$timestamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()
$key = [Text.Encoding]::UTF8.GetBytes($env:ANO_WEBHOOK_SECRET)
$hmac = [Security.Cryptography.HMACSHA256]::new($key)
$payload = [Text.Encoding]::UTF8.GetBytes("$timestamp.cancel:$jobId")
$signature = [Convert]::ToHexString($hmac.ComputeHash($payload)).ToLowerInvariant()
Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8080/jobs/$jobId/cancel" `
  -Headers @{ 'X-Ano-Timestamp' = $timestamp; 'X-Ano-Signature' = "sha256=$signature" }
```

中止・タイムアウトは処理の待機を打ち切ります。すでに完了したファイル書き込みや外部操作は巻き戻さず、送信済みの外部操作が相手側で継続する場合もあります。独自toolは非同期の待機を使い、同期ブロックや別途起動した処理の停止が必要ならtool自身でも中止を扱ってください。

`workspace_write` は `allow_writes = true` の環境だけで動作し、設定された workspace の外へ出る絶対パスや `..` を拒否します。親ディレクトリは 1 階層ずつ正規化して workspace 内であることを確かめてから作成し、シンボリックリンクを経由した書き込みも拒否します。shell 実行 tool は標準登録していません。必要な場合はアプリケーション側で、さらに狭い権限の tool を登録してください。

## ファイル調査の tool

- `workspace_list`: `path` の直下を名前順に取得します。`limit` は既定100、最大1000件です。`next_after` が返ったら、その値を次の呼び出しの `after` に指定すると続きを取得できます。
- `workspace_read`: UTF-8 ファイルを `offset`（バイト位置）と `max_bytes` で分割して読みます。既定64 KiB、最大10 MiBです。`next_offset` で続きが読め、日本語などの文字の途中では分割しません。
- `workspace_search`: `path` 以下の UTF-8 テキストを `query` の文字列で再帰検索し、パス・行番号・列番号・抜粋を返します。正規表現ではなく、大文字小文字を区別する検索です。`max_results` は既定100、最大1000件です。

検索は10,000エントリ・32 MiBの走査量に上限を設け、リンク、バイナリ、10 MiB超のファイル、`.git` や `node_modules`、`target` などの生成物ディレクトリを省略します。上限に達した場合は検索範囲を狭めて再検索してください。必要なファイルだけを調べるため、shell を許可せずリポジトリの調査ができます。読み取り専用環境で検索を使うには `allowed_tools` に `workspace_search` を加えます。

## ローカル tool の登録

アプリケーションに埋め込む場合は `ToolRegistry` に JSON Schema と async handler を登録します。

```rust
use ano::{ToolDefinition, ToolRegistry};
use anyhow::Result;
use serde_json::json;

fn register_tools(registry: &ToolRegistry) -> Result<()> {
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
    Ok(())
}
```

実行環境を受け取る tool は `register_contextual` を使います。Webhook が選んだ user、environment、workspace、書き込み許可を `ToolContext` から参照できます。

```rust
use ano::{ToolContext, ToolDefinition, ToolRegistry};

registry.register_contextual(
    ToolDefinition::new(
        "environment_info",
        "Return the selected environment.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
    ),
    |_arguments, context: ToolContext| async move {
        Ok(serde_json::json!({"environment": context.environment}))
    },
)?;
```

標準の tool（`echo`、`unix_time`、`workspace_*`）は `ano::register_builtin_tools(&registry)` で登録できます。tool 名は Responses API の関数名規則に合わせて ASCII 英数字・`_`・`-` の 64 文字以内に限られ、`tool_search` と `mcp__` で始まる名前は予約されています。

`Agent::run` は function call を自動で処理します。モデルが不正な JSON 引数を返した場合や tool が失敗した場合は、実行を中断せずエラー内容を tool 出力としてモデルへ返します。各 tool 呼び出しには `agent.tool_timeout_secs`（既定 120 秒）のタイムアウトがあります。

`agent.max_tool_rounds` は Responses リクエスト数の上限です。最後の1回は tool を無効にして、実行済みの内容と未完了の作業を報告するために確保します。`1` を指定した場合は tool を使わず回答します。API が `incomplete`・`failed` などの未完了状態を返した場合は、その応答のローカル tool を実行せずエラーにします。回答拒否の説明文はそのまま利用者へ返します。

1 つのレスポンスに複数の function call が含まれる場合は、`agent.max_parallel_tool_calls`（既定 8）件まで並行して実行し、結果はレスポンス内の順序でモデルへ返します。`agent.parallel_tool_calls = false` にすると 1 件ずつ順に実行します。MCP の承認要求は並行実行中でも 1 件ずつ承認ハンドラーへ渡すため、CLI の確認プロンプトが混ざることはありません。同じレスポンス内で `tool_search` を呼んだ場合、その結果は次のリクエストから有効になり、同時に呼ばれた他の tool はモデルがそのレスポンスを生成した時点の tool 一覧で判定します。`Agent::with_event_listener` を使うと tool 呼び出しなどのイベントを発生時点で受け取れます（CLI はこれで進行状況を stderr に表示します）。登録した名前を `users.<id>.disabled_tools` に置くと、そのユーザーの Responses リクエストから tool 定義が除外されます。万一モデルが直接呼び出しても、実行前にもう一度 denylist を確認して `tool_disabled` を返します。

## MCP 設定

`[[mcp_servers]]` の transport は3種類です。省略時は `responses` となり、Responses API がリモート MCP server の tool 一覧取得と実行を担当します。この方式は `url` または `tunnel_id` を使います。Rust 側で直接接続する場合は `stdio` または `streamable_http` を指定します。

```toml
[[mcp_servers]]
label = "github"
url = "https://example.invalid/mcp"
allowed_tools = ["list_issues", "delete_issue"]
require_approval = "always"

[users.alice]
disabled_tools = ["mcp:github:delete_issue"]
```

Streamable HTTP は `url` に接続先を指定し、必要なら `authorization_env` にトークンが入った環境変数名を設定します。stdio は `command`、`args`、任意の `cwd` でローカルプロセスを起動します。子プロセスは ano の環境変数を継承し、`env_vars` は「子プロセスへ渡す環境変数名 = ano 側で値を読む環境変数名」の対応表で値を追加・上書きします。stdio プロセスは ano と同じ OS ユーザー権限で実行され、継承した環境変数にもアクセスできるため、信頼できる MCP server のみ登録してください。

```toml
[[mcp_servers]]
label = "docs_http"
transport = "streamable_http"
url = "https://mcp.example.invalid/mcp"
authorization_env = "DOCS_MCP_TOKEN"
allowed_tools = ["search_docs", "read_page"]

[[mcp_servers]]
label = "local_files"
transport = "stdio"
command = "node"
args = ["./server.js"]
cwd = "./mcp-servers/files"
env_vars = { FILES_API_TOKEN = "FILES_API_TOKEN" }
allowed_tools = ["read_file", "list_files"]
```

直接接続では、接続時に MCP server から tool 名・説明・schema を取得しますが、モデルへは検索カタログの名前と説明だけを使います。`tool_search` が選んだ関数の schema だけを次の Responses リクエストに含めます。`allowed_tools` とユーザー別 `disabled_tools` の両方を適用し、`require_approval` の既定値は `always` です。Webhook ジョブも同じ制限・承認フローを利用します。

### 接続の再利用

直接接続の MCP server は `McpPool` が管理し、実行開始時に接続と tool 一覧取得を行い、以後の実行で使い回します。`ano serve` ではすべての Webhook ジョブが 1 つのプールを共有するため、stdio server のプロセスはジョブごとではなく 1 回だけ起動し、同時に走るジョブも同じ接続へ並行してリクエストします。ユーザー別の制限は実行ごとに適用するので、共有していても各ユーザーに見える tool は変わりません。サーバー全体が denylist で無効化されている場合は接続しません。

直接接続の初期化と tool 一覧取得には、サーバーごとに合計30秒の上限があります。応答しないサーバーはエラーとなり、再実行時に再接続できます。shutdown は進行中の接続待ちも中止します。

プロセスの異常終了などで接続が切れた場合、その実行中の呼び出しはエラーとしてモデルへ返り、次の実行で自動的に接続し直します。tool 一覧は接続時に取得するため、server 側で tool を増減した場合は再接続（ano の再起動）で反映されます。CLI の終了時と `ano serve` の graceful shutdown 時には接続を閉じ、stdio のプロセスを停止します。

server がタスク間で状態を持ち、別のユーザーやジョブと共有したくない場合は `reuse_connection = false` を指定してください。その server だけ従来どおり実行ごとに接続し、実行の終了時に閉じます（Responses API 管理方式の server には指定できません）。

アプリケーションに埋め込む場合は、`McpPool` を 1 つ作って `Arc` で各 `Agent` に渡すと接続を共有できます。

```rust
let mcp = Arc::new(McpPool::new(config.mcp_servers.clone()));
let agent = Agent::new(client, settings, Arc::clone(&mcp), registry, policy, approval);
// ... 終了時
mcp.shutdown().await;
```

`require_approval = "never"` は信頼済みサーバーを完全自動で動かす設定です。未指定時は安全側の `always` になり、CLI から確認します。`--auto-approve-mcp` を付けると CLI の確認を自動承認へ切り替えられます。stdin が端末でない場合（プロンプトをパイプで渡した場合など）は対話確認ができないため、承認要求は拒否されます。Responses API 側から届いた承認要求も、ユーザーポリシーと直前の `tool_search` の選択に含まれない tool であれば、承認ハンドラーに渡さず拒否します。

### ポリシーの名前空間

MCP tool は `mcp:<label>:<tool>` または `<label>:<tool>` で指定します。サーバー全体は `<label>:*` や `mcp:<label>` で指定できます。

- `disabled_tools` は安全側に倒すため、`delete_*` のような修飾なしのルールも全サーバーの同名 MCP tool に適用されます。
- `allowed_tools` はサーバー名で修飾したルール（または `*`）だけが MCP tool に一致します。ローカル tool 用の `read_*` が別サーバーの `read_file` を許可してしまうことはありません。
- environment の `allowed_tools` はユーザーの allowlist に重ねて適用され、両方で許可された tool だけが使えます。

MCP server の `label` は ASCII 英数字・`_`・`-` だけが使え、重複は起動時にエラーになります。

## 大量の tool / MCP を登録する場合

tool 定義や MCP server を登録しても、初回の Responses リクエストへ全件は送りません。最初に固定サイズの `tool_search` だけを公開し、モデルが capability を検索した後、上位 `agent.tool_discovery_limit` 件だけを次のリクエストへ追加します。これにより登録数に比例して tool schema が毎回トークンを消費することを防ぎます。

Responses API 管理方式では検索用の軽量 `tool_catalog` を設定できます。`tool_catalog` がない場合でも、MCP の `allowed_tools` に列挙した名前は検索対象になります。サーバーの tool 全件をモデルへ公開したくない場合は、`allowed_tools` と `tool_catalog` を明示してください。直接接続方式では実サーバーからローカルで取得した一覧が検索対象になります。

```toml
[agent]
tool_discovery_limit = 8

[[mcp_servers]]
label = "github"
url = "https://example.invalid/mcp"
allowed_tools = ["list_issues", "create_issue", "delete_issue"]

[[mcp_servers.tool_catalog]]
name = "list_issues"
description = "List issues in a repository"

[[mcp_servers.tool_catalog]]
name = "create_issue"
description = "Create an issue"
```

モデルは `tool_search` に「issue を一覧」「ファイルを読む」のように capability を渡します。検索結果の schema だけが有効化されるため、必要な tool が変わった場合は再度検索します。

## 検証

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Responses API へのリクエストには `api.timeout_secs`（既定 600 秒）のタイムアウトがあり、接続失敗と 429 / 5xx は `api.max_retries`（既定 2 回）まで `Retry-After` を尊重しつつ再試行します。

API キーは設定ファイルに保存せず、`api_key_env` で指定した環境変数から読み込みます。リモート MCP server は外部へデータを送信できるため、信頼できる server だけを登録してください。
