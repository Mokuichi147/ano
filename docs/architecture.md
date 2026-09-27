# アーキテクチャ

ano はクリーンアーキテクチャに沿って4つの層に分かれています。依存は常に内側（`domain`）へ向かい、外部サービスやファイル・プロセスへのアクセスはアプリケーション層が定義するトレイト（ポート）を通して行います。

```text
┌──────────────────────────────┐   ┌──────────────────────────────┐
│ interface                    │   │ infrastructure               │
│  cli（composition root）     │   │  openai（HTTP クライアント）  │
│  webhook（axum サーバー）    │   │  mcp（rmcp 接続プール）       │
│                              │   │  session_store（JSON ファイル）│
│                              │   │  tools（workspace_*, checks） │
└──────────────┬───────────────┘   └──────────────┬───────────────┘
               │ 使う                             │ ポートを実装
               ▼                                  ▼
┌──────────────────────────────────────────────────────────────────┐
│ application                                                      │
│  agent（実行ループ）  ports（トレイト）  registry  profile  input │
└──────────────────────────────┬───────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│ domain                                                           │
│  plan  usage  policy  session  compaction  tool  mcp  environment│
└──────────────────────────────────────────────────────────────────┘
```

`config`（`src/config.rs`）は各層が持つ設定型を `config.toml` の1ファイルに束ねるだけのモジュールで、層の外側に置いています。

## 依存のルール

| 層 | 依存してよい先 | 置かないもの |
| --- | --- | --- |
| `domain` | 標準ライブラリ、serde、anyhow | ネットワーク・プロセス・非同期 I/O |
| `application` | `domain` | HTTP、MCP SDK、ファイル保存形式、端末入出力 |
| `infrastructure` | `application`、`domain` | CLI・Webhook の都合 |
| `interface` | すべての層 | 業務ルール |

`application` が外部と話す必要がある箇所は、すべて [`src/application/ports.rs`](../src/application/ports.rs) のトレイト経由です。

| ポート | 役割 | 実装 |
| --- | --- | --- |
| `ResponsesApi` | `/responses`（ストリーミングを含む）と `/responses/compact` の呼び出し | `infrastructure::openai::OpenAiClient` |
| `McpGateway` / `DirectMcpServer` | 直接接続 MCP の接続管理と tool 呼び出し | `infrastructure::mcp::McpPool` |
| `ConversationStore` | 会話の保持（変更ごとに保存） | `infrastructure::session_store::Session`（ファイル）、`infrastructure::memory_store::MemoryConversation`（メモリ） |
| `ApprovalHandler` | MCP 呼び出しと、承認が必要なローカル tool（`workspace_exec` など）の承認 | `AlwaysApprove`・`DenyApproval`・`AutoApproval`（application）、`InteractiveApproval`・対話モードの確認（interface/cli） |

テストや別の保存先・API を使う場合は、これらのトレイトを実装して `Agent` に渡せます。

例外として、`application::input` は `InputPart::Image(PathBuf)` / `InputPart::Audio(PathBuf)` のローカルファイルを読み込みます。入力の正規化と一体の処理のため、ポートには分けていません。

## ディレクトリ構成

