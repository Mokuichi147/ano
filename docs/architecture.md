# アーキテクチャ

ano は、特定の tool を知らない汎用のエージェント（実行ループ）と、それを workspace での作業に使うためのハーネスに分かれています。全体はクリーンアーキテクチャに沿った層で構成し、依存は常に内側（`domain`）へ向かいます。外部サービスやファイル・プロセスへのアクセスは、アプリケーション層が定義するトレイト（ポート）を通して行います。

```text
┌──────────────────────────────────────────────────────────────────┐
│ interface                                                        │
│  cli（コマンドと端末での表示・承認）  webhook（axum サーバー）     │
└──────────────────────────────┬───────────────────────────────────┘
                               │ Agent を組み立てさせる
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│ harness（workspace で作業するエージェント）                        │
│  Harness（組み立て）  settings（[agent]）  instructions  profile  │
│  models（接続先・ロール）  approval  review（ReviewGate）         │
└──────────────┬──────────────────────────────────┬────────────────┘
               │ 使う                             │ 使う
               ▼                                  ▼
┌──────────────────────────────┐   ┌──────────────────────────────┐
│ config（config.toml）        │   │ infrastructure               │
│  各層の設定を束ねる          │   │  openai・mcp・session_store   │
│  selection・edit             │   │  tools（workspace_*, git_*）  │
│                              │   │  chronotope・skills・project  │
└──────────────┬───────────────┘   └──────────────┬───────────────┘
               │                                  │ ポートを実装
               ▼                                  ▼
┌──────────────────────────────────────────────────────────────────┐
│ application（エージェント）                                       │
│  agent（実行ループ・拡張点）  ports  registry  settings  input    │
└──────────────────────────────┬───────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│ domain                                                           │
│  plan  usage  policy  session  compaction  tool  mcp  environment│
└──────────────────────────────────────────────────────────────────┘
```

`config`（`src/config/`）は各層が持つ設定型（ハーネスの `AgentConfig`、Webhook の `WebhookSettings` を含む）を `config.toml` の1ファイルに束ねるモジュールです。束ねた設定から1回の実行の接続先・モデル・推論の強さを決める処理（`selection`）と、コメントや書式を保ったまま設定ファイルを書き換える処理（`edit`）もここに置きます。`[agent]` はハーネスの `AgentConfig` が受けます。

## 依存のルール

| 層 | 依存してよい先 | 置かないもの |
| --- | --- | --- |
| `domain` | 標準ライブラリ、serde、anyhow | ネットワーク・プロセス・非同期 I/O |
| `application` | `domain` | HTTP、MCP SDK、ファイル保存形式、端末入出力、特定の tool の名前 |
| `infrastructure` | `application`、`domain` | CLI・Webhook・ハーネスの都合 |
| `harness` | `config`・`infrastructure`・`application`・`domain` | 端末入出力・HTTP サーバーの都合 |
| `interface` | すべての層 | 業務ルール、`Agent` の組み立ての詳細 |

エージェント（`domain`・`application`）の本体のコードが外側の層を参照していないこと（`domain` は `application` も参照しないこと）は、[`tests/layering.rs`](../tests/layering.rs) で確かめています。クレートのルートに再公開した型を経由する参照も違反として扱います。テストはアダプターを使ってかまいません。

`application` が外部と話す必要がある箇所は、すべて [`src/application/ports.rs`](../src/application/ports.rs) のトレイト経由です。

| ポート | 役割 | 実装 |
| --- | --- | --- |
| `ResponsesApi` | `/responses`（ストリーミングを含む）と `/responses/compact` の呼び出し | `infrastructure::openai::OpenAiClient` |
| `McpGateway` / `DirectMcpServer` | 直接接続 MCP の接続管理と tool 呼び出し | `infrastructure::mcp::McpPool` |
| `ConversationStore` | 会話の保持（変更ごとに保存） | `infrastructure::session_store::Session`（ファイル）、`infrastructure::memory_store::MemoryConversation`（メモリ） |
| `HistoryBackend` | 圧縮されない原文の記録と送信 | `infrastructure::chronotope::Chronotope` |
| `ApprovalHandler` | MCP 呼び出しと、承認が必要なローカル tool（`workspace_exec` など）の承認 | `AlwaysApprove`・`DenyApproval`（application）、`AutoApproval`（harness）、`InteractiveApproval`・対話モードの確認（interface/cli） |

