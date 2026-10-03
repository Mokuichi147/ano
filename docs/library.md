# ライブラリとして使う

`ToolRegistry` に JSON Schema と async handler を登録し、`Agent` に渡します。動く例は [examples/embed.rs](../examples/embed.rs) にあります。

```rust
use ano::{register_builtin_tools, ToolDefinition, ToolRegistry};
use serde_json::json;

let registry = ToolRegistry::new();
register_builtin_tools(&registry)?; // echo・unix_time・workspace_*
registry.register(
    ToolDefinition::new(
        "lookup_customer",
        "Look up a customer by id.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false
        }),
    ),
    |arguments| async move {
        let id = arguments["id"].as_str().unwrap_or_default();
        Ok(json!({"id": id, "status": "example"}))
    },
)?;
```

- `Agent::with_event_listener` で tool 呼び出しなどのイベントを、`Agent::with_text_listener` で生成中の回答の差分を受け取れます。
- 実行環境（user・environment・workspace・書き込み許可）を受け取る tool は `register_contextual` で登録し、`ToolContext` から参照します。
- tool 名は Responses API の関数名規則に合わせて ASCII 英数字・`_`・`-` の64文字以内です。`tool_search`・`task_plan`・`delegate_task` と `mcp__` で始まる名前は予約されています。
- `register_builtin_tools` の `git_commit_push` は、レビューを受けた状態のファイルだけをコミットします。その照合と `review_changes` は拡張 `ReviewGate` が担うため、組み込みの tool を使う `Agent` には `.with_extension(Arc::new(ReviewGate::new()))` を付けてください（`Agent` ごとに1つ）。付けない場合、コミットは常に `review_required` で拒否されます。
- 影響の大きい tool は `ToolDefinition::new(...).with_approval()` で登録すると、呼び出しごとに `ApprovalHandler` へ確認します（`McpApprovalRequest::source` が `ApprovalSource::LocalTool` になります）。
- 直接接続の MCP を使う場合は `McpPool` を1つ作り、`Arc` で各 `Agent` に渡すと接続を共有できます。終了時に `shutdown().await` を呼んでください。
- `Agent` は外部依存をトレイト（`ResponsesApi`・`McpGateway`・`ConversationStore`・`ApprovalHandler`）で受け取るため、別の API クライアントや保存先に差し替えられます。構成は [docs/architecture.md](architecture.md) を参照してください。
- `Agent` の実行ループは特定の tool を前提としません。複数の tool にまたがる規則や、サブエージェントを起動するランタイム tool を加える場合は `AgentExtension` を実装して `with_extension` で渡します（`ReviewGate` が実装例です）。`ano` と同じ構成（組み込み tool・既定の instructions・AGENTS.md・スキル・承認モード・ロール）で組み立てる場合は `Harness` を使います。
