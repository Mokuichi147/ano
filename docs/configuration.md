# 設定ファイル

`--config` を省略すると、次の順に最初に見つかった `config.toml` を読みます。どちらも無ければ組み込みの既定値で動きます。全項目の例は [config.example.toml](../config.example.toml) を参照してください。

1. カレントディレクトリの `./.ano/config.toml`（プロジェクトごとの設定）
2. OS 標準の設定ディレクトリの `config.toml`

無関係な `./config.toml` を ano の設定と誤認しないよう、カレントディレクトリ直下の `config.toml` は自動では読みません。`--config config.toml` で明示したファイルは引き続き読み込めます。

| OS | 設定ディレクトリ |
| --- | --- |
| macOS | `~/Library/Application Support/ano` |
| Linux | `$XDG_CONFIG_HOME/ano`（既定 `~/.config/ano`） |
| Windows | `%APPDATA%\ano\config` |

`ano provider`・`ano model`・`ano preset`・`ano mcp` は読み込んだ設定ファイルを書き換えます。どこにも無い場合、`ano provider`・`ano model`・`ano preset` は OS 標準の設定ディレクトリに作成します（`ano mcp` は設定済みの MCP サーバーが必要です）。

| セクション | 内容 | 詳細 |
| --- | --- | --- |
| `[api]` | endpoint・API キーの環境変数名・タイムアウト・リトライ・ストリーミング | [ローカル AI](#ローカル-ailm-studioollama-など) |
| `[providers.<name>]` | `[api]` とは別の名前付き接続先と、その既定モデル・使えるモデル・フォールバック先 | [接続先の切り替え](providers.md#複数の接続先を切り替える) |
| `[presets.<name>]` / `[agent.roles]` | 接続先・モデル・推論の強さの組と、メインのエージェント・サブエージェント・レビュー担当・承認の判定に使うプリセット | [プリセット](providers.md#プリセットとロール) |
| `[agent]` | モデル・instructions・推論設定・プロジェクト指示・承認モード・実行ラウンド数・並行数・tool 出力の上限・圧縮・トークン上限 | [docs/agent-runtime.md](agent-runtime.md) |
| `[environments.<name>]` | workspace・許可する tool・書き込み・コマンド実行・承認モード・検証コマンド | [実行環境](cli.md#実行環境) |
| `[users.<id>]` | ユーザーごとの `allowed_tools` / `disabled_tools` | [ポリシーの名前空間](mcp.md#ポリシーの名前空間) |
| `[[mcp_servers]]` | MCP server の接続方式・許可する tool・承認 | [docs/mcp.md](mcp.md) |
| `[webhook]` | 待ち受けアドレス・署名・ジョブ数とタイムアウト | [docs/webhook.md](webhook.md) |
| `[history]` | chronotope の接続先・主体・ローカル未送信キュー・タイムアウト | [docs/chronotope-history.md](chronotope-history.md) |
| `[skills]` | スキルの有効化と保存先 | [スキル](agent-runtime.md#スキルskill_read--skill_save) |

設定の誤りで意図せず制限が外れないよう、次の場合はエラーになります。

- `--config` で明示したファイルが存在しない（ポリシーなしの既定値へ黙って切り替えないため）
- 未知のキーがある（`disable_tools` のような綴り間違いで制限が無効になることを防ぐため）
- `--user` に未定義の名前を指定した（別ユーザーの設定へ切り替えないため）

## 既存のプロジェクト設定の移行

これまでプロジェクト直下の `config.toml` を使っていた場合は、`ano --config config.toml ...` で明示するか、`.ano/config.toml` に移動してください。明示したファイルが無い場合や未知のキーがある場合などの厳密な検証は変わりません。自動探索で選ばれる ano の設定が不正・読み取り不可・リンク切れの場合もエラーにし、別の設定や既定値へ黙って切り替えません。

`environments.*.workspace`、MCP の `cwd`、`history.data_dir`、`skills.dir`、API・provider の `chatgpt_auth_file` の相対パスは設定ファイルのあるディレクトリを基準に解決するため、移動時にはパスを調整してください。たとえば、プロジェクト直下の設定で `workspace = "."` としていた場合、`.ano/config.toml` では `workspace = ".."` とすると同じプロジェクトを指します。MCP の `cwd = "./mcp-servers/files"` も `cwd = "../mcp-servers/files"` に変更します。

CLI の `--workspace` は引き続き起動ディレクトリが基準です。`agent.project_instructions` は workspace 基準なので、workspace を維持する場合は変更しません。MCP の `command` は先頭の `~` だけを展開し、`args` はそのまま子プロセスに渡します。workspace 内で実行する検証コマンドのパスも、設定ファイルの移動だけでは変更する必要はありません。

`history.data_dir` を省略していた場合は、旧保存先のプロジェクト直下の `.ano/history` を移動せずに残してください。設定を `.ano/config.toml` へ移しても、そのキューを引き続き検出して使います。ただし、設定ファイルの隣の `.ano/history`（移動後は `.ano/.ano/history`）も存在する場合は、従来どおりそちらを優先します。使うキューを固定するには `history.data_dir` を明示してください。`history.data_dir` を明示していた場合は、他の相対パスと同様に移動後の設定ファイルを基準に調整してください。

## プロジェクト指示（AGENTS.md）

workspace のルートに `AGENTS.md` があると、その内容をプロジェクト固有の指示として instructions の末尾に追加します。コーディング規約・ビルド方法・触れてはいけないファイルなどを書いておくと、毎回プロンプトで説明する必要がなくなります。

```toml
[agent]
project_instructions = ["AGENTS.md", "docs/agent-rules.md"]   # 既定は ["AGENTS.md"]、[] で無効
```

- 存在しないファイルは無視します。workspace の外を指すリンク、UTF-8 でないファイル、合計 64 KiB を超える場合はエラーになります。
- 追加された指示は「運用者の instructions とユーザーの依頼に反しない範囲で従う」ものとして扱われます。信頼できないリポジトリでは `project_instructions = []` にしてください。

## パスと API キー

`environments.*.workspace` と MCP の `cwd` の相対パスは、設定ファイルのあるディレクトリを基準に解決します。OS 標準の設定ディレクトリに置いた設定では、絶対パスか `~/...` で指定してください。先頭の `~`（`~` と `~/...`）はホームディレクトリに展開します（MCP の `command` も同様）。CLI の `--workspace` は起動ディレクトリを基準にします。

API キーは設定ファイルに保存せず、`api_key_env` で指定した環境変数から読み込みます。環境変数は `.env` にも書けます。起動時にカレントディレクトリ（無ければ親ディレクトリ）の `.env`、続いて OS 標準の設定ディレクトリの `.env`（`config.toml` の隣）を読みます。すでに設定されている変数は上書きしないため、優先順はシェルの環境変数、カレントの `.env`、設定ディレクトリの `.env` の順です。設定ディレクトリの `.env` は、どのディレクトリから起動しても使う API キー（MCP の `url` が参照する変数を含む）の置き場所に向いています。秘密情報を含むので、所有者だけが読めるようにしてください（`chmod 600`）。`OPENAI_BASE_URL` を設定すると、設定ファイルの `[api]` の `base_url` より優先して endpoint を変更できます（モックサーバーなど）。`[providers]` の接続先は `OPENAI_BASE_URL` の影響を受けません。

## ローカル AI（LM Studio・Ollama など）

OpenAI 互換の `/v1/responses` を提供するサーバーなら、`[api]` の `base_url` を差し替えて使えます。

```toml
[api]
base_url = "http://127.0.0.1:1234/v1"      # LM Studio。Ollama なら http://127.0.0.1:11434/v1

[agent]
model = "ロードしたモデル名"
```

- **API キーは不要です。** キーが必須なのは OpenAI 公式の endpoint（`api.openai.com`）だけで、それ以外は `api_key_env` の環境変数が未設定なら Authorization ヘッダーを付けずに送ります。LAN 内の別マシンや Docker 上のサーバーでも同じです。サーバー側で認証を有効にしている場合は、`api_key_env` に指定した環境変数にキーを設定してください。
- `--config` を省略すると `./.ano/config.toml`、OS 標準の設定ディレクトリの `config.toml` の順に探し、どちらも無ければ組み込みの既定値を使います。`./config.toml` は自動では読みません（[設定ファイル](#設定ファイル)）。どこからでも同じ接続先を使うには OS 標準の設定ディレクトリに置くか、環境変数 `OPENAI_BASE_URL` で endpoint を指定してください。設定が読まれていないと既定の OpenAI endpoint に接続しようとして、API キーがないというエラーになります。
- LM Studio で Remote MCP を使う場合は、Server Settings で MCP 利用を有効にします。
- 回答はストリーミングで表示します（`stream: true` に対応していない server でも動きます）。
- 長い会話の圧縮は、`/responses/compact` の代わりにモデル自身が書く要約で行います（`[agent] compaction = "auto"` の既定動作）。LM Studio が返す読み込み中のコンテキスト長に近づいたところで圧縮します。llama.cpp の server も `/v1/models` の `meta.n_ctx` から取得します。ほかのサーバーでは `context_window` を指定してください（[履歴の圧縮](agent-runtime.md#履歴の圧縮)）。

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
