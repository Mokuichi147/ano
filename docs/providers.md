# 接続先とプリセット

## 複数の接続先を切り替える

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
- 指定できる項目は `[api]` と同じ（`auth`・`chatgpt_auth_file`・`base_url`・`api_key_env`・`wire_api`・`timeout_secs`・`max_retries`・`stream`・`context_window`）に加え、`model` と `approval_model`（`approval_mode = "auto"` の判定用モデル）です。`timeout_secs`・`max_retries`・`stream` を省略すると `[api]` の値を引き継ぎます。`base_url`・`api_key_env`・`wire_api`・`context_window` は引き継がず、`base_url` と `api_key_env` の省略時は OpenAI の既定値になります。`context_window` はモデルが1回の要求で受け取れるトークン数で、履歴をその手前で圧縮します（LM Studio は自動で取得。[履歴の圧縮](agent-runtime.md#履歴の圧縮)）。
- モデルは「環境の指定 → `--preset`/`--provider`/`--model` → 対話中の `/preset`/`/provider`/`/model`」の順に上書きします（[プリセット](#プリセットとロール)）。接続先だけを選んだ場合は、その接続先の `model` に切り替わります。接続先に `model` がなければ、それまでのモデルをそのまま使います。
- `[agent].model` は `[api]` のモデルで、`model` のない既定の接続先でも使います。`[agent].approval_model` は `[api]` 用です。ほかの接続先では、その接続先の `approval_model`（省略時は使用中のモデル）で審査します。
- `ano chat` では `/provider lan` や `/model qwen3:30b` で、会話を保ったまま切り替えられます。接続先を変えると、それまでの会話が新しい接続先へ送られます。元の接続先でしか読めない暗号化された推論は履歴から除きます。OpenAI の `/responses/compact` で圧縮済みの会話は、ほかの接続先では読めないため移せません（`/clear` か新しいセッションで始めてください）。
- `--session` の会話は、切り替えた接続先・モデル・推論の強さを記録し、次に `--preset`・`--provider`・`--model` を付けずに再開したときもそれを使います（`--reasoning-effort` だけを付けた場合は、記録した接続先とモデルのまま推論の強さだけを変えます）。保存時と異なる接続先で再開するには `--provider` を明示してください。明示しない限り、別の接続先へ会話を送ることはありません。
- `ano serve` は、環境とロールが参照する接続先へ起動時に接続を確認し、ジョブごとに環境の接続先・モデル・推論の強さを使います。無効にした接続先やモデルを選ぶ環境は起動を止めず、その環境のジョブがエラーになります（あとで有効にすれば再起動は不要です）。

### 接続先の管理

`ano provider` で接続先を、`ano model` で接続先ごとのモデルを、設定ファイルを直接編集せずに管理できます。設定ファイルのコメントや書式は保ったまま該当箇所だけを書き換えます。書き換え後の設定が不正になる場合は保存しません。設定ファイルがなければ OS 標準の設定ディレクトリに作成します（[設定ファイル](configuration.md#設定ファイル)）。

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

### フォールバック

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

## プリセットとロール

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
