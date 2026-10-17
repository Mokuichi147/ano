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
- 自動化（[docs/automation.md](automation.md)）のソースと通知先は、`kind` の名前で `AutomationRegistry` に登録した種類から作られます。独自の種類を加えるには、[独自のソースと通知先](#独自のソースと通知先)を参照してください。
- `Agent` の実行ループは特定の tool を前提としません。複数の tool にまたがる規則や、サブエージェントを起動するランタイム tool を加える場合は `AgentExtension` を実装して `with_extension` で渡します（`ReviewGate` が実装例です）。`ano` と同じ構成（組み込み tool・既定の instructions・AGENTS.md・スキル・承認モード・ロール）で組み立てる場合は `Harness` を使います。

## 独自のソースと通知先

自動化の入力元（ソース）は `EventSource`、通知先は `Notifier` を実装し、`kind` の名前で `AutomationRegistry` に登録します。設定ファイルの `[automation.sources.<name>]`・`[automation.notifiers.<name>]` で `kind` にその名前を書くと、`kind` と `trust` 以外の項目が JSON として `KindSpec::options` に渡ります。設定の解釈と検証は、種類ごとに自分で行います。組み込みの `webhook`・`ntfy`・`discord`・`slack` も、同じ仕組みで `register_builtin_automation` が登録しています（`src/infrastructure/automation/`）。

ソースは `POST /events/<name>` に送られた要求を、自分の形式で読んでイベントにします。次の例は、監視サービスが独自の形式とトークンで送ってくる Webhook を、そのまま受け取る種類です。

```rust
use ano::{
    register_builtin_automation, AutomationEvent, AutomationRegistry, EventSource,
    PostAuthentication, PostedRequest, Rejection, Trust,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

/// 監視サービスの通知（`{"id": "...", "status": "down", "monitor": "api"}`）。
struct Monitoring {
    token: String,
}

#[derive(Deserialize)]
struct Alert {
    id: String,
    status: String,
    monitor: String,
}

#[async_trait]
impl EventSource for Monitoring {
    fn trust(&self) -> Trust {
        Trust::Trusted // 自分の監視サービスの値なので信頼できる入力
    }

    fn describe(&self) -> String {
        "alerts of the monitoring service".into()
    }

    // 送り手は ano の署名を作れないので、ソースが自分で確かめる。
    fn authentication(&self) -> PostAuthentication {
        PostAuthentication::Source
    }

    fn receive(&self, request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
        if request.header("X-Monitoring-Token") != Some(self.token.as_str()) {
            return Err(Rejection::Unauthorized("wrong token".into()));
        }
        let alert: Alert = serde_json::from_slice(request.body).map_err(Rejection::invalid)?;
        Ok(AutomationEvent {
            source: request.source.to_string(),
            kind: format!("alert_{}", alert.status),
            // 同じ通知の再送は同じ key になり、重複として除かれる。
            key: alert.id,
            title: format!("{} is {}", alert.monitor, alert.status),
            body: String::new(),
            url: None,
            author: None,
            trust: Trust::Trusted,
        })
    }
}

let mut automation = AutomationRegistry::new();
register_builtin_automation(&mut automation);
automation.register_source("monitoring", |spec| {
    let variable = spec.options["token_env"].as_str().unwrap_or("MONITORING_TOKEN");
    Ok(Arc::new(Monitoring {
        token: std::env::var(variable)?,
    }))
});
// ano::serve_webhook(config, client, mcp, registry, automation).await?;
```

```toml
[automation.sources.monitoring]
kind = "monitoring"
token_env = "MONITORING_TOKEN"
```

| `EventSource` のメソッド | 内容 |
| --- | --- |
| `trust` | イベントの既定の信頼度。`untrusted` のソースに一致して作業を始めうるルールは、`approval_mode = "allow"` の環境を指定できません |
| `describe` | `ano automation list` に出す1行の説明 |
| `authentication` | 要求の認証の担い手。既定の `PostAuthentication::Server` では、サーバーが ano の署名を検証し、リプレイを拒否してから渡します。`PostAuthentication::Source` では、検証はソースが `receive` で行います（GitHub・Slack・Stripe など、送り手が独自の方式で署名するサービス向け）。この場合、サーバーは何も検証しません。署名と時刻を確かめ、同じ要求から同じ `key` を作って重複として除くなど、認証とリプレイの拒否はソースの役目です |
| `receive` | POST された要求（`PostedRequest`: ソース名・ヘッダー・本文）をイベントにします。認証できなければ `Rejection::Unauthorized`（`401`）、読めなければ `Rejection::Invalid`（`400`）を返します。理由は 1,000 文字までに切って返します |

- `receive` が返したイベントの `source` は、受け取ったソースの名前に置き換えられます。
- どの種類のイベントも、共通の検証（`kind`・`key`・`title` が空でないこと、長さの上限）を受け、満たさなければ `400` になります。
- 同じ `key` のイベントは、2回目以降は処理せず `200` と `"duplicate": true` を返します。
- イベントの `trust` は種類が決めます。`trust()` が `untrusted` の種類でも、一部のイベントを信頼できる入力にできますが、第三者の文面を `trusted` にしないでください。
- 通知先は `Notifier::send` を実装します。`NamedNotifier` の名前は、ログと `notify_user` の結果に使われます。