テストや別の保存先・API を使う場合は、これらのトレイトを実装して `Agent` に渡せます。

例外として、`application::input` は `InputPart::Image(PathBuf)` / `InputPart::Audio(PathBuf)` のローカルファイルを読み込みます。入力の正規化と一体の処理のため、ポートには分けていません。

## エージェントとハーネス

実行ループ（`application::agent`）は、特定の tool を知らない汎用のエージェントです。モデルへの要求、tool 呼び出しの実行、履歴の圧縮、トークン上限、作業計画、`tool_search`、`delegate_task` を扱います。workspace で作業するための規則は、ハーネス（`harness`）が次の方法でエージェントに加えます。

- **tool の定義。** 実行できる条件（`ToolDefinition::available_when`）、1回の呼び出しの期限（`with_deadline`）、引数が指す対象に作用するか（`targeted`。繰り返しの判定に使う）を、tool を登録する側が定義に持たせます。実行ループは tool の名前で分岐しません。
- **拡張（`AgentExtension`）。** レジストリではなく拡張が処理するランタイム tool と、登録済みの tool の呼び出しの前に入る規則です。ランタイム tool は `ExtensionCall::run_subagent` でサブエージェントを起動できます。`review_changes` と `git_commit_push` のレビュー照合は、ハーネスの `ReviewGate` が拡張として加えます。
  - 拡張の tool は `delegate_task` と同じランタイム tool で、ポリシーのうち `disabled_tools` だけに従います（allowlist への追加は不要）。定義の実行条件・承認・期限は登録済みの tool と同じく適用します。名前は予約名・登録済みの tool・他の拡張と重なってはならず、重なると実行の開始時にエラーになります。
  - 拡張が起動するサブエージェントは、呼び出した実行と同じ利用者・環境・workspace で動き、親にない権限（書き込み・コマンド実行・Web・検証コマンド）は持てません。親が承認をすべて拒否する実行なら、子も拒否します。
  - スキーマで追加の引数を禁じた tool（`additionalProperties: false`）では、宣言にない引数を実行前に捨てます。`git_commit_push` のレビュー済みのファイル（`reviewed`）のように実行時に加える引数は、モデルからは渡せず、拡張の `prepare_call` だけが加えられます。
- **サブエージェントのロール。** `SubagentModels` はロール名（`delegate`、ハーネスの `review` など）ごとのモデルを持ちます。
- **instructions と設定。** 実行ループの `AgentSettings` は、ループ自身の tool（`task_plan`・`tool_search`・`delegate_task`）だけを前提にした既定の instructions を持ちます。`[agent]` を受けるハーネスの `AgentConfig` は、instructions を書かなければ workspace・git・web の tool の使い方を含むハーネスの既定（`DEFAULT_INSTRUCTIONS`）を使います。AGENTS.md とスキルの一覧もハーネスが instructions に加えます。
- **組み立て（`Harness`）。** 組み込み tool・原文履歴・スキルの登録と、実行ごとの `Agent` の組み立て（instructions の追記、ロールのモデル、承認モードに応じた承認ハンドラー、`ReviewGate`、原文履歴）を1か所で行います。CLI（`run`・`chat`）と Webhook は、実行設定（`ExecutionProfile`）とモデルを解決して `Harness` に渡し、進捗の表示や記録のリスナーだけを自分で付けます。

## ディレクトリ構成

