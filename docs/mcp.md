# MCP の設定

`[[mcp_servers]]` で MCP server を登録します。接続方式（`transport`）は3種類です。

| transport | 接続する主体 | 必須項目 | 用途 |
| --- | --- | --- | --- |
| `responses`（既定） | Responses API（OpenAI 側） | `url` または `tunnel_id` のどちらか一方 | リモート MCP、Secure MCP Tunnel |
| `streamable_http` | ano | `url` | ano から直接接続する HTTP の MCP server |
| `stdio` | ano | `command` | ano が子プロセスとして起動するローカル MCP server |

MCP server の `label` は ASCII 英数字・`_`・`-` だけが使え、重複は起動時にエラーになります。

## Responses API 管理方式

Responses API がリモート MCP server の tool 一覧取得と実行を担当します。

```toml
[[mcp_servers]]
label = "github"
url = "https://example.invalid/mcp"
allowed_tools = ["list_issues", "delete_issue"]
require_approval = "always"

[users.alice]
disabled_tools = ["mcp:github:delete_issue"]
```

Secure MCP Tunnel を使う場合は `url` の代わりに `tunnel_id` を指定します。`authorization_env` を指定すると、その環境変数の値を `authorization` として Responses API へ渡します。

## 直接接続（streamable_http / stdio）

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

- Streamable HTTP は `url` に接続先を指定し、必要なら `authorization_env` にトークンが入った環境変数名を設定します。
- stdio は `command`・`args`・任意の `cwd` でローカルプロセスを起動します。`cwd` の相対パスは設定ファイルのディレクトリを基準に解決します。
- 子プロセスは ano の環境変数を継承します。`env_vars` は「子プロセスへ渡す環境変数名 = ano 側で値を読む環境変数名」の対応表で、値を追加・上書きします。
- stdio プロセスは ano と同じ OS ユーザー権限で実行され、継承した環境変数にもアクセスできます。信頼できる MCP server だけを登録してください。

直接接続では、接続時に MCP server から tool 名・説明・schema を取得しますが、モデルへは検索カタログの名前と説明だけを使います。`tool_search` が選んだ関数の schema だけを次の Responses 要求に含めます。`allowed_tools` とユーザー別 `disabled_tools` の両方を適用し、Webhook ジョブも同じ制限・承認フローを使います。

### 接続の再利用

直接接続の MCP server は `McpPool` が管理します。実行開始時に接続と tool 一覧取得を行い、以後の実行で使い回します。

- `ano serve` ではすべての Webhook ジョブが1つのプールを共有します。stdio server のプロセスはジョブごとではなく1回だけ起動し、同時に走るジョブも同じ接続へ並行してリクエストします。
- ユーザー別の制限は実行ごとに適用するため、接続を共有していても各ユーザーに見える tool は変わりません。サーバー全体が denylist で無効化されている場合は接続しません。
- 初期化と tool 一覧取得には、サーバーごとに合計30秒の上限があります。応答しないサーバーはエラーになり、次の実行で再接続を試みます。shutdown は進行中の接続待ちも中止します。
- プロセスの異常終了などで接続が切れた場合、実行中の呼び出しはエラーとしてモデルへ返り、次の実行で自動的に接続し直します。
- tool 一覧は接続時に取得するため、server 側で tool を増減した場合は再接続（ano の再起動）で反映されます。
- CLI の終了時と `ano serve` の graceful shutdown 時には接続を閉じ、stdio のプロセスを停止します。

server がタスク間で状態を持ち、別のユーザーやジョブと共有したくない場合は `reuse_connection = false` を指定します。その server だけ実行ごとに接続し、実行の終了時に閉じます（Responses API 管理方式の server には指定できません）。

## 承認（require_approval）

| 値 | 動作 |
| --- | --- |
| `always`（既定） | 呼び出しごとに承認が必要 |
| `never` | 信頼済みサーバーを完全自動で実行 |

承認の方法は実行方法によって決まります。

| 実行方法 | 承認 |
| --- | --- |
| `ano run`（端末から、環境指定なし） | 端末で確認（`[y/N]`） |
| `ano run --auto-approve-mcp` | 自動承認 |
| `ano run --non-interactive`、stdin が端末でない場合 | 拒否 |
| `ano run --environment NAME`、Webhook ジョブ | 環境の `auto_approve_mcp = true` なら自動承認、それ以外は拒否 |

Responses API 側から届いた承認要求も、ユーザーポリシーと直前の `tool_search` の選択に含まれない tool であれば、承認ハンドラーに渡さずに拒否します。

## ポリシーの名前空間

`users.<id>` と `environments.<name>` の `allowed_tools` / `disabled_tools` では、MCP tool を `mcp:<label>:<tool>` または `<label>:<tool>` で指定します。サーバー全体は `<label>:*` や `mcp:<label>` で指定できます。末尾の `*` はワイルドカードです。

- `disabled_tools` は安全側に倒すため、`delete_*` のような修飾なしのルールも全サーバーの同名 MCP tool に適用されます。
- `allowed_tools` はサーバー名で修飾したルール（または `*`）だけが MCP tool に一致します。ローカル tool 用の `read_*` が別サーバーの `read_file` を許可することはありません。
- environment の `allowed_tools` はユーザーの allowlist に重ねて適用され、両方で許可された tool だけが使えます。

## 検索カタログ（tool_catalog）

Responses API 管理方式では、`tool_search` の検索対象として軽量な `tool_catalog` を設定できます。`tool_catalog` がない場合は、`allowed_tools` に列挙した名前が説明なしで検索対象になります。サーバーの tool 全件をモデルへ公開したくない場合は、`allowed_tools` と `tool_catalog` を明示してください。直接接続方式では、実サーバーから取得した一覧が検索対象になります。

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

リモート MCP server は外部へデータを送信できるため、信頼できる server だけを登録してください。
