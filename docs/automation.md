# 自動化（イベントへの反応と通知）

`ano serve` は、人が依頼するのを待つだけでなく、外から届くイベントに自分で反応できます。`POST /events/<name>` に送られたイベント（サービスのレポート、監視の結果、cron からの起動など）を受け取り、設定したルールに従って、**無視する・ユーザーに通知する・作業（ジョブ）を始める**のどれにするかを決めます。どれにするかは、判定用モデル（トリアージ）に判断させることも、ルールで固定することもできます。作業の途中や終了時に、エージェントが「知らせるべき」と判断したことも通知します。

ano 自身は外へデータを取りに行きません。入力は送られてくるものだけです。定期的に何かを調べたいときは、cron などから定期的にイベントを送って作業を起動し、作業の中でエージェントが MCP（GitHub MCP など）で取りに行きます（[定期的に調べる](#定期的に調べるcron-と-mcp)）。

入力元（ソース）と通知先は、`kind` の名前で登録された種類から作られます。組み込みの種類（ソースは `webhook`、通知先は `ntfy`・`discord`・`slack`・`webhook`）も、ライブラリから加える独自の種類も、同じ仕組みで動きます。ルール・判断・作業・通知の処理は、種類に依存しません（[独自の種類](#独自の種類)）。

```text
 送り手                       ソース           ルール               判断                    結果
 サービスのレポート ┐                                                                  ┌→ ignore（記録だけ）
 監視の結果         ├→ POST /events/<name> → 本文を読む → 最初に一致した → トリアージ ─┼→ notify（要約を通知）
 cron からの起動    ┘   （署名を確認）       （重複は除く） ルール      （またはルールで固定）└→ run（ジョブ）→ notify_user・終了時の通知
                                                                                          （作業の中で MCP を使って取得）
```

- [起動](#起動)
- [ソース](#ソース)
- [定期的に調べる（cron と MCP）](#定期的に調べるcron-と-mcp)
- [ルールと判断](#ルールと判断)
- [作業と通知](#作業と通知)
- [通知先](#通知先)
- [状態と重複の扱い](#状態と重複の扱い)
- [`ano automation`](#ano-automation)
- [独自の種類](#独自の種類)
- [セキュリティ](#セキュリティ)
- [今後の予定](#今後の予定)

## 起動

`[automation.sources]` を1つ以上設定すると、`ano serve` が自動化も行います。Webhook サーバーと同じプロセスで動き、作業は Webhook のジョブとして実行されます（同時実行数・タイムアウト・保持数は `[webhook]` の設定に従います。[docs/webhook.md](webhook.md)）。

```toml
[automation.notifiers.phone]
kind = "ntfy"
url = "https://ntfy.sh/my-private-topic"

[automation.sources.analytics]
kind = "webhook"

[[automation.rules]]
name = "active-users"
sources = ["analytics"]
notify = ["phone"]
guidance = "アクティブユーザー数が前週から 10% 以上減ったときだけ知らせて"
```

```sh
export ANO_WEBHOOK_SECRET="replace-with-a-long-random-secret"
ano serve
# ano automation: 1 sources, 1 rules; state in ~/Library/Application Support/ano/automation/state.json
```

判断の内容はイベントごとに標準出力へ記録します。

```text
automation: analytics/weekly_report 2026-w42 → rule 'active-users': notify (DAU fell 30% from last week)
```

全項目の例は [config.example.toml](../config.example.toml) の `[automation]` にあります。

## ソース

`[automation.sources.<name>]` の `kind` で種類を選びます。名前には英数字・`-`・`_` を使えます。`kind` と `trust` 以外の項目は種類ごとの設定で、種類自身が検証します（知らない項目はエラーです）。`trust` の既定値は種類ごとに決まっています。

### webhook（レポートなどの受け取り）

```toml
[automation.sources.analytics]
kind = "webhook"
secret_env = "ANALYTICS_SECRET"   # このソース専用の秘密（推奨）
trust = "trusted"                 # 自分のサービスのレポートなので、本文を自分の言葉として扱う
```

| 設定 | 既定値 | 内容 |
| --- | --- | --- |
| `secret_env` | なし | このソース専用の秘密を読む環境変数。省略するとサーバーの秘密（`webhook.secret_env`）で署名を確かめます |
| `signature_tolerance_secs` | 300 | `secret_env` を使うときの、`X-Ano-Timestamp` の許容誤差 |
| `trust` | `untrusted` | 本文を自分の言葉として扱うか。自分のシステムのデータだけを送る場合に `trusted` にします |

`POST /events/<name>` に JSON を送ると、イベントになります。本文の形式は受け取るソースの種類が決めます（以下は `webhook` の形式です。[独自の種類](#独自の種類)では別の形式も受け取れます）。署名は Webhook と同じ方式ですが、署名対象は `<timestamp>.event:<name>:<body>` です（タスク投入の署名を流用できないよう、対象を分けています）。

送り手にはソース専用の秘密（`secret_env`）を渡してください。サーバーの秘密を渡すと、送り手はイベントだけでなく、タスクの投入やジョブの中止もできてしまいます。

```json
{
  "kind": "weekly_report",
  "key": "2026-w42",
  "title": "Weekly active users",
  "body": "DAU: 700 (last week 1000)\nWAU: 3,100 (last week 3,400)",
  "url": "https://analytics.example.com/reports/2026-w42",
  "author": "analytics"
}
```

- `title` だけが必須です（500 文字まで）。`body` は 100,000 バイトまで、`kind`・`key`・`author` は 200 文字まで、`url` は 2,000 文字までです。`kind` は省略すると `report` です。
- `key` を指定すると、同じ `key` のイベントは1回だけ処理し、2回目以降は `200` と `"duplicate": true` を返します（サーバーの秘密で署名した場合、同じ署名のままの再送は、タスクの投入と同じくリプレイとして `401` になります）。`key` を省略すると、署名の時刻（`X-Ano-Timestamp`）と本文から key を作ります。同じ要求を再送しても、サーバーの再起動後でも1回だけ処理し、時刻を変えて送れば別のイベントになります。
- 受け付けると `202` を返し、判断と通知・作業は裏で続けます。未定義のソースは `404` です。
- 受け付けたイベントは、処理の枠（同時に4件）を取ってから記録し、`202` を返します。10秒待っても枠が空かない場合、待っている要求が 64 件に達している場合、状態ファイルに記録できなかった場合は `503` を返します。どれも署名を使わないため、同じ要求をそのまま再送できます。
- 認証を無効にしたサーバー（loopback のみ）へ署名なしで送ったイベントは、`key` を省くと毎回別のイベントになります。
- 既定では信頼できない入力（`untrusted`）として扱います。署名で分かるのは送り手であって、本文を誰が書いたかではないためです。自分のシステムのデータだけを送る場合は `trust = "trusted"` にします。

```sh
body='{"kind":"weekly_report","key":"2026-w42","title":"Weekly active users","body":"DAU: 700 (last week 1000)"}'
timestamp=$(date +%s)
signature=$(printf '%s.event:analytics:%s' "$timestamp" "$body" \
  | openssl dgst -sha256 -hmac "$ANALYTICS_SECRET" | sed 's/^.* //')
curl -sS -X POST http://127.0.0.1:8080/events/analytics \
  -H "X-Ano-Timestamp: $timestamp" \
  -H "X-Ano-Signature: sha256=$signature" \
  -H 'Content-Type: application/json' \
  --data-binary "$body"
```

## 定期的に調べる（cron と MCP）

ano には定期実行もポーリングもありません。定期的に何かを調べるには、OS の cron（macOS の launchd、Windows のタスク スケジューラなど）から、決まった時刻にイベントを送ります。そのイベントで作業を始めるルールを書けば、エージェントが MCP の tool で必要なデータを取りに行き、知らせるべきことがあれば `notify_user` で通知します。

```toml
[automation.sources.cron]
kind = "webhook"
secret_env = "CRON_SECRET"
trust = "trusted"                       # 送るのは自分の cron だけ

[[automation.rules]]
name = "daily-issues"
sources = ["cron"]
kinds = ["daily"]
action = "run"
task = "GitHub MCP で owner/name の過去1日の新しい Issue を読み、対応が要るものだけ notify_user で知らせる"
environment = "coding"                  # GitHub MCP を許可し、approval_mode = "auto" の環境
notify = ["phone"]
notify_on = ["failed", "timed_out"]
```

送る側のスクリプトの例です（`~/bin/ano-event`）。

```sh
#!/bin/sh
# 使い方: ano-event <ソース名> <kind> <タイトル>
source="$1"; kind="$2"; title="$3"
body=$(printf '{"kind":"%s","title":"%s"}' "$kind" "$title")
timestamp=$(date +%s)
signature=$(printf '%s.event:%s:%s' "$timestamp" "$source" "$body" \
  | openssl dgst -sha256 -hmac "$CRON_SECRET" | sed 's/^.* //')
curl -sS -X POST "http://127.0.0.1:8080/events/$source" \
  -H "X-Ano-Timestamp: $timestamp" \
  -H "X-Ano-Signature: sha256=$signature" \
  -H 'Content-Type: application/json' \
  --data-binary "$body"
```

```text
# crontab: 毎朝 9 時に起動する
0 9 * * * CRON_SECRET=... ~/bin/ano-event cron daily "毎朝の Issue 確認"
```

- 取りに行くのは作業の中のエージェントなので、取得先ごとの API・認証・続きの読み方は MCP server に任せられます。作業1回ごとにモデルを動かすため、細かい間隔で多くのデータを監視する用途には向きません。
- MCP で読んだ Issue やコメントは第三者が書いた文面です。作業の環境は `approval_mode = "auto"` か `deny` にし、書き込みとコマンド実行を必要な範囲に絞ってください（[セキュリティ](#セキュリティ)）。

## ルールと判断

`[[automation.rules]]` を上から順に照合し、最初に一致したルールでイベントを扱います。どのルールにも一致しないイベントは記録だけします。

```toml
[[automation.rules]]
name = "alerts"
sources = ["monitoring"]
kinds = ["alert_*"]
guidance = "本番の障害は高い優先度で知らせる。ステージングの警告は無視してよい"
task = "警告の内容を確認し、原因の候補と影響の範囲をまとめて報告する"
environment = "ops"
notify = ["phone"]
```

| 設定 | 内容 |
| --- | --- |
| `name` | ログと通知に使う名前（必須） |
| `sources` / `kinds` / `authors` | 一致するソース・種類・投稿者。省略するとすべてに一致します。`kinds` は末尾の `*` で前方一致（`alert_*`） |
| `action` | `auto`（既定）・`notify`・`run`・`ignore` |
| `guidance` | 判定用モデルへの判断の基準 |
| `task` / `environment` / `user` / `goal` | 作業の内容・実行環境・実行するユーザー（既定 `default`）・ゴール |
| `notify` | 通知先（`[automation.notifiers]` の名前） |
| `notify_on` | 作業の終了時に通知する状態。既定は `blocked`・`incomplete`・`failed`・`timed_out` |
| `notify_user` | 作業中のエージェントに `notify_user` を使わせるか。既定は、信頼できるイベントでだけ使わせます |

`action = "auto"` では、判定用モデルがイベントを読み、ルールでできることの中から選びます。通知先があれば `notify`、`task` があれば `run` が選択肢に入り、`ignore` は常に選べます。モデルは `guidance` に従って選び、優先度（`low`・`normal`・`high`）と、通知に使う1〜3文の要約も返します。

- 判断と作業の開始は、同時に4件までです。
- 判定用モデルは `[agent.roles] triage` のプリセットで選べます。未設定なら、自動承認の判定用モデル（`approval`）と同じです（[プリセットとロール](providers.md)）。軽いモデルで十分です。
- 判定の応答が読めなければ、もう一度だけ尋ねます。それでも読めない場合や接続に失敗した場合は、通知先があれば `notify`（タイトルと本文の冒頭を通知）、無ければ `ignore` にします。判断に失敗して作業を始めることはありません。
- `action` が `notify`・`run`・`ignore` のどれかなら、モデルには尋ねずにその通りにします。

## 作業と通知

`run` になったイベントでは、ルールの `task` を `environment` で実行するジョブを始めます。ジョブの状態は `GET /jobs/<job_id>` で確認でき、`trigger` にルールとイベントが入ります。エージェントへの依頼文は、ルールの `task` の後に、イベントを `<event>` で囲んだデータとして付けたものです。

自動化で始めたジョブでは、エージェントが `notify_user` tool を使えます。ルールの通知先へ、タイトル・本文（Markdown）・優先度を送ります。「知らせるべきことがあるときだけ使い、何も起きなかったことは知らせない」よう指示しているため、たとえば次のルールでは、数字に異常があるときだけ通知が届きます。

```toml
[[automation.rules]]
name = "weekly-report"
sources = ["analytics"]
action = "run"
task = "レポートを前週と比べ、減少や異常があれば notify_user で原因の候補とともに知らせる"
environment = "analysis"
notify = ["phone"]
notify_on = ["failed", "timed_out"]
```

- `notify_user` は1回の作業で5回までです。サブエージェントは使えません。
- 信頼できないイベント（第三者が書いた文面を中継するものなど）の作業では、既定で `notify_user` を出しません。本文に埋め込まれた指示で、作業中に読んだ内容を通知先へ送らせないためです。必要ならルールに `notify_user = true` を書きます。終了時の通知（`notify_on`）には作業結果の冒頭が入る点にも注意してください。
- 作業が終わると、`notify_on` に含まれる状態ならルールの通知先へ結果（回答の冒頭 1,500 文字、または失敗の理由）を送ります。完了も知らせるには `notify_on` に `completed` を加えます。
- ジョブを始められなかった場合（待機中のジョブが `max_pending_jobs` に達したなど）も通知します。
- 誰も見ていないため、承認モードの `ask` は拒否として扱います（Webhook と同じです）。作業に承認が必要な操作を含む場合は、環境を `approval_mode = "auto"` にします。

## 通知先

`[automation.notifiers.<name>]` の `kind` で種類を選びます。`kind` 以外の項目は種類ごとの設定です。

| `kind` | 送り方 |
| --- | --- |
| `ntfy` | [ntfy](https://ntfy.sh) のトピック URL（`https://ntfy.sh/<topic>`、セルフホストも可）。`token_env` でアクセストークンを付けられます。優先度・リンク・Markdown を送ります |
| `discord` | Discord の Incoming Webhook。`@everyone` などのメンションは無効にして送ります |
| `slack` | Slack の Incoming Webhook。`<!channel>` などのメンションやリンクの記法はエスケープします |
| `webhook` | 任意の URL へ JSON（`title`・`message`・`priority`・`url`・`rule`）を POST します。`secret_env` を設定すると、`ano serve` と同じ方式（`X-Ano-Timestamp`・`X-Ano-Signature`）で署名します |

- URL は `https://` に限ります（`http://` は `localhost` などこのマシンのアドレスだけ）。トークンや通知の内容が、ネットワーク上で読めてしまわないためです。
- 通知の送信に失敗しても、再試行はしません（ログに残ります）。
- URL は `url` か `url_env`（環境変数名）で渡します。Discord と Slack の Webhook URL は知っていれば誰でも投稿できる秘密なので、`url_env` でだけ受け付けます。ntfy の公開サーバーのトピック名も、推測されにくいものにしてください。
- 環境変数は `ano serve` の起動時に読みます。足りなければ起動に失敗します。
- 送信のエラーにも URL は含めません。リダイレクトはたどりません。

## 状態と重複の扱い

処理済みのイベントの key を、`automation.state_file`（既定は OS 標準のデータディレクトリの `automation/state.json`）に保存します。再起動しても、同じイベントを二度処理しません。

- 処理済みのイベントは、ソースごとに新しい 5,000 件まで覚えます。
- イベントは判断の前に処理済みとして記録します（多くても1回だけ処理します）。そのため、受け付けた後に判断・通知・作業の開始の途中でサーバーが止まったイベントは、再起動しても処理し直しません。確実に届ける必要がある通知には向きません。
- 記録に失敗したイベントは処理しません（webhook ソースでは `503` を返します）。
- 状態ファイルのロックは、同じディレクトリの `<状態ファイル名>.lock` です。
- 同じ状態ファイルは1つの `ano serve` だけが使えます（ロックで防ぎます）。複数のサーバーを動かす場合は、それぞれに `state_file` を設定します。
- 既定の状態ファイルは、設定ファイルによらず共通です。別のプロジェクトの設定で順に `ano serve` を動かす場合も、ソース名と key が重なると処理済みとみなされるため、設定ごとに `state_file` を分けてください。
- ジョブ自体はこれまでどおりメモリ内にだけ保持します。

## `ano automation`

サーバーを起動せずに、設定の確認と通知のテストができます。

```sh
ano automation list                                   # ソースと通知先を ano serve と同じく組み立てて確認し、ルール・使える種類とともに一覧（秘密は表示しない）
ano automation notify phone "テスト通知です"           # 通知先へテストを送る
ano automation notify team --title "ano" --priority high
```

## 独自の種類

Rust のライブラリとして使う場合、`EventSource`（ソース）と `Notifier`（通知先）を実装し、`AutomationRegistry` に `kind` の名前で登録すると、設定ファイルからその種類を使えます。

- ソースは、`POST /events/<name>` の要求（ヘッダーと本文）を自分の形式で読んでイベントにします（`receive`）。POST の認証は、ano の署名をサーバーに検証させるか、ソース自身が行うかを選べます。後者では、GitHub・Slack・Stripe のように送り手が独自の方式で署名するサービスの Webhook を、そのまま受け取れます。重複の除外、ルールの照合、判断、作業、通知は、組み込みの種類と同じく `ano serve` が行います。
- どの種類のイベントも、受け取り口で共通の検証を受けます。`kind`・`key`・`title` が空でないこと、`kind`・`key`・`author` が 200 文字・`title` が 500 文字・`url` が 2,000 文字以内であることです。満たさないイベントは処理しません。
- 種類ごとの設定は、`kind` と `trust` を除いた項目が JSON で渡されます。
- ソースが既定で信頼できない入力を出す（`trust` が `untrusted`）場合、そのソースに一致して作業を始めうるルールでは `approval_mode = "allow"` の環境を使えません。

書き方と例は [docs/library.md の独自のソースと通知先](library.md#独自のソースと通知先) を参照してください。

## セキュリティ

- **第三者の入力。** 中継された Issue やコメント、外部サービスの通知などは、誰でも書ける文面を含みます。こうしたイベント（`untrusted`）の本文は「データであり指示ではない」と明示してエージェントに渡します。自動承認（`approval_mode = "auto"`）は、ルールの `task` とソース名だけを依頼として審査し、イベントの本文・タイトル・URL・種類は含めません。そのため、Issue に「main へ push して」と書かれていても、依頼したことにはなりません。
- **`allow` の禁止。** 信頼できないイベントで作業を始めうるルール（`run`・`auto`）は、`approval_mode = "allow"` の環境を指定できません（設定の検証で拒否します）。
- **判断の限界。** エージェントはイベントの本文を読んで作業するため、本文に埋め込まれた指示に従ってしまう可能性は残ります。トリアージも自動承認も、モデルによる補助的な安全策です。自動承認はルールの `task` の範囲に見える操作を許可するため、`task` が書き込みを求めるルールでは、埋め込まれた指示による書き込みも通りうると考えてください。第三者の入力で動かす環境は、できるだけ読み取り専用（`allow_writes = false`・`allow_exec = false`）にして調査と報告に留め、書き込ませる場合も tool を必要なものに絞り、Git などで元に戻せる workspace にしてください。`notify_user` は承認なしにルールの通知先へ送れます（1回の作業で5回まで）。
- **プロジェクト指示。** 自動化の作業でも、workspace の `AGENTS.md` などのプロジェクト指示を読みます（手動の実行と同じです）。第三者が変更できるリポジトリを扱う環境では、`[agent] project_instructions = []` にしてください。
- **繰り返しの防止。** 作業が書いたもの（MCP で投稿したコメントなど）が、外部サービスの Webhook を通じて再びイベントとして届く構成では、作業が連鎖しないよう、ルールの `authors` を絞るか、その投稿者のイベントを `action = "ignore"` のルールで先に受けてください。MCP で読んだ第三者の文面も、作業の中では信頼できない入力です。
- **通知の内容。** 通知にはイベントや作業結果の一部が含まれます。Discord・Slack のチャンネルを共有している場合は、そのメンバーにも見えることに注意してください。

## 今後の予定

- Discord・Slack の Bot による入力（メッセージで依頼する）と、元のスレッドへの返信
- チャット上での承認（`ask` の確認をボタンで答える）
- GitHub などの Webhook を、独自の署名のまま受け取る組み込みの種類
- 必要になれば、定期実行やポーリングを ano の内側に持つこと（今は cron と MCP で代わりにしています）