```text
src/
├── lib.rs                  クレートのルート。よく使う型を再公開
├── main.rs                 バイナリ。interface::cli::run を呼ぶだけ
├── config/
│   ├── mod.rs              config.toml の読み込みと検証（各層の設定を集約）
│   ├── selection.rs        プリセットと各層の指定から、接続先・モデル・推論の強さを決める
│   └── edit.rs             ano provider / model / preset / mcp による設定ファイルの書き換え
├── domain/
│   ├── approval.rs         承認モード（ask・auto・allow・deny）
│   ├── plan.rs             作業計画と完了判定（RunOutcome）
│   ├── usage.rs            トークン使用量と停止理由
│   ├── policy.rs           ユーザー別の tool allowlist / denylist
│   ├── session.rs          会話の状態遷移（保留中の呼び出し、失敗時の補完）
│   ├── compaction.rs       履歴圧縮の判定、remote の結果の検証、summary の記録と置き換え
│   ├── tool.rs             ToolDefinition・ToolContext・予約名
│   ├── mcp.rs              MCP サーバー設定と公開可否の判定
│   ├── skill.rs            スキルの名前・説明・本文の検証
│   ├── environment.rs      実行環境と検証コマンドの設定
│   └── github.rs           git remote の URL から GitHub のリポジトリ名を読む
├── application/
│   ├── ports.rs            外部依存のトレイト
│   ├── agent/
│   │   ├── mod.rs          実行ループ（Agent::run / run_in_session）
│   │   ├── dispatch.rs     function call・MCP 呼び出し・承認・サブエージェント（delegate_task）と拡張の tool の実行
│   │   ├── extension.rs    ハーネスが tool と規則を加える拡張点（AgentExtension）
│   │   ├── discovery.rs    tool_search による遅延公開
│   │   ├── mcp_runtime.rs  1回の実行で使う MCP 接続とポリシー適用
│   │   ├── events.rs       AgentEvent と逐次通知
│   │   └── response.rs     Responses API 出力の解析
│   ├── registry.rs         ローカル tool の登録と実行
│   ├── settings.rs         実行ループの設定（AgentSettings）と、特定の tool を前提としない既定の instructions
│   ├── approval.rs         非対話の承認ポリシー（AlwaysApprove・DenyApproval）
│   └── input.rs            テキスト・画像・音声入力の組み立て
├── harness/
│   ├── mod.rs              Harness（組み込み tool・原文履歴・スキルの登録と、実行ごとの Agent の組み立て）
│   ├── settings.rs         config.toml の [agent]（実行ループの設定と、AGENTS.md・承認・接続先・ロールの設定）
│   ├── instructions.rs     既定の instructions（workspace・git・web の tool の使い方を含む）と、AGENTS.md・スキル一覧の追記
│   ├── profile.rs          実行環境から1回の実行設定を解決（ExecutionProfile）
│   ├── models.rs           接続先のクライアントと、メインのエージェント・ロールのモデルの解決
│   ├── approval.rs         承認モードから承認ハンドラーを作る（ApprovalFactory）
│   ├── auto_approval.rs    判定用モデルによる自動承認（ResponsesApi を利用）
│   └── review.rs           review_changes と git_commit_push のレビュー照合（ReviewGate）
├── infrastructure/
│   ├── openai.rs           Responses API クライアント（リトライ・SSE ストリーミング）
│   ├── mcp.rs              stdio / Streamable HTTP の MCP 接続プール
│   ├── session_store.rs    セッションファイルとロック
│   ├── memory_store.rs     プロセス内だけで保持する会話（ano chat）
│   ├── project.rs          AGENTS.md などプロジェクト指示の読み込み
│   ├── skills.rs           SKILL.md の読み書きと skill_read・skill_save
│   ├── fs.rs               原子的なファイル置き換え
│   └── tools/
│       ├── mod.rs          組み込み tool の登録（echo・unix_time）
│       ├── names.rs        ハーネスの規則が参照する組み込み tool の名前
│       ├── workspace.rs    list・read・search・find・edit・write
│       ├── paths.rs        workspace 内に限ったパスの解決と、シンボリックリンクをたどらない書き込み
│       ├── args.rs         tool 引数の読み取り
│       ├── manage.rs       move・delete
│       ├── walk.rs         上限付きのディレクトリ走査（.gitignore 対応）
│       ├── glob.rs         パスのグロブ照合
│       ├── checks.rs       workspace_check
│       ├── exec.rs         workspace_exec（シェルコマンド）
│       ├── git.rs          git_diff・git_commit_push（差分と、指定ファイルのコミットと push）
│       ├── web.rs          web_fetch（公開 Web ページの取得と Markdown 変換）
│       └── process.rs      子プロセスの実行（期限・出力上限・プロセスグループの停止）
└── interface/
    ├── cli/                clap による CLI（run・chat・tools・session・serve）、進捗表示、端末での承認
    └── webhook/            署名付き Webhook、ジョブ管理
```

