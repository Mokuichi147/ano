# Webhook サーバー

`ano serve` は署名付きの HTTP リクエストでタスクを受け付け、設定済みの実行環境でエージェントを非同期ジョブとして実行します。呼び出し側は環境の名前だけを選べ、workspace のパスや tool の allowlist を外部から上書きすることはできません。

- [起動](#起動)
- [エンドポイント](#エンドポイント)
- [署名](#署名)
- [タスクの投入](#タスクの投入)
- [ジョブの状態](#ジョブの状態)
- [ジョブの中止](#ジョブの中止)
- [同時実行・保持・シャットダウン](#同時実行保持シャットダウン)
- [セキュリティ](#セキュリティ)

## 起動

`[environments.<name>]` にサーバー側の実行環境を登録してから起動します。

```toml
[webhook]
bind = "127.0.0.1:8080"
path = "/webhook/tasks"
secret_env = "ANO_WEBHOOK_SECRET"

[environments.coding]
workspace = "/path/to/my-repository"
allowed_tools = ["workspace_*"]
allow_writes = true
auto_approve_mcp = false
```

```sh
export ANO_WEBHOOK_SECRET="replace-with-a-long-random-secret"
ano serve                      # --bind / --path で設定を上書きできます
```

| 設定（`[webhook]`） | 既定値 | 内容 |
| --- | --- | --- |
| `bind` | `127.0.0.1:8080` | 待ち受けアドレス |
| `path` | `/webhook/tasks` | タスク投入のパス。固定パスのみで、`/jobs` 以下と `/healthz` は使えません |
| `secret_env` | `ANO_WEBHOOK_SECRET` | HMAC 秘密鍵を読む環境変数名 |
| `max_body_bytes` | 1048576 | リクエスト body の上限 |
| `signature_tolerance_secs` | 300 | タイムスタンプの許容誤差（リプレイ検出の期間） |
| `max_concurrent_jobs` | 2 | 同時に実行するジョブ数 |
| `max_pending_jobs` | 64 | 待機中と実行中の合計の上限 |
| `max_retained_jobs` | 1000 | 保持する終了済みジョブ数（`0` で保持しない） |
| `job_timeout_secs` | 1800 | 1ジョブの実行時間の上限（待機時間を除く） |
| `allow_unauthenticated` | `false` | 署名なしで起動する（loopback のみ） |

## エンドポイント

| メソッドとパス | 署名対象 | 内容 |
| --- | --- | --- |
| `POST <webhook.path>` | `<timestamp>.<body>` | タスクを投入し、`202` と `job_id` を返す |
| `GET /jobs/<job_id>` | `<timestamp>.<job_id>` | ジョブの状態と結果を取得 |
| `POST /jobs/<job_id>/cancel` | `<timestamp>.cancel:<job_id>` | ジョブを中止 |
| `GET /healthz` | なし | 死活確認 |

## 署名

すべてのリクエスト（`/healthz` を除く）に次のヘッダーが必要です。

- `X-Ano-Timestamp`: 現在の Unix 秒
- `X-Ano-Signature`: `sha256=<hex>`。上の表の署名対象を秘密鍵で HMAC-SHA256 した値

タイムスタンプが `signature_tolerance_secs` より古い・新しいリクエストと、一度受け付けた署名の再送（リプレイ）は `401` で拒否します。中止用の署名は `cancel:` で区別しているため、状態取得用の署名では中止できません。

## タスクの投入

```json
{
  "task": "リポジトリを確認して必要な修正を行って",
  "user": "default",
  "environment": "coding",
  "images": [{"data": "<base64>", "mime_type": "image/png"}],
  "audio": [{"data": "<base64>", "format": "wav"}]
}
```

- `user` は `[users]` に定義済みの名前（または `default`）だけを受け付けます。省略時は `default` です。
- `environment` は省略時 `default` です。未知の環境は `400` になります。
- `images` と `audio` は任意です。音声は文字起こしせず、native `input_audio`（`mp3` / `wav`）としてそのままモデルへ渡します。
- 未知のフィールド（`workspace` など）を含む body は `400` になります。`task` は空にできず、100,000 バイトまでです。

成功すると `202 Accepted` と `job_id`・`status_url`・`cancel_url` を返します。

### macOS / Linux（curl + openssl）

署名と送信には同じバイト列の body を使います。

```sh
body='{"task":"リポジトリを確認して必要な修正を行って","user":"default","environment":"coding"}'
timestamp=$(date +%s)
signature=$(printf '%s.%s' "$timestamp" "$body" \
  | openssl dgst -sha256 -hmac "$ANO_WEBHOOK_SECRET" | sed 's/^.* //')
curl -sS -X POST http://127.0.0.1:8080/webhook/tasks \
  -H "X-Ano-Timestamp: $timestamp" \
  -H "X-Ano-Signature: sha256=$signature" \
  -H 'Content-Type: application/json' \
  --data-binary "$body"
```

状態の取得では、署名対象を `"$timestamp.$job_id"` にします。

```sh
job_id='<job_id>'
timestamp=$(date +%s)
signature=$(printf '%s.%s' "$timestamp" "$job_id" \
  | openssl dgst -sha256 -hmac "$ANO_WEBHOOK_SECRET" | sed 's/^.* //')
curl -sS "http://127.0.0.1:8080/jobs/$job_id" \
  -H "X-Ano-Timestamp: $timestamp" \
  -H "X-Ano-Signature: sha256=$signature"
```

### Windows（PowerShell）

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

## ジョブの状態

`GET /jobs/<job_id>` は次のフィールドを返します。

| フィールド | 内容 |
| --- | --- |
| `status` | `queued`・`running`・`completed`・`blocked`・`incomplete`・`failed`・`cancelled`・`timed_out` |
| `result` / `stop_reason` | 実行が終了した場合の回答・停止理由 |
| `plan` / `usage` | 作業計画と使用量。実行中も最新の値を返し、終了後は最終結果の値になる |
| `recent_events` | 直近50件のイベント（tool 呼び出し・結果・計画の更新・推論の要約など）。1,000 文字を超える値は `{"truncated": true, "preview": ...}` に短縮 |
| `error` | 失敗・中止・タイムアウトの理由 |
| `created_at_unix` / `started_at_unix` / `finished_at_unix` | 各時刻。開始前・終了前は `null` |
| `cancellation_requested` | 中止が要求されたか |

長いジョブは `GET /jobs/<job_id>` を定期的に呼ぶと、`plan` と `recent_events` で進み具合を確認できます。`blocked` と `incomplete` も終了状態です。`result` と `plan` から理由と残りの工程を確認してください（[作業計画と完了判定](agent-runtime.md#作業計画と完了判定)）。

## ジョブの中止

`POST /jobs/<job_id>/cancel` は待機中・実行中のジョブを中止します。

- 未終了なら `202` と `cancellation_requested: true` を返します。その後 `GET /jobs/<job_id>` で `cancelled` への遷移を確認できます。
- すでに終了済みなら `200` で既存の結果を返すため、中止要求は再送できます。未知・削除済みのジョブは `404` です。

```sh
job_id='<job_id>'
timestamp=$(date +%s)
signature=$(printf '%s.cancel:%s' "$timestamp" "$job_id" \
  | openssl dgst -sha256 -hmac "$ANO_WEBHOOK_SECRET" | sed 's/^.* //')
curl -sS -X POST "http://127.0.0.1:8080/jobs/$job_id/cancel" \
  -H "X-Ano-Timestamp: $timestamp" \
  -H "X-Ano-Signature: sha256=$signature"
```

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

中止・タイムアウトは処理の待機を打ち切るだけです。すでに完了したファイル書き込みや外部操作は巻き戻さず、送信済みの外部操作が相手側で継続する場合もあります。独自 tool は非同期の待機を使い、同期ブロックや別途起動した処理の停止が必要なら tool 自身でも中止を扱ってください。

## 同時実行・保持・シャットダウン

- 同時に実行するジョブは `max_concurrent_jobs` までで、超えたジョブは `queued` のまま待ちます。待機中と実行中の合計が `max_pending_jobs` に達すると `503` を返します。
- `job_timeout_secs` は待機時間を除いた実行全体の上限で、API 応答待ちや tool の実行時間も含みます。上限に達したジョブは `timed_out` になり、空いた実行枠で次のジョブを開始します。
- 終了済みのジョブだけを `max_retained_jobs` 件まで保持し、終了するたびに古い結果から削除します。
- 混雑による `503` や入力不備による `400` では署名を消費しません。署名の有効期限内なら同じ要求を再送できます。同時に同じ署名を送っても、ジョブは一度しか登録されません。
- ジョブと結果はメモリ内に保持するため、サーバーの再起動をまたいだ復元には対応していません。
- Ctrl+C では新しいジョブの受付を止め、待機中・実行中のジョブへ中止を通知し、ジョブの終了を最大5秒待ってから残ったタスクを中止します。その後 MCP 接続を閉じます。
- tool の panic もジョブの `failed` として記録し、次のジョブに実行枠を返します（プロセス全体を abort する panic 設定を除きます）。

## セキュリティ

- secret を設定せずに起動する `--allow-unauthenticated` は loopback アドレスへの bind でだけ使えます。secret の環境変数が空文字の場合は起動を拒否します。
- Webhook 実行は対話端末を持たないため、MCP の承認は既定で拒否されます。環境に `approval_mode = "auto"` を設定すると、判定用モデルが承認した呼び出しだけを実行し、確認が必要と判定されたものは拒否します。すべて承認する `approval_mode = "allow"`（`auto_approve_mcp = true`）は信頼済みの環境だけにしてください。
- 書き込みは `allow_writes = true` の環境でだけ可能です。
- コマンド実行（`workspace_exec`）は `allow_exec = true` の環境でだけ使え、MCP と同じ承認モードで判定します。既定の `deny` では実行されません。`approval_mode = "auto"` では判定用モデルが承認したコマンドだけを実行します。
- Web ページの取得（`web_fetch`）は `allow_web = true` の環境でだけ使え、同じ承認モードで判定します。取得先は公開アドレスに限られ、ローカルネットワークやクラウドのメタデータ endpoint には接続しません。
