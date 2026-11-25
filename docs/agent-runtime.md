# エージェントの実行モデル

`Agent::run` は Responses API に要求を送り、返された function call を実行して結果を次の要求へ返すループです。このページでは、ループの上限・並行実行・ストリーミング・セッション・圧縮・トークン上限・作業計画・tool の遅延公開・サブエージェント・スキル・キャラクター・記憶・目標・フィード・スペースについて説明します。

- [実行ループと上限](#実行ループと上限)
- [回答のストリーミング](#回答のストリーミング)
- [会話セッション](#会話セッション)
- [履歴の圧縮](#履歴の圧縮)
- [トークン使用量と上限](#トークン使用量と上限)
- [作業計画と完了判定](#作業計画と完了判定)
- [ゴールと完了条件](#ゴールと完了条件)
- [tool の遅延公開（tool_search）](#tool-の遅延公開tool_search)
- [サブエージェント（delegate_task）](#サブエージェントdelegate_task)
- [変更のレビュー（review_changes）](#変更のレビューreview_changes)
- [スキル（skill_read / skill_save）](#スキルskill_read--skill_save)
- [キャラクター（character_update）](#キャラクターcharacter_update)
- [記憶（memory_save / memory_forget）](#記憶memory_save--memory_forget)
- [目標（objective_set / objective_log / objective_read）](#目標objective_set--objective_log--objective_read)
- [フィード（feed_post / feed_read）](#フィードfeed_post--feed_read)
- [スペース（space_save / space_read / space_data_set）](#スペースspace_save--space_read--space_data_set)
- [予定の候補（agenda_suggest）](#予定の候補agenda_suggest)
- [記事の出来事（event_save / event_search）](#記事の出来事event_save--event_search)

## 実行ループと上限

| 設定（`[agent]`） | 既定値 | 内容 |
| --- | --- | --- |
| `max_tool_rounds` | 100 | Responses 要求の回数上限。作業量の予算ではなく、暴走を止める歯止めです（1回の応答で並行して呼ぶ tool は1回と数えます）。最後の1回は tool を無効にして、実行済みの内容と未完了の作業を報告するために確保します。`1` では tool を使わずに回答します。CLI では `--max-tool-rounds` で実行ごとに変えられます。上限で止まった場合、CLI は結果の後に続け方を表示します。費用を抑えるには `max_total_tokens` を使います |
| `parallel_tool_calls` | `true` | 1つの応答に含まれる複数の function call を並行実行するか |
| `max_parallel_tool_calls` | 8 | 同時に実行する tool 呼び出しの上限 |
| `tool_timeout_secs` | 120 | ローカル tool・直接接続 MCP tool 1回あたりのタイムアウト |
| `max_tool_output_bytes` | 131072 | tool 出力（JSON のバイト数）の上限。超えた出力は先頭と末尾だけを残し、`truncated: true` と元のサイズを付けてモデルへ返します（4096 以上） |
| `tool_discovery_limit` | 12 | `tool_search` 1回で有効化する tool 数。検索で読み込んだ tool は、この2倍まで残します |
| `max_output_tokens` | なし | 各応答の出力トークン上限 |
| `reasoning_effort` | なし | 推論モデルの `reasoning.effort`（`none`・`minimal`・`low`・`medium`・`high`・`xhigh`・`max`・`ultra`。対応範囲はモデルによる）。CLI では `--reasoning-effort`、`ano chat` では `/effort`。接続先・モデルと組にするには[プリセット](providers.md#プリセットとロール)を使う |
| `reasoning_summary` | なし | 推論の要約（`auto`・`concise`・`detailed`）。要約は `reasoning_summary` イベントとして進捗に表示 |
| `project_instructions` | `["AGENTS.md"]` | instructions の末尾に追加する workspace 内のファイル（[プロジェクト指示](configuration.md#プロジェクト指示agentsmd)） |

- モデルが不正な JSON 引数を返した場合や tool が失敗した場合は、実行を中断せず、エラー内容を tool 出力としてモデルへ返します。
- `workspace_check` は検証ごとの `timeout_secs`、`workspace_exec` は呼び出しの `timeout_secs`（既定120秒、最大1800秒）を使い、結果回収のために外側の制限へ5秒の猶予を設けます。
- API が `incomplete`・`failed` などの未完了状態を返した場合は、その応答のローカル tool を実行せずにエラーにします。回答拒否（refusal）の説明文はそのまま利用者へ返します。
- 応答が出力トークンの上限（`max_output_tokens`、またはコンテキスト長の残り。Chat Completions の `finish_reason: "length"`）で途切れた場合だけは、その応答を捨てて、同じ要求を一度だけ送り直します。途切れた応答の tool は実行せず、履歴にも残しません。送り直す要求には、推論を短くし、長い出力は複数の tool 呼び出しに分けるよう求める通知を末尾に添えます（この通知も履歴には残しません）。コンテキスト長が埋まって途切れた場合は、送り直す前に[履歴を圧縮](#履歴の圧縮)します。続けてもう一度途切れた場合と、最後の報告の応答が途切れた場合はエラーにします。途切れた応答の使用量は `usage` に含まれ、端末にはその途中までの文章が表示されたままになります。
- 並行実行した結果は、応答内の順序でモデルへ返します。MCP の承認要求は並行実行中でも1件ずつ承認ハンドラーへ渡すため、CLI の確認プロンプトが混ざることはありません。
- `Agent::with_event_listener` を使うと、tool 呼び出しなどのイベントを発生時点で受け取れます（CLI はこれで進行状況を stderr に表示し、Webhook はジョブの `recent_events` に記録します）。
- 推論設定はどちらも未指定なら送信しません。対応値はモデルによって異なり、非対応の値は API がエラーを返します。
- Responses API への要求には `api.timeout_secs`（既定600秒）のタイムアウトがあり、接続失敗と 429 / 5xx は `api.max_retries`（既定2回）まで `Retry-After` を尊重して再試行します。

## 回答のストリーミング

`[api] stream = true`（既定）のとき、CLI は stdout が端末なら回答を生成しながら表示します。

- 要求に `stream: true` を付け、server-sent events の `response.output_text.delta` を順に表示します。最終的な応答は `response.completed` のものを使うため、tool 実行や履歴の扱いはストリーミングしない場合と同じです。
- 端末向けの整形は Markdown のブロック（空行で区切られた段落・リスト・表、閉じたコードブロック）ごとに行います。`--raw` では届いた文字をそのまま表示します。
- tool 呼び出しの前にモデルが書いた説明文も表示します。推論モデルが考えている間は `[thinking]` を進捗として表示します。
- 端末では、thinking・推論の要約・tool 呼び出し・計画の更新といった途中経過を1行のステータス行に上書き表示し、画面を埋めないようにします。失敗・拒否・中断など注意が必要なものだけをログに残します。計画の全体は `/plan` で確認できます。`--verbose` を付けるか、stderr が端末でない場合は、従来どおりすべてを行として出力します。
- `--json` やパイプへの出力、Webhook ではストリーミングしません。サブエージェントの回答も表示せず、報告として元のエージェントに返します。
- `stream: true` を無視して JSON を返す互換 endpoint でも動きます（回答は完了後にまとめて表示）。うまく動かない場合は `stream = false` にしてください。
- ライブラリでは `Agent::with_text_listener` で差分を受け取れます。`AgentResult::streamed` が `true` なら、最終回答はすでにリスナーへ渡されています。

## 会話セッション

`--session` でセッションファイルを指定すると、プロセスをまたいで会話を続けられます。`ano chat` は `--session` がなくても、プロセス内のメモリで同じ仕組みの会話を保持します。

```sh
ano run --environment default --session .ano/review.json "リポジトリを調査して問題点を整理して"
ano run --environment default --session .ano/review.json "前回の調査結果を使って修正して"
ano session .ano/review.json          # 状態・計画・累積使用量を表示
ano session .ano/review.json --json   # 保存内容をすべて JSON で表示
```

- セッションはユーザー・環境・workspace・Responses API endpoint に束縛され、別の実行コンテキストでは開けません。endpoint だけは `--provider` の明示か `ano chat` の `/provider` で切り替えられます。その際、元の endpoint でしか読めない暗号化された推論を履歴から除きます。リモート圧縮済みの会話は移せません（[複数の接続先を切り替える](providers.md#複数の接続先を切り替える)）。
- `--preset`・`--provider`・`--model`・`--reasoning-effort` や `/preset`・`/provider`・`/model`・`/effort` で選んだ接続先・モデル・推論の強さはセッションに記録され、指定なしで再開したときに引き継がれます。
- 履歴と tool 結果をローカルに保存し、次の要求では完全な履歴を `store:false` で再送します。
- 実行中のセッションは sidecar lock（`<名前>.lock`）で二重起動を防ぎます。壊れたファイルはそのまま残して読み込みを拒否します。サイズ上限は 32 MiB です。
- プロセスが中断して `running` のまま残ったセッションは、workspace の状態を確認してから `--recover-session` を付けて再開します。エラーを記録して `failed` になったセッションは通常どおり再開できます。
- `.ano/` は既定で Git 管理対象外です。

### tool 呼び出しの記録と中断時の扱い

tool 呼び出しは実行前に保存し、並行実行では応答が返ったものから個別に結果を保存します。遅い tool の待機中に中断しても、保存済みの結果は復元時に維持されます。

結果が未記録の呼び出しだけを「結果不明」として扱い、自動で再実行しません。外部操作の完了と結果保存の間に停止した場合も結果不明になるため、再試行する前に実際の状態を確認してください。

## 履歴の圧縮

会話が長くなったら、モデルのコンテキスト長に近づいたところで履歴を自動で圧縮します。圧縮は情報を捨てる操作なので、できるだけ遅く行います。

- **コンテキスト長**：`[api]` と `[providers.<name>]` の `context_window`（トークン数）で指定します（`ano provider set NAME --context-window N`）。LM Studio と llama.cpp の server（`/v1/models` の `meta.n_ctx`）は読み込み中のモデルのコンテキスト長を返すので、指定がなくても使います（実行の開始時にモデルが読み込まれていなければ、最初の応答の後に問い合わせます）。
- **時期**：各応答で報告される実際のトークン数（入力と出力）が、コンテキスト長から余裕分を引いた値に達したら、次の要求の前に圧縮します。余裕分は、次の要求で増える tool の結果と回答のためのもので、コンテキスト長の 1/5（少なくとも 24,000 トークン。小さいコンテキストでは 1/3）です。
- **あふれた場合**：コンテキスト長が分からない接続先でも、要求が「コンテキスト長を超えた」というエラー（OpenAI・LM Studio・llama.cpp・Ollama の文面）で拒否されたら、履歴を圧縮して一度だけ送り直します。
- **バイト数のしきい値**：`compact_threshold_bytes`（履歴を JSON にしたときのバイト数）を指定すると、それに達した時点でも圧縮します。

```sh
ano run --environment default --session .ano/work.json \
  --compact-threshold-bytes 262144 --max-total-tokens 100000 \
  "調査、修正、検証を続けて"
```

これらの設定は Webhook ジョブにも適用されます。CLI の `--compact-threshold-bytes` はその実行だけ設定を上書きします。`ano chat` では `/compact` でいつでも圧縮できます（しきい値の設定は不要）。

ano が履歴を保持していない実行（セッションなしで OpenAI などの接続先に応答を保存させて続ける場合）は、ano 自身では圧縮できません。コンテキスト長かしきい値を指定すると、ano が履歴を保持して圧縮します。

圧縮の方法は `[agent] compaction` で選びます。

| 値 | 方法 |
| --- | --- |
| `auto`（既定） | endpoint が `api.openai.com` なら `remote`、それ以外（LM Studio・Ollama など）なら `summary` |
| `remote` | [OpenAI の `/responses/compact`](https://developers.openai.com/api/docs/guides/compaction) を使う |
| `summary` | モデルに履歴の要約を書かせ、要約で置き換える。どの endpoint でも使える |

### remote

- 各応答とその tool 結果を揃えてから次の応答の前に実行し、返されたメッセージと暗号化された状態をすべて保持して `store:false` で再送します。
- 対応していない互換 endpoint・モデルでは使えません。圧縮要求が失敗した場合は元の履歴を残してエラーで終了します。

### summary

- 履歴をテキストの記録に変換し（tool の引数と結果は先頭と末尾だけに短縮、推論の暗号化データは除外）、通常の `/responses` でモデルに要約を依頼します。記録の大きさはしきい値の半分（未設定なら 128 KiB）までで、超える場合は古い項目から省きます。
- 圧縮後の履歴は、1つのユーザーメッセージ（直近のユーザーの依頼と要約）と、その後に続く直近の作業です。ユーザーの依頼をそのまま残し、その後に要約を続けます（役割の交互を求める chat template でも受け付けられる形にするため）。
- 自動の圧縮では、直近の応答とその tool の結果を、しきい値の 1/4（コンテキスト長から決める場合はその約 1/8）まで要約せずにそのまま残します（応答の途中や tool の結果だけを切り出すことはしません）。直前に読んだファイルの内容まで要約に置き換えると、モデルが同じファイルを読み直して要求の回数を使うためです。`ano chat` の `/compact` のようにしきい値のない圧縮では、すべてを要約します。
- 再度圧縮するときは、前回の要約も新しい要約に引き継ぎます。
- 要約はモデルが書くため、細部（行番号やファイルの内容など）は失われることがあります。要約にはファイルを読み直すよう注意書きを付けています。

### 共通

- セッションなしでも、圧縮を有効にすると実行中の履歴を保持します。
- セッションの履歴を置き換える前に、同じディレクトリへ `<セッション名>.archive-<UUID>.json` を保存します。`ano session PATH --json` の `compactions` で記録と退避ファイル名を確認できます。退避ファイルは自動削除しないため、不要になったら利用者が整理してください。
- 作業計画と累積使用量は圧縮後も別に保持します。圧縮の使用量は `usage.compactions` と合計に含めます。

しきい値は履歴 JSON のバイト数（1024〜16777216）で、トークン数やモデルのコンテキスト上限を保証するものではありません。暗号化された出力のバイト数が増える場合もあるため、再圧縮には前回の出力から少なくともしきい値の半分の履歴増加が必要です。大きいファイルは分割して読み、コンテキスト上限に達する前に圧縮される値を設定してください。目安として、日本語が中心の会話ではトークン数の約3倍、英語ではトークン数の約4倍のバイト数になります。

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
| `no_progress` | 同じ tool 呼び出しの繰り返しが、3回の通知の後も続いたため、報告を求めて止めた |

停止理由と作業の完了状態（`outcome`）は別なので、両方を確認してください。Webhook の終了結果にも `usage` と `stop_reason` が入り、トークン上限による停止は計画の有無にかかわらず `incomplete` になります。

## 作業計画と完了判定

モデルは `task_plan` で工程を作り、進捗を更新します。

- 各工程は一意の `id`、`description`、`status`（`pending`・`in_progress`・`completed`・`blocked`）、`detail` を持ちます。`blocked` には理由が必要で、`in_progress` は同時に1つまでです。
- `steps:null`・`goal:null` で現在の計画を読みます。更新時は全工程（または[ゴール](#ゴールと完了条件)全体）と `expected_revision`（初回は0）を送ります。`null` の項目は今の値を保ちます。同じ版に対する並行更新は一方を拒否し、変更の消失を防ぎます。
- 既存工程・ゴール・完了条件の削除や変更には `explanation` が必要です。
- 工程とゴールが今の計画と同じ更新は「変更なし」として扱い、版を進めません。結果には、呼び出しが成功したことと次に行うことを文章で添えます。計画の状態 `incomplete` をツールの失敗と取り違え、同じ計画を送り直し続けるモデルがあるためです。
- `task_plan` だけを呼ぶ応答が3回続くと、ランタイムは計画の更新をやめて作業を進めるよう促します。

未完了の工程があるのにモデルが最終回答を返した場合、ランタイムは残りの `max_tool_rounds` 内で作業の継続を促します。結果の `outcome` は次のとおりです。

| outcome | 条件 |
| --- | --- |
| `completed` | 全工程が完了した（ゴールがあれば、全完了条件の達成も確認された）、または計画が作られなかった |
| `blocked` | 残りの工程・完了条件がすべて実行不能（`blocked`） |
| `incomplete` | 上限までに終わらなかった工程、または確認されていない完了条件がある |

これはモデルが記録した工程の状態であり、成果物の正しさは `workspace_check` などの検証結果で確認してください。

`--session` を使うと計画も保存され、`ano session PATH` で残りの工程を確認できます。計画導入前のセッションも読み込めます。`task_plan` は `tool_search` と同じく常時利用できるランタイム機能で、allowlist への追加は不要です。`disabled_tools = ["task_plan"]` または `--disable-tool task_plan` で無効化できます。

## ゴールと完了条件

作業計画には、工程とは別にゴール（`goal`）を持たせられます。ゴールはユーザーが明示的に指定したときだけ使われ、普段の会話や作業では使われません。ゴールは「最終的にどうなっていれば終わりか」を表す `objective` と、それを確かめる完了条件（`acceptance`）の一覧です。ゴールがあると、工程をすべて終えただけでは完了にならず、完了条件ごとの確認が必要になります。

```sh
ano run --goal "cargo test がすべて通り、警告が増えていない" \
  --environment coding
```

```text
> /goal README の手順どおりにビルドでき、手順のコマンドがすべて成功する
> /goal          # ゴールと完了条件の状態を表示
> /goal clear    # ゴールを外す
```

- 各完了条件は `id`、`description`、`status`（`pending`・`met`・`blocked`）、`evidence` を持ちます。`met`（達成）と `blocked`（達成不能）には、確認の根拠や理由を `evidence` に書く必要があります。
- 完了条件が1つでも `pending` のままモデルが最終回答を返すと、ランタイムは未確認の条件を挙げて、検証（検証コマンドの実行や結果の読み取りなど）と記録を促します。条件を満たしていなければ作業を続けさせます。`max_tool_rounds` に達した場合は `incomplete` で終わります。
- ゴールを定められるのはユーザーだけです。モデルはゴールを作成・変更・解除できず、ゴールがないときに `task_plan` で送られたゴールは無視し、同じ呼び出しの手順だけを保存します（その旨を結果で伝えます）。多くのモデルが最初の計画にゴールを付けて送り、拒否されると1回分の要求を無駄にしていたためです。モデルが自分でゴールを定められた頃のセッションに残るゴールは、次の実行で外します。
- 完了条件は、モデルがゴールから具体的で確かめられる形に分解して定めます。

### ゴールの指定

`ano run --goal TEXT`、`ano chat` の `/goal TEXT`、Webhook の `goal` で指定できます。満たすべき条件があれば、ゴールの文中に書きます（例: 「cargo test がすべて通り、警告が増えていない」）。

- 指定したゴールは会話の作業計画を置き換えます（それまでの工程は閉じ、版番号は引き継ぎます）。`--goal` だけを指定した場合、プロンプトは省略できます。
- モデルはゴールの文（`objective`）を変更できません。更新時にモデルが送った `objective` は無視し（`null` でも可）、元の文を保ちます。モデルが文を正確に書き写せなくても、更新が拒否され続けることはありません。
- モデルは最初に、ゴールの文に書かれた条件をすべて含む完了条件を定めます。
- 最終回答の後には、ゴールと完了条件ごとの状態・根拠を表示します（`--json` では `plan.goal`）。

完了条件の `met` も、モデルが記録した判断です。重要な条件は、「`cargo test` が成功する」のように結果を機械的に確かめられる形でゴールに書いてください。

## tool の遅延公開（tool_search）

tool や MCP server を大量に登録しても、初回の Responses 要求へ全件は送りません。最初は固定サイズの `tool_search`・`task_plan`・`delegate_task` だけを公開し、モデルが capability を検索した後、上位 `tool_discovery_limit` 件だけを次の要求へ追加します。登録数に比例して tool schema が毎回トークンを消費することを防ぐためです。

- 検索で読み込める tool の名前は、実行ごとに instructions の末尾へ「ローカル tool」と「MCP server ごと（server の `description` 付き）」に分けて一覧します。最初に見えるのが `tool_search` だけだと、モデルは使える tool があることに気づかず、記憶で答えたり「できない」と返したりするためです。schema と説明文は含めず名前だけを載せ、8 KiB を超えた分は省略します（省略した tool も検索できます）。無効化された tool、`allow_exec` などで実行できない tool、常に公開している tool は載せません。
- モデルは `tool_search` に「issue を一覧」「ファイルを読む」のように capability を渡します。必要な tool が増えたら再度検索し、新しい検索結果を前に読み込んだ tool に加えます。「履歴を読む」「原文を確かめる」「候補を残す」のように手順ごとに検索しても、先に読み込んだ tool を失わないためです。読み込んだ tool が `tool_discovery_limit` の2倍を超えたら、読み込んでからいちばん時間のたったものから外します（もう一度検索で見つかった tool は新しく読み込んだものとして扱います）。1つの応答の中で複数回検索した場合も、結果を合わせて次の要求に載せます。
- セッションやチャットで次のターンを始めるときは、会話の履歴に残る `tool_search` の結果を順にたどり、新しく読み込んだものから `tool_discovery_limit` 個までを引き継ぎます（前のターンの作業の途中で使っていた tool を残しつつ、古いターンの tool で毎ターンの最初の要求が大きくならないようにするため）。前のターンで使った tool が、履歴には残っているのに一覧から消えて呼べなくなるのを防ぐためです。ポリシーと、tool がまだ存在するかは改めて確認します。圧縮で検索結果が履歴から消えた場合は、最初の状態から始めます。
- 同じ応答内で `tool_search` を呼んだ場合、その結果は次の要求から有効になります。同時に呼ばれた他の tool は、モデルがその応答を生成した時点の tool 一覧で判定します。
- 同じ tool だけを3回続けて呼ぶと、別の方法を取るよう実行時の通知を返します。その tool と同じ MCP server の tool、またはローカル tool なら同じ接頭辞（`workspace_` など）の tool のうち、まだ読み込まれていないものの名前も伝えます。読みたいのに `workspace_read` が読み込まれておらず、`workspace_search` を繰り返すような空回りを止めるためです。ファイルを順に読む・編集する・コマンドを実行するのは前進なので、`workspace_read`・`workspace_edit`・`workspace_write`・`workspace_exec` は、直前のラウンドとまったく同じ引数で呼び直した場合だけを数えます。通知を3回出しても（同じ呼び出しが9回続いても）変わらない場合は、上限を待たずに tool を外して報告を求め、`stop_reason: "no_progress"` で止めます。
- ユーザーの `disabled_tools` に含まれる tool は要求から除外します。万一モデルが直接呼び出しても、実行前にもう一度確認して `tool_disabled` を返します。

MCP tool の検索対象は [MCP の設定](mcp.md#検索カタログtool_catalog) を参照してください。

## サブエージェント（delegate_task）

`delegate_task` は、まとまった作業を新しい会話のサブエージェントに任せ、その最終回答（報告）だけを受け取るランタイム tool です。多数のファイルを調べる調査や独立した部分作業を切り出すことで、元の会話の履歴を小さく保てます。

- サブエージェントは同じ tool・ポリシー・workspace・承認ハンドラーで動き、元の会話は見えません。モデルは `task` に目的・前提・報告してほしい内容を書きます。
- 接続先・モデル・推論の強さは、`[agent.roles] delegate` にプリセットを指定するとそれを使い、指定しなければ元のエージェントと同じです（[プリセットとロール](providers.md#プリセットとロール)）。進捗には `[subagent] started on MODEL` と使うモデルを表示します。
- サブエージェントはさらに委任できません（1段まで）。1つの応答に複数の `delegate_task` があれば、他の tool と同じく並行実行します。
- tool 出力は `report`（最終回答）・`outcome`・`stop_reason` と、サブエージェントが作った計画（`plan`）です。失敗した場合は `subagent_failed` として元のエージェントに返し、実行は続けます。
- 承認の判定には、モデルが書いた `task` ではなく元のユーザーの依頼文を使います。
- 使用量は元の実行の `usage` に合算し、セッションにも記録します。`max_total_tokens` はサブエージェントの開始時点の残りを上限として引き継ぎます。並行実行したサブエージェントは互いの消費を見ないため、上限を超えることがあります（ソフト上限）。
- サブエージェントのイベントは `Agent::with_event_listener` のリスナーに通知されます（CLI の進捗表示にも出ます）が、元の実行の `events` には `subagent_started` / `subagent_finished` だけが入ります。
- `disabled_tools = ["delegate_task"]` または `--disable-tool delegate_task` で無効化できます。

## 変更のレビュー（review_changes）

`review_changes` は、workspace の未コミットの変更を、新しい会話の**書き込みのできないレビュー担当**に確認させるランタイム tool です。Issue の解決や PR の修正のように、変更を push する作業では必ず通ります。

1. 実装したエージェントが、変更の目的（Issue の内容や要件）を `request` に書いて呼び出します。
2. ano が `git_diff` で差分を取り、目的とともにレビュー担当のサブエージェントへ渡します。レビュー担当は実装の会話を見ずに、差分と周辺のコードを読み、登録済みの検証コマンドを実行し、指摘を重要度（高・中・低）順に報告します。
3. 実装したエージェントが指摘ごとに妥当かを判断し、正しい指摘は直し、見送る指摘は理由を最終回答や PR の本文に残します。
4. `git_commit_push` は、コミットするファイルが**最後のレビュー時点の状態（内容と実行ビットの sha256）と一致する場合だけ**実行します。レビューのない変更や、レビュー後に1バイトでも、または実行ビットだけでも変わったファイルは `review_required` で拒否するため、指摘を直した後は再度レビューを受けます。照合は承認の前と、workspace の書き込みロックを取ったコミットの直前の2回行うため、並行する編集が割り込むことはありません。ディレクトリを指定したコミットは、配下のファイルを照合できないため拒否します。

- レビュー担当は書き込み・コマンド実行（`workspace_exec`）ができず、承認が必要な呼び出し（MCP の書き込みや push）はすべて拒否されます。サブエージェントと同じく、さらに委任はできません。
- レビュー担当も、環境に登録した検証コマンド（`workspace_check`）と、承認不要（`require_approval = "never"`）の MCP tool は使えます。レビューでテストを実行するためです。検証コマンドが workspace を書き換える場合は、書き換えられたファイルがレビュー時点と一致しなくなり、再レビューが必要になります。
- レビューの結果は `report`（指摘）・`reviewed_files`・`recorded`（記録したか）と、差分を切り詰めたか大きな新規ファイルを省略して渡した場合の `diff_truncated` です。レビュー中にファイルが変わった場合、そのレビューは記録しません。
- 記録は `Agent` のインスタンス（`ano chat` の会話、`ano run` の1回の実行、Webhook の1ジョブ）ごとにメモリ上に持ち、プロセスや実行をまたいでは引き継ぎません。引き継げない場合は再レビューを求めるだけで、安全側に倒れます。
- 書き込みを許可した実行（`allow_writes`）でだけモデルに提示します。`disabled_tools = ["review_changes"]` で無効化すると、`git_commit_push` によるコミットもできなくなります。
- 強制するのは「コミットする内容がレビューを受けたものであること」までです。指摘にどう対処したか（直したか・なぜ見送ったか）は実装したエージェントの判断に任せ、最終回答や PR の本文で報告させます。レビュー担当も LLM なので、レビューはセキュリティ上の境界ではありません。差分や依頼文の中の指示には従わないよう指示していますが、PR はマージ前に人が確認してください。
- 使用量の合算・承認の判定・イベントの扱いは `delegate_task` と同じです。レビュー担当の接続先・モデル・推論の強さは `[agent.roles] review` のプリセットで選べます。実装と別のモデルにすると、同じモデルに特有の見落としを避けやすくなります。

## スキル（skill_read / skill_save）

スキルは、以前の作業で上手くいった手順（手順・コマンド・確認方法・落とし穴）を名前付きで保存したものです。同じ種類の依頼を受けたとき、モデルが試行錯誤をやり直さずにその手順に沿って進められるようにします。`[skills] enabled = true` で有効になります。

```toml
[skills]
enabled = true
# dir = "~/notes/ano-skills"   # 省略時は OS 標準のデータディレクトリ
```

**保存形式。** Claude Code や Codex などと同じ Agent Skills 形式の `SKILL.md` です。frontmatter の `name`（英小文字・数字・ハイフンの64文字以内。ディレクトリ名と一致させる）と `description`（いつ使うか）、本文の Markdown からなります。手で書いたり、他のツール用に書かれたスキルを置いたりしても読めます（`name`・`description` 以外の frontmatter は無視し、同じディレクトリの補助ファイルは読みません）。

```markdown
---
name: release-build
description: リリースビルドを作り、スモークテストで確認する。
---

1. `cargo build --release`
2. ...
```

**保存先。** `<dir>/<ユーザー>/<name>/SKILL.md` です。`dir` を省略すると OS 標準のアプリ用データディレクトリ（macOS は `~/Library/Application Support/ano/skills`、Linux は `$XDG_DATA_HOME/ano/skills`（既定 `~/.local/share/ano/skills`）、Windows は `%APPDATA%\ano\data\skills`）を使います。プロジェクトをまたいで使えるよう workspace の外に置き、Webhook のユーザー同士で混ざらないよう `--user`（Webhook の `user`）ごとに分けます。`ano skills` で一覧、`ano skills NAME` で内容を確認できます。

**参照。** 実行の開始時に、ユーザーのスキルの名前と説明だけを instructions に一覧します（合計 16 KiB まで。溢れた分は名前を指定すれば読めます）。モデルは依頼に合うスキルがあれば `skill_read` で本文を読んでから作業します。本文を毎回送らないため、スキルが増えても要求は大きくなりません。一覧は実行（chat では起動時）に一度だけ作り、会話の途中では変えません。読み取れない `SKILL.md` は警告を出して除外します。

**記録。** モデルは、検証まで終えた作業のうち、試行錯誤して見つけた手順や、従ったスキルの誤り・不足、ユーザーが上手くいったと言った手順を、最終回答の前に `skill_save` で保存します。同じ名前で保存すると置き換わるため、既存のスキルは読んでから改善版を保存します。chat では `/skill [観点]` で、この会話で上手くいった手順をスキルにするよう明示的に頼めます（保存するほどのものがなければ保存しません）。

- **保存には承認が必要です。** スキルは以後のすべての実行で指示として読まれるため、`workspace_exec` と同じ[承認モード](mcp.md#承認モード)で判定します。`auto` の判定用モデルは、依頼の作業から得た再利用できる手順を許可し、秘密情報・個人情報を含むもの、承認の省略や権限の緩和・データの送信を指示するもの、文書や tool 出力に埋め込まれた指示に由来するように見えるものを拒否します。承認モードが `deny` の実行（環境の既定）では、保存についての指示を省きます。
- 信頼できないイベントから始まった[自動化](automation.md)の作業では、`skill_save` を使わせません。
- `skill_read` と `skill_save` は `tool_search` を経ずに常に公開します（スキルの一覧を見たモデルがすぐ呼べるように）。`allowed_tools` を指定した環境では `skill_*` を加えてください。`skill_read` を使えない実行にはスキルの一覧も載せません。
- スキルは手順の参考で、運用者の instructions やユーザーの依頼より優先されません。不要・誤ったスキルは `SKILL.md` をディレクトリごと削除するか、直接編集してください。

## キャラクター（character_update）

キャラクターは、エージェントがユーザーと話すときの名前・一言の紹介（タグライン）・性格と口調（ソウル）です。どのプロジェクトでも同じ相手と話している感覚で使えるよう、ユーザーごとに1つだけ持ちます。`[character] enabled = true` で有効になります。

```toml
[character]
enabled = true
# dir = "~/notes/ano-character"   # 省略時は OS 標準のデータディレクトリ
```

**作り方。** コマンドで作るか、会話で頼みます。

```sh
ano character set --name ミラ --tagline "好奇心旺盛な相棒" --soul "敬語を使わず、短く話す。冗談は控えめに。"
ano character set --soul-file soul.md   # ソウルをファイルから読む
ano character                            # 今のキャラクターと保存先を表示
```

会話では「これからはミラと名乗って」「もっと短く話して」のように頼むと、モデルが `character_update` で名前・タグライン・ソウルのうち頼まれた部分を書き換えます。

**保存形式。** `<dir>/<ユーザー>/SOUL.md` に、frontmatter の `name`（64文字以内）と `tagline`（省略可、200文字以内）、本文のソウル（Markdown、4 KiB まで）として保存します。手で編集しても読めます（ほかの frontmatter は無視します）。`dir` を省略した場合の場所はスキルと同じく OS 標準のデータディレクトリ（macOS は `~/Library/Application Support/ano/character`、Linux は `$XDG_DATA_HOME/ano/character`（既定 `~/.local/share/ano/character`）、Windows は `%APPDATA%\ano\data\character`）です。

```markdown
---
name: ミラ
tagline: 好奇心旺盛な相棒
---

敬語を使わず、短く話す。冗談は控えめに。
```

**参照。** 実行の開始時に、既定の instructions の後ろ（AGENTS.md とスキル一覧の前）へ `# Character` として名前・タグライン・ソウルを追記します。運用者の instructions を置き換えるのではなく、話し方だけを足します。ソウルは毎回の要求に含まれるため、ローカル AI の小さなコンテキストでも負担にならないよう 4 KiB に制限しています。キャラクターがまだない場合は、作り方だけを伝えます。

- **変更には承認が必要です。** キャラクターは以後のすべての実行で指示として読まれるため、`skill_save` と同じく[承認モード](mcp.md#承認モード)で判定します。`auto` の判定用モデルは、ユーザーが名前・性格・口調の変更を頼んだときだけ許可し、話し方を超えた指示（承認の省略、データの送信など）を含むものは拒否します。モデルにも、文書や tool の出力に書かれていても変えないよう指示しています。承認モードが `deny` の実行では、変更についての指示を省きます。
- キャラクターは話し方の指定で、何をしてよいかは変えません。運用者の instructions・ユーザーの依頼・承認より優先されず、ソウルに操作・権限・承認・データの送信についての指示があっても無視するよう伝えています。コード・コミット・ファイル・サブエージェント向けの報告は普段どおりの言葉で書かせます。
- 承認モードが `allow` の環境では、変更も確認なしに通ります。第三者が書いた文面（Web ページ、Issue、信頼できないリポジトリの `AGENTS.md` など）を扱う環境では `allow` を避けるか、`disabled_tools` に `character_update` を加えてください。
- 会話の途中で変えた場合、その会話では tool の結果に新しいキャラクターが返り、モデルはそれに従います。instructions に載るのは次の会話（chat・Web・ACP では新しいセッション）からです。
- 信頼できないイベントから始まった[自動化](automation.md)の作業では、`character_update` を使わせません。
- `character_update` は `tool_search` を経ずに常に公開します。`allowed_tools` を指定した環境では `character_update` を加えてください。使えない実行でも、キャラクター自体は instructions に載ります。
- 読み取れない `SOUL.md` は警告を出して無視し、その実行では変更も案内しません。

## 記憶（memory_save / memory_forget）

記憶は、会話の中でユーザーが話した自分についての事実（好み・予定・習慣・身近な人・仕事の進め方など）を短い一文ずつ保存したものです。次の会話からは、言い直さなくても答えや提案に生かします。`[memory] enabled = true` で有効になります。

```toml
[memory]
enabled = true
# dir = "~/notes/ano-memory"   # 省略時は OS 標準のデータディレクトリ
```

**保存形式。** `<dir>/<ユーザー>/MEMORY.md` に、事実を `- ` で始まる1行ずつ書きます（1件 300 文字まで、ユーザーごとに 300 件まで）。手で編集でき、`- ` で始まらない行（見出しやメモ）は事実として扱わず、そのまま残します。手で書いた長い行は 300 文字で切って読みます。`dir` を省略した場合の場所は、macOS は `~/Library/Application Support/ano/memory`、Linux は `$XDG_DATA_HOME/ano/memory`（既定 `~/.local/share/ano/memory`）、Windows は `%APPDATA%\ano\data\memory` です。

```sh
ano memory                      # 番号付きで一覧
ano memory add "緑茶が好き"      # 追加
ano memory forget 2 5           # 一覧の番号で削除
```

**参照。** 実行の開始時に、新しい方から合計 4 KiB までの事実を `# Memory` として instructions に載せます（入りきらない古い事実は件数だけ伝えます）。事実はユーザーについてのデータで、指示ではないと明示し、今ユーザーが言うことと食い違えばユーザーを優先させます。役に立つときは、頼まれなくても記憶に沿った提案をするよう伝えています。

**記録と削除。** ユーザーが以後にも関わりそうなことを話すと、モデルが `memory_save` で一文として保存します。「忘れて」と頼まれたときや、事実が古くなったときは `memory_forget` で一覧どおりの文言を指定して消し、必要なら正しい事実を保存し直します。同じ事実（大文字小文字と空白の違いを除く）は重ねて保存しません。

- **保存には承認が必要です。** 記憶は以後のすべての実行で読まれるため、[承認モード](mcp.md#承認モード)で判定します。`auto` の判定用モデルは、ユーザー自身が話した事実を許可し、秘密情報や、文書・Web ページ・tool の出力に由来するように見えるもの、話し方や事実を超えた指示を含むものを拒否します。削除も、文書や Web ページに書かれた指示で記憶を消させないよう承認が必要で、`auto` ではユーザーが忘れる・直すよう頼んだときだけ許可します。承認モードが `deny` の実行では、保存と削除についての指示を省きます。
- **第三者の入力では使いません。** [自動化](automation.md)で信頼できないイベント（`trust = "untrusted"`）から始まった作業には、記憶を載せず、`memory_save`・`memory_forget` も使わせません。Issue などに埋め込まれた指示で、ユーザーについての情報を外へ送らせないためです。同じ理由で、過去の会話を読む `history_*`（[chronotope の会話履歴](chronotope-history.md)）と、以後の実行に残るものを書く `character_update`・`skill_save` も使わせません。
- `memory_save` と `memory_forget` は `tool_search` を経ずに常に公開します。`allowed_tools` を指定した環境では `memory_*` を加えてください。
- 会話の途中で保存した事実は、tool の結果としてモデルに伝わり、instructions に載るのは次の会話からです。

## 目標（objective_set / objective_log / objective_read）

目標は、ユーザーが数週間から数か月かけて取り組むこと（習慣・貯金・学習など）と、その進捗の記録です。会話がまたがっても続き、エージェントは進んでいる目標を踏まえて次の一歩を提案し、報告された進捗を記録します。`[objectives] enabled = true` で有効になります。

1回の実行の完了条件である[ゴール](#ゴールと完了条件)（`--goal`・`/goal`）とは別のものです。ゴールはその作業が終わったかを確かめるための条件で、目標は作業をまたいで追いかけるものです。

```toml
[objectives]
enabled = true
# dir = "~/notes/ano-objectives"   # 省略時は OS 標準のデータディレクトリ
```

**中身。** 各目標は、id（`o1` のように振られ、消しても使い回しません）、タイトル、カテゴリ（health・money などの自由な一語）、理由、状態（`active`・`paused`・`done`・`dropped`）、次の一歩、期日、数値の目標（値と単位）、進捗の記録（日時・一文・そのときの値）を持ちます。

```sh
ano objectives                                                    # 進行中と保留中の一覧（--all で終わったものも）
ano objectives add "貯金" --category money --target 100000 --unit 円 --due 2026-12-31
ano objectives log o1 "今月は3万円貯めた" --value 30000             # 進捗の記録
ano objectives set o1 --next-step "スマホのプランを見直す"           # 一部の変更
ano objectives set o1 --status done
ano objectives set o1 --clear-target                               # 数値の目標を外す
ano objectives show o1                                            # 進捗の記録を含めて表示
ano objectives remove o1
```

**参照。** 実行の開始時に、進行中（`active`）の目標を、更新の新しい順に1行ずつ `# Objectives` として載せます（合計 3 KiB まで）。各行には、数値の目標に対する今の値、期日、次の一歩、最後の進捗を含めます。全体や進捗の記録は `objective_read` で読めます。

**作成と記録。** ユーザーが時間をかけて達成したいことを話すと、エージェントは目標にすることを提案し、理由や目標の数値・日付をいくつか質問して具体的にし、最初の一歩を添えて `objective_set` で作ります。進捗を報告されたら `objective_log` で記録します（数値の目標があれば、増えた分ではなくその時点の値を記録します）。

- **作成と変更（`objective_set`）には承認が必要です。** 目標は以後のすべての実行に載るためです。進捗の記録（`objective_log`）と読み取り（`objective_read`）は承認なしで行います。承認モードが `deny` の実行では、作成と変更についての指示を省きます。
- **第三者の入力では使いません。** 信頼できないイベントから始まった[自動化](automation.md)の作業には、目標を載せず、`objective_*` も使わせません。
- 保存先は `<dir>/<ユーザー>/objectives.json` です（JSON。目標は 100 件、進捗は目標ごとに新しい 200 件まで）。`dir` を省略した場合の場所は、macOS は `~/Library/Application Support/ano/objectives`、Linux は `$XDG_DATA_HOME/ano/objectives`（既定 `~/.local/share/ano/objectives`）、Windows は `%APPDATA%\ano\data\objectives` です。
- `objective_set` と `objective_log` は `tool_search` を経ずに常に公開します。`objective_read` は要求を小さく保つため常には公開せず、必要なときにモデルが `tool_search` で読み込みます（進行中の目標は instructions に載っているため、ほとんどの会話では不要です。`tool_search` を無効にした実行では、`objective_read` の案内を省きます）。`allowed_tools` を指定した環境では `objective_*` を加えてください。`objective_read` を使えない実行には目標を載せません。

## フィード（feed_post / feed_read）

フィードは、エージェントがユーザーに後で見てもらうために残すものの置き場です。記憶や目標から考えた**提案**（`idea`。実行するための依頼文つき）、興味に合う**記事**（`article`。URL つき）、自分で進めた作業の**報告**（`report`）を残します。ユーザーは `ano web` のホームや `ano feed` で読み、👍・👎 の評価や「済み」を付けます。`[feed] enabled = true` で有効になります。

```toml
[feed]
enabled = true
# dir = "~/notes/ano-feed"   # 省略時は OS 標準のデータディレクトリ
```

**先回りの提案と記事の収集。** 決まった時刻に動かすと、頼まれる前に提案や記事が届きます（[定期実行](automation.md#定期実行schedule)）。

```toml
[automation.sources.ideas]
kind = "schedule"
cron = "0 8 * * *"

[automation.sources.reading]
kind = "schedule"
cron = "30 7 * * MON-FRI"

[[automation.rules]]
name = "ideas"
sources = ["ideas"]
action = "run"
task = "記憶と目標を踏まえて、今日やるとよさそうなことを2〜3件考え、feed_post で idea として残す。feed_read で最近の評価を確かめ、似たものの繰り返しや 👎 のついた種類は避ける"
environment = "assistant"

[[automation.rules]]
name = "reading"
sources = ["reading"]
action = "run"
task = "記憶にある興味に合う新しい記事を web_fetch で探し、良いものを2〜3件 feed_post で article として残す"
environment = "reader"                  # web_fetch を許し、approval_mode = "auto" の環境
```

- `feed_post`（残す）と `feed_read`（最近の項目を評価・済みとともに読む）は承認なしで使えます。残すのはユーザーに見せる項目で、以後の実行の instructions には載せません。`feed_read` で読んだ項目は、作業の中ではデータとして扱います。`feed_post` は `tool_search` を経ずに常に公開し、`feed_read` は要求を小さく保つため常には公開せず、必要なときにモデルが `tool_search` で読み込みます（`tool_search` を無効にした実行では、投稿前に読む案内を省きます）。
- 実行の instructions には、フィードの使い方だけを短く載せます（項目そのものは載せません）。
- 保存先は `<dir>/<ユーザー>/feed.json` です（新しい 200 件まで）。`ano feed` で一覧、`ano feed rate f3 up`（`down`・`none`）で評価、`ano feed done f3` で済みにできます。
- 信頼できないイベントから始まった作業では、`feed_*` を使わせません。

## スペース（space_save / space_read / space_data_set）

スペースは、頼まれてエージェントが作る小さなページです（習慣のトラッカー、週間プランナー、イベントの一覧など）。`ano web` の `/spaces/<name>` で開きます。定期実行の作業でデータを更新すれば、開くたびに新しい内容が見られます。`[spaces] enabled = true` で有効になります。

```toml
[spaces]
enabled = true
# dir = "~/notes/ano-spaces"   # 省略時は OS 標準のデータディレクトリ
```

- **作り方。** 「毎日の運動を記録できるトラッカーを作って」のように頼むと、モデルが `space_save` で1枚の HTML（CSS と JavaScript はページ内に書く）とデータ（JSON、64 KiB まで）を保存します。ページは `window.ANO_SPACE.data` を読んで表示します。直すときは `space_read` で読んでから保存し直し、表示する中身だけを変えるときは `space_data_set` でデータを置き換えます。
- **隔離。** `/spaces/<name>` は外枠のページで、スペースの中身はその中の iframe（`sandbox="allow-scripts allow-forms allow-modals"`、中身は `/spaces/<name>/frame`）に表示します。中身は CSP の `sandbox` でオリジンを持たないページとして動くので、`ano web` の Cookie も API も使えず、`connect-src 'none'` などで通信と外部からの読み込みも禁じます。外枠の CSP（`default-src 'self'`）が iframe を外部のサイトへ遷移させないので、データを URL に載せて持ち出すこともできません。ポップアップと最上位のページの遷移も許しません。データはサーバーが中身に埋め込み、`</script>` で抜け出せないようにエスケープします。
- **今の制約。** ページの中で付けたチェックやメモは保存されません（ページを開き直すと戻ります）。記録を残したいものは、会話で頼んでデータを更新させてください。
- 保存先は `<dir>/<ユーザー>/<name>/`（`index.html`・`data.json`・`space.json`）で、ユーザーごとに 30 件まで、ページは 200 KiB までです。名前は英小文字・数字・ハイフンです。`ano spaces` で一覧、`ano spaces remove <name>` で削除できます。
- 実行の instructions には、作り方と、ユーザーのスペースの名前と題名を載せます。`space_*` は承認なしで使えます（ページは隔離されるため）。`space_save` は `tool_search` を経ずに常に公開し、`space_read` と `space_data_set` は要求を小さく保つため常には公開せず、必要なときにモデルが `tool_search` で読み込みます。データだけを更新するときに `space_save` でページごと作り直さないよう、instructions で伝えます（`tool_search` を無効にした実行では、既存のスペースを記憶から作り直さないよう伝えます）。信頼できないイベントから始まった作業では、`space_*` を使わせません。

## 予定の候補（agenda_suggest）

会話の中でユーザーがふと口にした予定（「来週の金曜15時に歯医者」「12月3日は友達の結婚式」など）を、リマインダーの候補としてフィードに残します。リマインダーにするかはユーザーがフィードで決めます（「やってみる」で、承認つきの `routine_create` が作ります）。`[agenda] enabled = true` で有効になります。候補を残す先の `[feed]` と、リマインダーを作る `[routines]` も有効にしてください（どちらかが無効だと設定の誤りになります）。

```toml
[feed]
enabled = true

[routines]
enabled = true

[agenda]
enabled = true
```

**時刻の読み取り。** モデルは予定の名前と、時刻を**ユーザーの言葉のまま**（「来週の金曜15時」）渡します。ano は [chronotope](chronotope-history.md) の時間表現の解析（`chronotope-core`）で、それを**ユーザーが話した時刻を基準に**、その時点の現地の時刻として読みます。夜に話した「明日」は翌日、先週の会話の「明日」は先週の翌日です。モデルに日付を計算させないので、ローカル AI でも曜日や月末を取り違えません。

- フィードには「`2026-11-27 15:00 歯医者`」のように読み取った日時を題名に付け、本文にユーザーの元の言葉を残します。読み違いがあれば一目でわかります。時刻は「15時」のように言ったときだけ付け、「午後」「15時ごろ」のような幅のある言い方では日付だけにします。
- 「来週末」のように 2 日までの期間は、最初と最後の日（`2026-11-28〜2026-11-29`）で候補にします。「今晩」「明朝」のような時間帯は日付で示します。
- 次のものは候補にせず、理由をモデルに返します（モデルは飛ばすか、ユーザーに聞き直します）。日が決まらない言い方（「来月」「来週」「そのうち」）、繰り返し（「毎週金曜」。頼まれたら `routine_create` の cron で作ります）、もう過ぎた予定。
- 同じ題名（日時と名前）の候補がフィードに残っていれば、済みのものも含めて新しく作らずにそれを返します。リマインダーにした予定を、あとで履歴から拾い直しても重ねて出しません。
- 題名は「やってみる」で送る依頼文にそのまま入るので、かぎ括弧や引用符を含む題名は受け付けません。
- 時刻は、話した時点の時差で読んだうえで、予定の日の時差に合わせ直します（夏時間の切り替えをまたいでも、言った時刻のままになります）。
- ユーザー自身が話した予定だけを対象にし、文書・Web ページ・tool の結果に書かれた予定は残しません。`agenda_suggest` は承認なしで使えます（フィードに候補を置くだけで、リマインダーの作成には承認が要るため）。
- `agenda_suggest` は要求を小さく保つため常には公開せず、モデルが `tool_search` で読み込みます。`tool_search` を無効にした実行では、instructions に案内を載せません。信頼できないイベントから始まった作業では使わせません。

**過去の会話から拾う。** [chronotope の会話履歴](chronotope-history.md)を有効にしていれば、定期実行で、その日の会話から予定を拾い直せます。`said_at` に発言の `received_at` を渡すので、「明日」はその発言の翌日として読まれます。

```toml
[automation.sources.agenda]
kind = "schedule"
cron = "0 21 * * *"

[[automation.rules]]
name = "agenda"
sources = ["agenda"]
action = "run"
task = "history_search で今日のユーザーの発言（origins: [human]）を読み、日の決まった予定の話があれば agenda_suggest で候補にする。when には発言の中の言い方をそのまま、said_at には発言の received_at を渡す"
environment = "assistant"
```

## 記事の出来事（event_save / event_search）

ユーザーが共有した記事や「後で読む」記事に、日付の決まった出来事（イベント・展示・セール・発売など）があれば、その出来事を保存して、あとで期間から探せるようにします。`[events] enabled = true` で有効になります。出来事は [chronotope](chronotope-history.md) に保存するので、`[history]` も有効にしてください（無効だと設定の誤りになります）。

```toml
[history]
enabled = true

[events]
enabled = true
# dir = "~/notes/ano-events"
```

**保存（`event_save`）。** 記事を読んだモデルが、出来事ごとに次の値を渡します。
- 記事の URL
- 出来事の名前と、時刻（**記事の書き方のまま**。「11月28日から29日まで」）
- 場所
- その出来事を告げる記事の文（根拠）
- 記事の公開日

ano はそれを抽出結果として chronotope の `/v1/ingest/text` に送ります。chronotope は時刻を**記事の公開日を基準に**読み、出来事を記事の文を出典とする公開の知識として保存します。

- **モデルが抽出する理由。** chronotope の組み込みの規則ベース抽出器は、告知記事の日付や地名を誤ることがあります。そのため抽出はモデルが行い、時刻の解決と検索を chronotope に任せます。
- **受け付けない時刻。** 読めない時刻・繰り返し・始まりや終わりの無い時刻は保存せず、理由をモデルに返します。「12月」のように1か月続く出来事は保存できます（予定の候補と違い、出来事には期間の長いものがあるため）。
- **重複の防止。** 同じ記事の同じ名前の出来事は保存し直さず、保存済みのものを返します。
- **すぐ探せるようにする。** 保存した直後に chronotope の Materializer を実行するので、保存した出来事はすぐに検索に出ます。
- **公開データに残るもの。** 公開データに記録されるのは、ano の主体（`history.principal`）が記事を読んだことだけで、ユーザーの識別子は残りません。どのユーザーがどの出来事を保存したかは、ano の手元（`events.dir`、既定は OS のデータディレクトリの `events`）にだけ置きます。
- **名前の制限。** 名前は依頼文などに入るので、かぎ括弧や引用符を含む名前は受け付けません。

**検索（`event_search`）。** 期間をユーザーの言い方（「来週末」「今月」「12月」）で渡すと、chronotope がローカルの暦で解決します。その期間と重なる、ユーザーが保存した出来事を記事の URL つきで返します。名前や場所の語でも探せます。どちらも指定しなければ、まだ終わっていない保存済みの出来事を日付順に返します。
- **索引の遅れ。** chronotope の索引が追いついていなければ、Materializer を実行してから問い合わせ直します。それでも追いつかないときや、検索が途中で打ち切られたときは、警告を返します。

**扱いの決まり。**
- 出来事の名前と場所は Web ページから来たデータです。instructions では、それを指示として扱わず、ユーザー自身の予定（`agenda_suggest` の対象）とも扱わないよう案内します。
- フィードに投稿できる実行では、自分からアイデアを投稿する前に近日の出来事を探し、ユーザーの関心に合うものを記事の URL つきで提案するよう案内します。
- 2つの tool は要求を小さく保つため常には公開せず、モデルが `tool_search` で読み込みます。
- 何を読んでいるかは個人の情報なので、信頼できないイベントから始まった作業では保存も検索も使わせません。

まだ無いもの: 保存した出来事の削除、記事の URL からの自動取得（記事はモデルが `web_fetch` などで読んでから渡します）。