```text
src/
├── lib.rs                  クレートのルート。よく使う型を再公開
├── main.rs                 バイナリ。interface::cli::run を呼ぶだけ
├── config.rs               config.toml の読み込み（各層の設定を集約）
├── domain/
│   ├── approval.rs         承認モード（ask・auto・allow・deny）
│   ├── plan.rs             作業計画と完了判定（RunOutcome）
│   ├── usage.rs            トークン使用量と停止理由
│   ├── policy.rs           ユーザー別の tool allowlist / denylist
│   ├── session.rs          会話の状態遷移（保留中の呼び出し、失敗時の補完）
│   ├── compaction.rs       履歴圧縮の判定、remote の結果の検証、summary の記録と置き換え
│   ├── tool.rs             ToolDefinition・ToolContext・予約名
│   ├── mcp.rs              MCP サーバー設定と公開可否の判定
│   └── environment.rs      実行環境と検証コマンドの設定
├── application/
│   ├── ports.rs            外部依存のトレイト
│   ├── agent/
│   │   ├── mod.rs          実行ループ（Agent::run / run_in_session）
│   │   ├── dispatch.rs     function call・MCP 呼び出し・承認・サブエージェントの実行
│   │   ├── discovery.rs    tool_search による遅延公開
│   │   ├── mcp_runtime.rs  1回の実行で使う MCP 接続とポリシー適用
│   │   ├── events.rs       AgentEvent と逐次通知
│   │   └── response.rs     Responses API 出力の解析
│   ├── registry.rs         ローカル tool の登録と実行
│   ├── profile.rs          実行環境から1回の実行設定を解決
│   ├── settings.rs         AgentSettings
│   ├── approval.rs         非対話の承認ポリシー
│   ├── auto_approval.rs    判定用モデルによる自動承認（ResponsesApi を利用）
│   └── input.rs            テキスト・画像・音声入力の組み立て
├── infrastructure/
│   ├── openai.rs           Responses API クライアント（リトライ・SSE ストリーミング）
│   ├── mcp.rs              stdio / Streamable HTTP の MCP 接続プール
│   ├── session_store.rs    セッションファイルとロック
│   ├── memory_store.rs     プロセス内だけで保持する会話（ano chat）
│   ├── project.rs          AGENTS.md などプロジェクト指示の読み込み
│   ├── fs.rs               原子的なファイル置き換え
│   └── tools/
│       ├── mod.rs          組み込み tool の登録（echo・unix_time）
│       ├── workspace.rs    list・read・search・find・edit・write とパスの検証
│       ├── manage.rs       move・delete
│       ├── walk.rs         上限付きのディレクトリ走査
│       ├── glob.rs         パスのグロブ照合
│       ├── checks.rs       workspace_check
│       ├── exec.rs         workspace_exec（シェルコマンド）
│       ├── web.rs          web_fetch（公開 Web ページの取得と Markdown 変換）
│       └── process.rs      子プロセスの実行（期限・出力上限・プロセスグループの停止）
└── interface/
    ├── cli/                clap による CLI（run・chat・tools・session・serve）、進捗表示、端末での承認
    └── webhook/            署名付き Webhook、ジョブ管理
```

## 1回の実行の流れ

1. `interface`（CLI または Webhook）が設定から `ExecutionProfile`（モデル設定・有効なポリシー・`ToolContext`）を解決し、workspace の `AGENTS.md` を instructions に加え、アダプターを組み立てて `Agent` を作ります。
2. `Agent::run` は入力を Responses API の `input` に変換し、`McpGateway` からポリシーで許可された MCP 接続を借ります。
3. 各ラウンドで `ResponsesApi::create_response`（テキストのリスナーがあれば `create_response_streaming`）を呼び、返ってきた `function_call`・`mcp_approval_request` を `dispatch` が並行実行します。セッションがあれば、実行前に呼び出しを、完了するたびに結果を `ConversationStore` へ保存します。
4. `tool_search` の結果は次のラウンドから tool 一覧に反映されます（`discovery`）。`delegate_task` は同じ `Agent` の実行ループを新しい会話で1段だけ再帰的に動かし、最終回答を tool 出力として返します。
5. 最終回答で未完了の計画工程が残っていれば継続を促し、完了・中断・上限到達のいずれかで `AgentResult` を返します。

## 設計上の判断

- **会話履歴は Responses API の item 形式のまま扱う。** 独自の中間表現へ変換せず、`domain::session` や `domain::compaction` も同じ JSON を扱います。暗号化された reasoning や圧縮 item をそのまま再送する必要があるためです。
- **セッションの状態遷移と保存を分ける。** 状態遷移（`domain::session::SessionData`）はメモリ上だけで完結し、保存・ロック・アーカイブは `Session` が担当します。別の保存先を使う場合も同じ遷移規則を再利用できます。
- **CLI が composition root を兼ねる。** バイナリのエントリポイントで具象アダプター（`OpenAiClient`・`McpPool`・`Session`）を組み立てます。Webhook サーバーはクライアントと MCP ゲートウェイを受け取るだけで、自分では作りません。
