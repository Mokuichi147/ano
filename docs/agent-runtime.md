# エージェントの実行モデル

`Agent::run` は Responses API に要求を送り、返された function call を実行して結果を次の要求へ返すループです。このページでは、ループの上限・並行実行・セッション・圧縮・トークン上限・作業計画・tool の遅延公開・サブエージェントについて説明します。

- [実行ループと上限](#実行ループと上限)
- [会話セッション](#会話セッション)
- [履歴の圧縮](#履歴の圧縮)
- [トークン使用量と上限](#トークン使用量と上限)
- [作業計画と完了判定](#作業計画と完了判定)
- [tool の遅延公開（tool_search）](#tool-の遅延公開tool_search)
- [サブエージェント（delegate_task）](#サブエージェントdelegate_task)

## 実行ループと上限

| 設定（`[agent]`） | 既定値 | 内容 |
| --- | --- | --- |
| `max_tool_rounds` | 24 | Responses 要求の回数上限。最後の1回は tool を無効にして、実行済みの内容と未完了の作業を報告するために確保します。`1` では tool を使わずに回答します |
| `parallel_tool_calls` | `true` | 1つの応答に含まれる複数の function call を並行実行するか |
| `max_parallel_tool_calls` | 8 | 同時に実行する tool 呼び出しの上限 |
| `tool_timeout_secs` | 120 | ローカル tool・直接接続 MCP tool 1回あたりのタイムアウト |
| `tool_discovery_limit` | 12 | `tool_search` 1回で有効化する tool 数 |
| `max_output_tokens` | なし | 各応答の出力トークン上限 |
| `reasoning_effort` | なし | 推論モデルの `reasoning.effort`（`none`・`minimal`・`low`・`medium`・`high`・`xhigh`）。CLI では `--reasoning-effort` |
| `reasoning_summary` | なし | 推論の要約（`auto`・`concise`・`detailed`）。要約は `reasoning_summary` イベントとして進捗に表示 |
| `project_instructions` | `["AGENTS.md"]` | instructions の末尾に追加する workspace 内のファイル（[README](../README.md#プロジェクト指示agentsmd)） |

- モデルが不正な JSON 引数を返した場合や tool が失敗した場合は、実行を中断せず、エラー内容を tool 出力としてモデルへ返します。
- `workspace_check` は検証ごとの `timeout_secs`、`workspace_exec` は呼び出しの `timeout_secs`（既定120秒、最大1800秒）を使い、結果回収のために外側の制限へ5秒の猶予を設けます。
- API が `incomplete`・`failed` などの未完了状態を返した場合は、その応答のローカル tool を実行せずにエラーにします。回答拒否（refusal）の説明文はそのまま利用者へ返します。
- 並行実行した結果は、応答内の順序でモデルへ返します。MCP の承認要求は並行実行中でも1件ずつ承認ハンドラーへ渡すため、CLI の確認プロンプトが混ざることはありません。
- `Agent::with_event_listener` を使うと、tool 呼び出しなどのイベントを発生時点で受け取れます（CLI はこれで進行状況を stderr に表示し、Webhook はジョブの `recent_events` に記録します）。
- 推論設定はどちらも未指定なら送信しません。対応値はモデルによって異なり、非対応の値は API がエラーを返します。
- Responses API への要求には `api.timeout_secs`（既定600秒）のタイムアウトがあり、接続失敗と 429 / 5xx は `api.max_retries`（既定2回）まで `Retry-After` を尊重して再試行します。

## 会話セッション

`--session` でセッションファイルを指定すると、プロセスをまたいで会話を続けられます。`ano chat` は `--session` がなくても、プロセス内のメモリで同じ仕組みの会話を保持します。

```sh
ano run --environment default --session .ano/review.json "リポジトリを調査して問題点を整理して"
ano run --environment default --session .ano/review.json "前回の調査結果を使って修正して"
ano session .ano/review.json          # 状態・計画・累積使用量を表示
ano session .ano/review.json --json   # 保存内容をすべて JSON で表示
```

- セッションはユーザー・環境・workspace・Responses API endpoint に束縛され、別の実行コンテキストでは開けません。
- 履歴と tool 結果をローカルに保存し、次の要求では完全な履歴を `store:false` で再送します。
- 実行中のセッションは sidecar lock（`<名前>.lock`）で二重起動を防ぎます。壊れたファイルはそのまま残して読み込みを拒否します。サイズ上限は 32 MiB です。
- プロセスが中断して `running` のまま残ったセッションは、workspace の状態を確認してから `--recover-session` を付けて再開します。エラーを記録して `failed` になったセッションは通常どおり再開できます。
- `.ano/` は既定で Git 管理対象外です。

### tool 呼び出しの記録と中断時の扱い

tool 呼び出しは実行前に保存し、並行実行では応答が返ったものから個別に結果を保存します。遅い tool の待機中に中断しても、保存済みの結果は復元時に維持されます。

結果が未記録の呼び出しだけを「結果不明」として扱い、自動で再実行しません。外部操作の完了と結果保存の間に停止した場合も結果不明になるため、再試行する前に実際の状態を確認してください。

## 履歴の圧縮

会話が長くなったら履歴を自動で圧縮できます。既定では無効です。

```sh
ano run --environment default --session .ano/work.json \
  --compact-threshold-bytes 262144 --max-total-tokens 100000 \
  "調査、修正、検証を続けて"
```

`[agent]` の `compact_threshold_bytes` でも指定でき、Webhook ジョブにも適用されます。CLI の同名オプションはその実行だけ設定を上書きします。

- 圧縮には [OpenAI の `/responses/compact`](https://developers.openai.com/api/docs/guides/compaction) を使います。各応答とその tool 結果を揃えてから次の応答の前に実行し、返されたメッセージと暗号化された状態をすべて保持して `store:false` で再送します。
- セッションなしでも、圧縮を有効にすると実行中の履歴を保持します。
- 対応していない互換 endpoint・モデルでは有効にしないでください。圧縮要求が失敗した場合は元の履歴を残してエラーで終了します。
- セッションの履歴を置き換える前に、同じディレクトリへ `<セッション名>.archive-<UUID>.json` を保存します。`ano session PATH --json` の `compactions` で記録と退避ファイル名を確認できます。退避ファイルは自動削除しないため、不要になったら利用者が整理してください。
- 作業計画と累積使用量は圧縮後も別に保持します。

しきい値は履歴 JSON のバイト数（1024〜16777216）で、トークン数やモデルのコンテキスト上限を保証するものではありません。暗号化された出力のバイト数が増える場合もあるため、再圧縮には前回の出力から少なくともしきい値の半分の履歴増加が必要です。大きいファイルは分割して読み、コンテキスト上限に達する前に圧縮される値を設定してください。

## トークン使用量と上限

結果の `usage` は今回の実行で API が返した使用量の合計です。

| フィールド | 内容 |
| --- | --- |
| `input_tokens` / `output_tokens` / `total_tokens` | 入力・出力・合計トークン |
| `cached_input_tokens` / `reasoning_tokens` | 上記の内訳（総量へ二重加算しません） |
| `responses` / `compactions` | 応答回数・圧縮回数（圧縮の使用量も合算） |
| `unreported_requests` | 使用量が欠落・不正だった回数 |

`ano session PATH` はセッション全体の累積値を表示します。HTTP リトライや応答を受信できなかった要求の消費量は計測できないため、課金額の算出には使えません。

`max_total_tokens`（`[agent]` または `--max-total-tokens`）は応答受信後に確認するソフト上限です。

- 1回の応答で超過することがあり、送信済みのリモート MCP 操作も取り消せません。
- 上限に達したら次の API 要求と新しいローカル tool の実行を止め、`stop_reason: "token_limit"`・`outcome: "incomplete"` を返します。上限指定中に使用量を取得できなければ `"usage_unavailable"` で止めます。
- 未実行の tool call は未実行として履歴を閉じ、セッションは同じファイルから再開できる状態にします。
- 上限は実行ごとに数え直し、以前の操作を自動で再実行しません。
- 応答生成前に圧縮だけで止まった場合の `response_id` は空文字列です。

### 停止理由（stop_reason）

| 値 | 意味 |
| --- | --- |
| `final_answer` | 通常の最終回答 |
| `round_limit` | `max_tool_rounds` の最後の（tool なしの）応答 |
| `token_limit` | `max_total_tokens` に到達 |
| `usage_unavailable` | 上限指定中に使用量を取得できなかった |

停止理由と作業の完了状態（`outcome`）は別なので、両方を確認してください。Webhook の終了結果にも `usage` と `stop_reason` が入り、トークン上限による停止は計画の有無にかかわらず `incomplete` になります。

## 作業計画と完了判定

モデルは `task_plan` で工程を作り、進捗を更新します。

- 各工程は一意の `id`、`description`、`status`（`pending`・`in_progress`・`completed`・`blocked`）、`detail` を持ちます。`blocked` には理由が必要で、`in_progress` は同時に1つまでです。
- `steps:null` で現在の計画を読みます。更新時は全工程と `expected_revision`（初回は0）を送ります。同じ版に対する並行更新は一方を拒否し、変更の消失を防ぎます。
- 既存工程の削除・変更には `explanation` が必要です。

未完了の工程があるのにモデルが最終回答を返した場合、ランタイムは残りの `max_tool_rounds` 内で作業の継続を促します。結果の `outcome` は次のとおりです。

| outcome | 条件 |
| --- | --- |
| `completed` | 全工程が完了した、または計画が作られなかった |
| `blocked` | 残りの工程がすべて実行不能（`blocked`） |
| `incomplete` | 上限までに終わらなかった工程がある |

これはモデルが記録した工程の状態であり、成果物の正しさは `workspace_check` などの検証結果で確認してください。

`--session` を使うと計画も保存され、`ano session PATH` で残りの工程を確認できます。計画導入前のセッションも読み込めます。`task_plan` は `tool_search` と同じく常時利用できるランタイム機能で、allowlist への追加は不要です。`disabled_tools = ["task_plan"]` または `--disable-tool task_plan` で無効化できます。

## tool の遅延公開（tool_search）

tool や MCP server を大量に登録しても、初回の Responses 要求へ全件は送りません。最初は固定サイズの `tool_search`・`task_plan`・`delegate_task` だけを公開し、モデルが capability を検索した後、上位 `tool_discovery_limit` 件だけを次の要求へ追加します。登録数に比例して tool schema が毎回トークンを消費することを防ぐためです。

- モデルは `tool_search` に「issue を一覧」「ファイルを読む」のように capability を渡します。必要な tool が変わったら再度検索し、新しい検索結果が前回の選択を置き換えます。
- 同じ応答内で `tool_search` を呼んだ場合、その結果は次の要求から有効になります。同時に呼ばれた他の tool は、モデルがその応答を生成した時点の tool 一覧で判定します。
- ユーザーの `disabled_tools` に含まれる tool は要求から除外します。万一モデルが直接呼び出しても、実行前にもう一度確認して `tool_disabled` を返します。

MCP tool の検索対象は [MCP の設定](mcp.md#検索カタログtool_catalog) を参照してください。

## サブエージェント（delegate_task）

`delegate_task` は、まとまった作業を新しい会話のサブエージェントに任せ、その最終回答（報告）だけを受け取るランタイム tool です。多数のファイルを調べる調査や独立した部分作業を切り出すことで、元の会話の履歴を小さく保てます。

- サブエージェントは同じモデル設定・tool・ポリシー・workspace・承認ハンドラーで動き、元の会話は見えません。モデルは `task` に目的・前提・報告してほしい内容を書きます。
- サブエージェントはさらに委任できません（1段まで）。1つの応答に複数の `delegate_task` があれば、他の tool と同じく並行実行します。
- tool 出力は `report`（最終回答）・`outcome`・`stop_reason` と、サブエージェントが作った計画（`plan`）です。失敗した場合は `subagent_failed` として元のエージェントに返し、実行は続けます。
- 承認の判定には、モデルが書いた `task` ではなく元のユーザーの依頼文を使います。
- 使用量は元の実行の `usage` に合算し、セッションにも記録します。`max_total_tokens` はサブエージェントの開始時点の残りを上限として引き継ぎます。並行実行したサブエージェントは互いの消費を見ないため、上限を超えることがあります（ソフト上限）。
- サブエージェントのイベントは `Agent::with_event_listener` のリスナーに通知されます（CLI の進捗表示にも出ます）が、元の実行の `events` には `subagent_started` / `subagent_finished` だけが入ります。
- `disabled_tools = ["delegate_task"]` または `--disable-tool delegate_task` で無効化できます。