## 1回の実行の流れ

1. `interface`（CLI または Webhook）が設定から `ExecutionProfile`（モデル設定・有効なポリシー・`ToolContext`）とモデルを解決します。`Harness` が workspace の `AGENTS.md` とスキルの一覧を instructions に加え、承認ハンドラー・`ReviewGate`・原文履歴とともに `Agent` を作ります。
2. `Agent::run` は入力を Responses API の `input` に変換し、`McpGateway` からポリシーで許可された MCP 接続を借ります。
3. 各ラウンドで `ResponsesApi::create_response`（テキストのリスナーがあれば `create_response_streaming`）を呼び、返ってきた `function_call`・`mcp_approval_request` を `dispatch` が並行実行します。セッションがあれば、実行前に呼び出しを、完了するたびに結果を `ConversationStore` へ保存します。
4. `tool_search` の結果は次のラウンドから tool 一覧に反映されます（`discovery`）。`delegate_task` は同じ `Agent` の実行ループを新しい会話で1段だけ再帰的に動かし、最終回答を tool 出力として返します。サブエージェントとレビュー担当（`ReviewGate` が `ExtensionCall::run_subagent` で起動）は、ハーネスが `[agent.roles]` のプリセットから解決した `SubagentModels`（クライアント・モデル・推論の強さ）があればそれで要求を送ります。
5. 最終回答で未完了の計画工程が残っていれば継続を促し、完了・中断・上限到達のいずれかで `AgentResult` を返します。

## 設計上の判断

- **会話履歴は Responses API の item 形式のまま扱う。** 独自の中間表現へ変換せず、`domain::session` や `domain::compaction` も同じ JSON を扱います。暗号化された reasoning や圧縮 item をそのまま再送する必要があるためです。
- **セッションの状態遷移と保存を分ける。** 状態遷移（`domain::session::SessionData`）はメモリ上だけで完結し、保存・ロック・アーカイブは `Session` が担当します。別の保存先を使う場合も同じ遷移規則を再利用できます。
- **組み立てはハーネスが担う。** `Harness` が組み込み tool・原文履歴・スキル・`ReviewGate` を含む `Agent` を組み立て、CLI と Webhook は実行設定とモデルを解決して渡します。プロセスごとに作る具象アダプター（`OpenAiClient`・`McpPool`）と `Session` は、バイナリのエントリポイント（CLI）が作ります。Webhook サーバーはクライアントと MCP ゲートウェイを受け取るだけで、自分では作りません。
- **実行ループは tool の名前を知らない。** tool ごとの条件・期限・繰り返しの扱いは定義に、tool をまたぐ規則（レビューを受けた変更だけをコミットする、など）は拡張に置きます。別の用途のハーネスは、実行ループに手を入れずに自分の tool と規則を加えられます。
- **crate は分けない。** エージェントとハーネスは同じ crate のモジュールとして分け、依存の向きはテストで守らせています。crate に分けるには、実行ループのテストが使っている OpenAI・MCP・セッションのアダプターをエージェント側へ含めるかテストダブルに置き換え、`ToolContext` が持つ検証コマンドの設定（`domain::environment::CheckConfig`）をエージェント側の型にする必要があります。エージェントだけを使う利用者が現れたときに見直します。
