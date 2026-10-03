# CLI の使い方

| コマンド | 内容 |
| --- | --- |
| `ano run [PROMPT]` | タスクを実行します。PROMPT を省略すると stdin から読みます |
| `ano chat` | 同じ会話で複数ターンのやり取りをします（[対話モード](#対話モード)） |
| `ano auth login/status/logout` | ChatGPT サブスクリプションにログインし、保存した認証情報の状態を確認・削除します（[ChatGPT サブスクリプション](chatgpt-subscription.md)） |
| `ano tools` | 利用可能な tool と MCP server を、ポリシーを適用して表示します |
| `ano session PATH` | 保存済みセッションの状態・計画・使用量を表示します（`--json` で全内容） |
| `ano web` | ブラウザで対話する Web UI を起動します。セッションごとに作業フォルダ・環境・プリセット・権限・承認モードを選べます（[docs/web.md](web.md)） |
| `ano serve` | Webhook サーバーを起動します（[docs/webhook.md](webhook.md)） |
| `ano history status/sync/search/get/context/conversations` | chronotope のローカル履歴キューを確認・再送し、原文を参照します（[履歴](chronotope-history.md)） |
| `ano skills [NAME]` | 保存済みのスキルを一覧表示し、NAME を指定するとその内容を表示します（[スキル](agent-runtime.md#スキルskill_read--skill_save)） |
| `ano mcp tools [LABEL]` | MCP server に接続して提供される tool をすべて表示し、設定で有効なものに印を付けます（[tool の確認と有効化](mcp.md#tool-の確認と有効化)） |
| `ano mcp edit LABEL` | MCP server の tool をチェックリストで有効化・無効化し、設定ファイルに保存します（`ano mcp enable/disable LABEL TOOL...` でも可） |
| `ano provider list` | 接続先の一覧（有効・無効、URL、既定モデル、モデルの絞り込み、フォールバック先）を表示します（[接続先の管理](providers.md#接続先の管理)） |
| `ano provider add/set/remove/enable/disable NAME` | 設定ファイルを直接編集せずに、接続先を追加・変更・削除・有効化・無効化します |
| `ano preset list/add/set/remove NAME` | 接続先・モデル・推論の強さの組（プリセット）を管理します（[プリセット](providers.md#プリセットとロール)） |
| `ano preset edit [NAME]` | プリセットの接続先・モデル・推論の強さを一覧から選んで変更し、設定ファイルに保存します |
| `ano preset role ROLE NAME` | メインのエージェントの既定（`default`）・サブエージェント（`delegate`）・レビュー担当（`review`）・承認の判定（`approval`）に使うプリセットを選びます |
| `ano model list [--provider NAME]` | 接続先に接続して提供されるモデル（と登録したモデル）をすべて表示し、設定で有効なものに印を付けます（[モデルの有効化](providers.md#接続先の管理)） |
| `ano model add/remove MODEL... [--provider NAME]` | モデル一覧を返さない接続先（ChatGPT サブスクリプションなど）に、使うモデル名を登録・削除します |
| `ano model edit [--provider NAME]` | 接続先のモデルをチェックリストで有効化・無効化し、設定ファイルに保存します（`ano model enable/disable MODEL... [--provider NAME]` でも可） |
| `ano mcp login LABEL` | OAuth が必要な MCP server を認可し、トークンを保存します（`ano mcp logout LABEL` で削除。[OAuth 認証](mcp.md#oauth-認証)） |

共通オプションは `--config PATH`（設定ファイル）と `--user NAME`（`[users]` のユーザー、既定 `default`）です。

## `ano run` / `ano chat` の主なオプション

| オプション | 内容 |
| --- | --- |
| `--environment NAME` | 設定済みの実行環境（workspace・書き込み権限・MCP 承認方針・model・instructions・検証コマンド）を使う |
| `--workspace PATH` / `--allow-writes` | 環境を指定しない場合の workspace（既定はカレントディレクトリ）と書き込み許可 |
| `--allow-exec` | 環境を指定しない場合に、`workspace_exec` でのコマンド実行を許可（[コマンド実行](tools.md#コマンド実行workspace_exec)） |
| `--preset NAME` | `[presets]` のプリセット（接続先・モデル・推論の強さ）を使う。`default` は `[agent]` の設定。`--provider`・`--model`・`--reasoning-effort` はその上に重なる（[プリセット](providers.md#プリセットとロール)） |
| `--provider NAME` | `[providers]` の接続先を使う（省略時は既定の接続先、`api` は `[api]`）。接続先に `model` があれば、そのモデルに切り替わる（[接続先の切り替え](providers.md#複数の接続先を切り替える)） |
| `--model NAME` | モデルを変更（選んだ接続先でのモデル名） |
| `--reasoning-effort LEVEL` | 推論の深さ（`none`・`minimal`・`low`・`medium`・`high`・`xhigh`・`max`・`ultra`。対応範囲はモデルによる。`max`・`ultra` は ChatGPT サブスクリプションの Codex モデルなど） |
| `--goal TEXT` | ゴール（最終的にどうなっていればよいか。満たすべき条件も文中に書ける）を指定し、達成が確認されるまで作業を続ける。プロンプトは省略可（`run` のみ。chat では `/goal`。[ゴールと完了条件](agent-runtime.md#ゴールと完了条件)） |
| `--image PATH` / `--audio PATH` | 画像・音声を入力に追加（複数指定可、`run` のみ） |
| `--disable-tool NAME` | この実行だけ tool を無効化（複数指定可） |
| `--session PATH` / `--recover-session` | 会話を保存・再開（[セッション](agent-runtime.md#会話セッション)） |
| `--compact-threshold-bytes N` / `--max-total-tokens N` | 履歴の圧縮とトークン上限（[圧縮と上限](agent-runtime.md#履歴の圧縮)） |
| `--max-tool-rounds N` | この実行で送る Responses 要求の回数の上限（`agent.max_tool_rounds` を上書き。既定は 100。最後の1回は tool を使わない報告に充てる）。暴走を止める歯止めで、費用の予算には `--max-total-tokens` を使う |
| `--approval-mode MODE` | MCP 呼び出しの承認方法。`ask`（確認）・`auto`（判定用モデルが審査して許可・拒否し、確認はしない。既定）・`allow`・`deny`（[承認モード](mcp.md#承認モード)） |
| `--auto-approve-mcp` / `--non-interactive` | `--approval-mode allow` / `deny` と同じ |
| `--json` | 結果を1つの JSON オブジェクトとして stdout へ出力（`run` のみ） |
| `--quiet` / `--verbose` | 進捗ログを省略 / 途中経過もすべて残し、引数と結果を含めて詳しく表示 |
| `--raw` | 回答の Markdown を整形せずにそのまま出力。stdout が端末のときは、既定で回答を生成しながら表示し、見出し・太字・リスト・表を端末向けに整形します（パイプ先や `--json` では完了後にそのまま出力。`NO_COLOR` を設定すると色と文字装飾を省略。[ストリーミング](agent-runtime.md#回答のストリーミング)） |

```sh
ano run "この画像を説明して" --image ./diagram.png
ano run "この音声の内容に答えて" --audio ./instruction.wav --model gpt-audio-1.5
ano run --disable-tool echo "echo は使わずに答えて"
ano tools --environment default
```

## 対話モード

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
| `/provider [NAME [MODEL]]` | 接続先の一覧を表示 / 接続先 NAME（とモデル MODEL）に切り替えて会話を続ける（[接続先の切り替え](providers.md#複数の接続先を切り替える)） |
| `/preset [NAME]` | プリセットの一覧を表示（今の設定と一致するものに印） / プリセット NAME の接続先・モデル・推論の強さに切り替える。`/preset default` で設定の既定に戻る（[プリセット](providers.md#プリセットとロール)） |
| `/effort [LEVEL]` | 推論の強さを表示 / LEVEL に変える（モデルはそのまま） |
| `/goal TEXT` | ゴールを指定して作業を始める。`/goal` で表示、`/goal clear` で解除 |
| `/skill [観点]` | この会話で上手くいった手順をスキルとして保存するよう依頼する（`[skills]` 有効時。保存は承認モードで確認） |
| `/compact` | 会話を今すぐ圧縮する（要約などで履歴を小さくし、コンテキストを空ける。[履歴の圧縮](agent-runtime.md#履歴の圧縮)） |
| `/clear` | 新しい会話を始める（`--session` 指定時は使えません） |
| `/help` | コマンド一覧 |
| ↑ / ↓ | 以前の入力を呼び出す |
| `/exit`、Ctrl+D | 終了 |
| Ctrl+C | 実行中のターンだけを中断し、会話は続ける（完了した操作は巻き戻しません） |

承認モードの既定は `auto` です。判定用モデルが依頼の範囲内で危険の少ない呼び出しを自動で承認し、それ以外は確認せずに拒否して理由をモデルに返します（[自動承認](mcp.md#自動承認auto)）。すべてを自分で確認するには `--approval-mode ask` か、設定の `[agent] approval_mode = "ask"` を使います。端末では全角文字の表示幅を考慮する行編集を使うため、IME での日本語入力や削除も正しく表示されます。`--session` を付けない場合、会話はプロセス内のメモリだけに保持されます。MCP の承認は同じ端末で確認します。stdin をパイプで渡すと、1行ずつ指示として処理します（承認は拒否されます）。

ブラウザから使う `ano web` は [docs/web.md](web.md) を参照してください。

## 画像・音声入力

音声入力には `input_audio` に対応したモデル（例: `gpt-audio-1.5`）を指定してください。画像と音声を同じリクエストで使う場合は、両方に対応したモデルが必要です。

## 実行環境

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

## 出力とログ

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

実行エラーは非ゼロの終了コードと stderr のメッセージで返ります。正常終了でも未完了の工程が残る場合があるため、自動処理では `outcome` も確認してください（[作業計画と完了判定](agent-runtime.md#作業計画と完了判定)）。
