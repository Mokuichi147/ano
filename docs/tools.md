# 組み込み tool

| tool | 内容 | 条件 |
| --- | --- | --- |
| `workspace_list` | ディレクトリ直下を名前順に取得（既定100件、最大1000件）。`next_after` を次の `after` に渡すと続きを取得 | workspace |
| `workspace_find` | パスのグロブ（`*.rs`、`src/**/*.ts`、`*.{toml,md}`）でファイル・ディレクトリを再帰的に探す（既定200件、最大1000件）。`.gitignore` の対象は `include_ignored:true` のときだけ含める | workspace |
| `workspace_read` | UTF-8 ファイルを `offset`（バイト位置）と `max_bytes`（既定 64 KiB、最大 10 MiB）で分割して読む。`next_offset` で続きを読み、文字の途中では分割しない。`start_line`（1始まり）と `max_lines` を指定すると行単位で読み、`next_line`・`total_lines` と全体の `sha256` を返す | workspace |
| `workspace_search` | UTF-8 テキストを再帰的に検索し、パス・行番号・列番号・抜粋を返す。既定は大文字小文字を区別する文字列検索で、`regex:true`（Rust の正規表現）、`ignore_case:true`、`include`（対象ファイルのグロブ）、`include_ignored`（`.gitignore` の対象も検索）を指定できる（既定100件、最大1000件） | workspace |
| `workspace_edit` | 置換対象がちょうど1回だけ出現することと、必要なら `expected_sha256` の一致を確認してから原子的に書き込む。`dry_run:true` で差分とハッシュを確認できる | `allow_writes` |
| `workspace_write` | UTF-8 テキストをファイルへ書き込む | `allow_writes` |
| `workspace_move` | ファイル・ディレクトリを移動（リネーム）。既存ファイルの上書きは `overwrite:true` のときだけ | `allow_writes` |
| `workspace_delete` | ファイル・リンク・空ディレクトリを削除。中身のあるディレクトリは `recursive:true` が必要。取り消しはできない | `allow_writes` |
| `workspace_check` | 環境の `checks` に登録した検証コマンドを実行。`name:null` で一覧 | `checks` |
| `workspace_exec` | シェルコマンドを workspace で実行し、終了コードと出力を返す。呼び出しごとに承認が必要（[詳細](#コマンド実行workspace_exec)） | `allow_exec` |
| `web_fetch` | 公開 Web ページを取得し、HTML を Markdown に変換して返す。`offset`・`max_bytes` で分割して読む。呼び出しごとに承認が必要（[詳細](#web-ページの取得web_fetch)） | 承認で判定（tool ポリシーで許可時） |
| `git_diff` | 未コミットの変更（新規ファイルを含む）の差分と、ファイルごとの状態と sha256（内容と実行ビットから計算）を返す | workspace |
| `git_commit_push` | 指定したファイルだけをコミットし、ブランチを remote へ push する。既定ブランチには直接コミットしない。`review_changes` を受けた内容のファイルだけをコミットできる。呼び出しごとに承認が必要（[詳細](#github-の-issue-と-pull-request)） | `allow_writes` |
| `skill_read` | 保存済みスキルの手順を名前で読む（[詳細](agent-runtime.md#スキルskill_read--skill_save)） | `[skills]` |
| `skill_save` | 上手くいった手順をスキルとして保存・更新する。呼び出しごとに承認が必要 | `[skills]` |
| `history_search` / `history_get` / `history_context` / `history_conversations` | chronotope に保存した会話・ツール履歴の原文を検索し、発言 ID から前後ごと読む | `[history]` |
| `task_plan` | 作業計画の読み書き（[詳細](agent-runtime.md#作業計画と完了判定)） | 常時 |
| `tool_search` | 登録済み tool・MCP の検索（[詳細](agent-runtime.md#tool-の遅延公開tool_search)） | 常時 |
| `delegate_task` | 作業をサブエージェントに任せ、報告を受け取る（[詳細](agent-runtime.md#サブエージェントdelegate_task)） | 常時 |
| `review_changes` | 未コミットの変更を、新しい会話の読み取り専用のレビュー担当に確認させ、指摘を受け取る（[詳細](agent-runtime.md#変更のレビューreview_changes)） | `allow_writes` |
| `echo` / `unix_time` | 動作確認用 | なし |

実際に使える tool は、ユーザーと環境の `allowed_tools` / `disabled_tools` で決まります。読み取り専用の環境で検索を使うには `allowed_tools` に `workspace_search`・`workspace_find` を加えてください。`workspace_*` は移動・削除・コマンド実行（`allow_exec` のとき）も許可する点に注意してください。

- **workspace の外には出ません。** 絶対パスや `..` を拒否し、既存の親ディレクトリを1階層ずつ正規化して workspace 内であることを確認します。シンボリックリンクを経由した書き込みも拒否し、リンクの削除・移動ではリンク先に触れません。
- **`.git` の中は書き込み・移動・削除できません。** hook や `.git/config` の書き換えで、次の git 操作時にコマンドが実行されるのを防ぐためです。workspace ルートも移動・削除できません。
- **検索量に上限があります。** `workspace_search`・`workspace_find` は 10,000 エントリ（検索はさらに 32 MiB）までを走査し、リンク・バイナリ・10 MiB 超のファイルと、`.git`・`node_modules`・`target` などの生成物ディレクトリを省略します。上限に達したら範囲を狭めて再検索します。
- **`.gitignore` に従います。** workspace 内の各ディレクトリの `.gitignore` と `.git/info/exclude` に一致するファイル・ディレクトリは検索しません（Git リポジトリでなくても適用）。`path` で明示したディレクトリは、それ自体が無視対象でも検索します。省いた数は結果の `ignored` / `skipped_ignored` に入ります。
- **検証コマンドは設定で固定されます。** `workspace_check` のコマンドと引数は設定ファイルで決まり、workspace を作業ディレクトリとして実行し、出力は上限付きで返します。検証ごとの `timeout_secs` を優先し、タイムアウト時も取得済みの出力を返します。Webhook のジョブ全体の制限は引き続き適用されます。

## コマンド実行（workspace_exec）

`git`・ビルド・個別のテスト・プロジェクトのスクリプトなど、他の tool で扱えない操作のために、シェルコマンドを実行できます。既定では無効で、環境の `allow_exec = true` か、環境を指定しない CLI 実行の `--allow-exec` で有効になります。

```sh
ano chat --allow-writes --allow-exec                          # コマンドごとに [y/N] で確認
ano run --allow-exec --approval-mode auto "テストを実行して失敗を直して"  # 判定用モデルが審査
```

- **コマンドごとに承認が必要です。** MCP と同じ[承認モード](mcp.md#承認モード)（`ask`・`auto`・`allow`・`deny`）で判定し、拒否されたコマンドは実行しません。環境の既定は `deny` なので、Webhook などで使う場合は `approval_mode = "auto"` などを設定します。`auto` の判定用モデルは、調査・ビルド・テストと、そのための読み取りだけのネットワーク接続（リポジトリの remote からの `git fetch`・`git ls-remote`、依存の取得など）を許可し、依頼にない削除・履歴の書き換え・push・インストール・データの送信や依頼に要らない接続先へのネットワーク接続などは拒否します（必要なら依頼の中で明示して頼み直します）。
- Unix では `/bin/sh -c`、Windows では `cmd /C` で実行します。作業ディレクトリは workspace（`cwd` で workspace 内のサブディレクトリを指定可）で、stdin は閉じています。
- `timeout_secs`（既定120秒、最大1800秒）で打ち切り、それまでの出力を返します。Unix ではコマンドを専用のプロセスグループで起動し、終了・タイムアウト・中断（Ctrl+C）の時点で、コマンドが残したバックグラウンドプロセスも停止します。
- 出力は stdout・stderr それぞれ先頭 16 KiB と末尾 48 KiB を返します（エラーは末尾に出ることが多いため）。`workspace_check` の出力も同じ形式です。
- 名前に `KEY`・`SECRET`・`TOKEN`・`PASSWORD`・`PASSWD`・`CREDENTIAL` を含む環境変数は、コマンドに渡しません（API キーの読み出し防止）。
- **サンドボックスではありません。** コマンドは ano を起動したユーザーの権限で動き、workspace の外のファイルやネットワークにもアクセスできます。承認で内容を確認してください。

## Web ページの取得（web_fetch）

ドキュメントや Issue など、作業に必要な Web ページを読むための tool です。専用の権限設定は無く、MCP の tool と同じく tool ポリシーで許可されていれば使え、取得ごとに承認モードで判定します。ユーザーや環境の `allowed_tools` を使う場合は、その allowlist に `web_fetch` を含め、使わせたくない場合は `disabled_tools` に `web_fetch` を加えます。

- **取得ごとに承認が必要です。** URL にはデータを載せて外部へ送れるため、`workspace_exec` と同じ[承認モード](mcp.md#承認モード)で判定します。`auto` の判定用モデルは、作業に必要なページの閲覧を許可し、URL に秘密情報や workspace のデータを含むもの、文書などに埋め込まれた指示に従っているように見えるものを拒否します。
- **公開アドレスだけに接続します。** ホスト名を解決したすべてのアドレスを確認し、loopback・プライベート・リンクローカル（クラウドのメタデータ endpoint を含む）などへの接続を拒否します。リダイレクト先（最大5回）も同じく確認し、解決したアドレスに接続先を固定するため、DNS の応答が変わっても内部ネットワークには届きません。プロキシの環境変数は使いません。
- HTML は Markdown に変換し（`script`・`style`・`nav` などは除外）、`title` を返します。テキスト・JSON・XML はそのまま返し、画像などのバイナリは扱いません。`raw:true` で HTML をそのまま返します。
- 1回の取得は 30 秒・5 MiB まで、返す内容は既定 32 KiB（`max_bytes` で最大 256 KiB）です。続きは `next_offset` を `offset` に渡して読みます。
- Cookie や認証情報は送りません。ログインが必要なページは読めません。
- 取得のたびに URL を外部へ送ります。`approval_mode = "allow"` では、ページに埋め込まれた指示でモデルがデータを URL に載せて送る可能性を確認なしに許すことになります。

## GitHub の Issue と Pull Request

GitHub の操作は [GitHub MCP Server](https://github.com/github/github-mcp-server) に任せ、手元の変更のコミットと push は `git_commit_push` で行います。組み合わせると、`ano chat` で「Issue を解決して PR を作成して」と頼むだけで、Issue の読み取りから PR の作成まで進みます。

```toml
[[mcp_servers]]
label = "github"
transport = "streamable_http"
url = "https://api.githubcopilot.com/mcp/"
authorization_env = "GITHUB_MCP_TOKEN"
description = "GitHub の Issue・Pull Request・リポジトリを操作します"
# マージ・削除・リポジトリ作成などは含めない
allowed_tools = ["issue_read", "list_issues", "search_issues", "pull_request_read", "list_pull_requests", "search_pull_requests", "get_file_contents", "create_pull_request", "update_pull_request", "add_issue_comment", "pull_request_review_write", "add_comment_to_pending_review"]
```

```sh
export GITHUB_MCP_TOKEN="$(gh auth token)"   # または Personal Access Token
cd /path/to/clone
ano chat --allow-writes
> https://github.com/OWNER/REPO/issues/6 を解決して PR を作成して
> PR #8 をレビューして、気になる点をコメントして
```

モデルは `issue_read` で Issue を読み、workspace を編集し、`review_changes` で別の新しい会話のレビュー担当にレビューさせ、指摘を判断して対処してから、`git_commit_push` で push し、`create_pull_request` で PR を作ります。レビューは必須で、レビュー後にファイルを変えた場合は再度レビューを受けるまで push できません（[変更のレビュー](agent-runtime.md#変更のレビューreview_changes)）。push と PR 作成はそれぞれ承認を求めます（既定の `auto` では判定用モデルが審査し、依頼に明示されていなければ拒否）。第三者の Issue を扱う場合は、`--approval-mode ask` ですべてを `[y/N]` で自分で確認することもできます。利用できる tool は `ano mcp tools github` で確認できます（`ano mcp edit github` で選択）。

`git_commit_push` の動作は次のとおりです。

- `files` に挙げたファイル（追加・変更・削除）だけをコミットします。ほかの未コミットの変更には触れません。ディレクトリは指定できません。
- `review_changes` でレビューを受けた時点と同じ内容のファイルだけをコミットできます（[変更のレビュー](agent-runtime.md#変更のレビューreview_changes)）。
- remote の既定ブランチには直接コミットしません。既定ブランチにいるときは `branch` の名前で新しいブランチを現在のコミットから作り、未コミットの変更ごと移ります。それ以外のブランチにいるときはそのブランチに追加でコミットするため、レビューを受けた修正も同じ PR に積めます。
- push 先は `origin`（無ければ最初の remote）です。git コマンドを使うため、git の認証設定（credential helper・SSH 鍵）で push します。結果として、ブランチ・既定ブランチ（PR の向き先）・`OWNER/REPO`・コミットを返し、MCP の `create_pull_request` にそのまま渡せます。
- 既定ブランチは毎回 push 先の remote に問い合わせます（clone 時に記録された `origin/HEAD` は、既定ブランチの変更で古くなるため）。確かめられない場合と、remote に同名のブランチが既にある場合は、何も変更せずに中止します。
- push に失敗してもコミットは残り、同じブランチで再度呼び出すと push だけをやり直します。
- リポジトリの hook（`core.hooksPath` を含む）と `core.fsmonitor` は実行しません。一方、利用者の git 設定にある clean/smudge filter（git-lfs など）とコミット署名（`commit.gpgSign`）は、コミットの正しさに関わるためそのまま使います。そのため、これらに設定した外部プログラム（filter のコマンド、gpg・ssh などの署名プログラム）は `git_commit_push` から実行されます。対話が必要な署名（パスフレーズの入力など）は失敗することがあります。これらは `.git/config` か利用者の設定にしか定義できず、`.git` はエージェントから書き換えられません。hook はモデルが書き込めるファイルのため、実行すると `allow_writes` だけでコマンドを実行できてしまうからです。コミット前の検証は `workspace_check` で行ってください。
- push は外部への公開になるため、呼び出しごとに承認が必要です（`workspace_exec` と同じ[承認モード](mcp.md#承認モード)）。`allow_exec` は不要です。

Issue・PR・コメントは第三者が書けます。GitHub MCP で読んだ文面に埋め込まれた指示でモデルが動く可能性があるため、公開リポジトリでは `approval_mode = "allow"` を避け、GitHub MCP の `allowed_tools` を必要な操作に絞ってください。作られた PR の差分はマージ前に確認してください。
