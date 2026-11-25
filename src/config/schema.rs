//! Every setting of the config file, as `ano web` shows it: its key, a label
//! and help in Japanese, and what kind of value it takes. The page builds its
//! form from [`SCHEMA`] and saves through [`super::save_settings`], which
//! writes only the keys named here.
//!
//! A test checks the keys against the settings types, so a key added to a
//! table that is missing here fails the build's tests.

use super::AppConfig;
use serde::Serialize;
use serde_json::{json, Value};

/// One key of a table.
#[derive(Debug, Serialize)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub help: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
}

/// What a key holds.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Bool,
    Integer,
    Text,
    /// Text of several lines.
    LongText,
    /// A list of strings, one per line in the form.
    Strings,
    /// One of `options`.
    Choice {
        options: &'static [&'static str],
    },
    /// A table of names to strings, `NAME=VALUE` per line in the form.
    Pairs,
    /// A table with keys of its own.
    Table {
        fields: &'static [Field],
    },
    /// Tables by name, such as `[providers.NAME]`.
    Named {
        fields: &'static [Field],
    },
    /// An array of tables, such as `[[mcp_servers]]`. `title` names the key
    /// that names an item in the form.
    List {
        fields: &'static [Field],
        title: &'static str,
    },
}

const fn field(key: &'static str, label: &'static str, help: &'static str, kind: Kind) -> Field {
    Field {
        key,
        label,
        help,
        kind,
    }
}

const REASONING_EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const APPROVAL_MODES: &[&str] = &["ask", "auto", "allow", "deny"];

const ROLES: &[Field] = &[
    field(
        "default",
        "メイン",
        "メインのエージェントのプリセット",
        Kind::Text,
    ),
    field(
        "delegate",
        "サブエージェント",
        "delegate_task のサブエージェントのプリセット",
        Kind::Text,
    ),
    field(
        "review",
        "レビュー",
        "review_changes のレビュー担当のプリセット",
        Kind::Text,
    ),
    field(
        "approval",
        "承認",
        "approval_mode = auto の判定役のプリセット（approval_model より優先）",
        Kind::Text,
    ),
    field(
        "triage",
        "振り分け",
        "自動化のイベントを振り分けるプリセット（省略時は承認と同じモデル）",
        Kind::Text,
    ),
];

const AGENT: &[Field] = &[
    field(
        "model",
        "モデル",
        "[api] で使うモデル。接続先が自分のモデルを持たないときにも使います",
        Kind::Text,
    ),
    field(
        "provider",
        "既定の接続先",
        "実行が使う接続先（[providers] の名前）。空欄は [api]",
        Kind::Text,
    ),
    field(
        "reasoning_effort",
        "推論の強さ",
        "推論モデルの強さ。使える値はモデルによります",
        Kind::Choice {
            options: REASONING_EFFORTS,
        },
    ),
    field(
        "reasoning_summary",
        "推論の要約",
        "進捗として表示する推論の要約",
        Kind::Choice {
            options: &["auto", "concise", "detailed"],
        },
    ),
    field(
        "approval_mode",
        "承認モード",
        "MCP の呼び出しとコマンド実行の承認の答え方（環境を使わない実行と ano web の既定）",
        Kind::Choice {
            options: APPROVAL_MODES,
        },
    ),
    field(
        "approval_model",
        "承認の判定モデル",
        "approval_mode = auto の判定に使うモデル。空欄は model",
        Kind::Text,
    ),
    field(
        "project_instructions",
        "プロジェクトの指示ファイル",
        "作業フォルダ直下にあれば指示に加えるファイル",
        Kind::Strings,
    ),
    field(
        "instructions",
        "指示",
        "空欄は組み込みの指示（計画→調査→編集→確認）。書くと置き換わります",
        Kind::LongText,
    ),
    field(
        "max_tool_rounds",
        "最大リクエスト数",
        "1回の実行のリクエスト数の上限（暴走の歯止め）",
        Kind::Integer,
    ),
    field(
        "tool_discovery_limit",
        "tool の検索数",
        "tool_search が一度に返す数",
        Kind::Integer,
    ),
    field("parallel_tool_calls", "tool の並列呼び出し", "", Kind::Bool),
    field(
        "max_parallel_tool_calls",
        "同時に動かす tool の数",
        "",
        Kind::Integer,
    ),
    field(
        "tool_timeout_secs",
        "tool のタイムアウト（秒）",
        "",
        Kind::Integer,
    ),
    field(
        "max_tool_output_bytes",
        "tool の出力の上限（バイト）",
        "超えた分は先頭と末尾を残して切ります",
        Kind::Integer,
    ),
    field(
        "max_output_tokens",
        "出力トークンの上限",
        "1回の応答の上限。空欄はモデルの既定",
        Kind::Integer,
    ),
    field(
        "max_total_tokens",
        "トークンの上限",
        "1回の実行の入出力トークンの目安の上限",
        Kind::Integer,
    ),
    field(
        "compaction",
        "履歴の圧縮方法",
        "remote は OpenAI の /responses/compact、summary はどの接続先でも使える要約",
        Kind::Choice {
            options: &["auto", "remote", "summary"],
        },
    ),
    field(
        "compact_threshold_bytes",
        "圧縮する履歴の大きさ（バイト）",
        "コンテキスト長に近づいたときのほか、この大きさでも圧縮します",
        Kind::Integer,
    ),
    field(
        "roles",
        "役割ごとのプリセット",
        "",
        Kind::Table { fields: ROLES },
    ),
];

/// The keys `[api]` and `[providers.NAME]` share.
macro_rules! connection_fields {
    ($($extra:expr),* $(,)?) => {
        &[
            $($extra,)*
            field("base_url", "URL", "Responses API（または Chat Completions）のエンドポイント。LM Studio なら http://127.0.0.1:1234/v1", Kind::Text),
            field("api_key_env", "API キーの環境変数", "API キーを入れた環境変数の名前（キーそのものは書きません）", Kind::Text),
            field("auth", "認証", "chatgpt は ChatGPT のサブスクリプション（先に ano auth login）", Kind::Choice { options: &["api_key", "chatgpt"] }),
            field("chatgpt_auth_file", "ChatGPT の認証情報の保存先", "", Kind::Text),
            field("wire_api", "API の種類", "Responses API の無いサーバーには chat_completions", Kind::Choice { options: &["responses", "chat_completions"] }),
            field("stream", "ストリーミング", "生成中の回答を表示します", Kind::Bool),
            field("timeout_secs", "タイムアウト（秒）", "", Kind::Integer),
            field("max_retries", "再試行の回数", "接続の失敗と 429・5xx の再試行", Kind::Integer),
            field("context_window", "コンテキスト長", "1回のリクエストのトークン数。LM Studio は自動で取得します", Kind::Integer),
            field("models", "モデルの候補", "モデルの一覧を返さない接続先で選べるモデル", Kind::Strings),
            field("allowed_models", "使ってよいモデル", "完全一致か末尾の * の前方一致。空欄は制限なし", Kind::Strings),
            field("disabled_models", "使わないモデル", "", Kind::Strings),
            field("fallback", "代わりの接続先", "つながらない・混んでいるときに順に試す接続先の名前", Kind::Strings),
        ]
    };
}

const API: &[Field] = connection_fields!();

const PROVIDER: &[Field] = connection_fields!(
    field(
        "enabled",
        "使う",
        "オフにすると設定を残したまま使えなくします",
        Kind::Bool
    ),
    field(
        "model",
        "モデル",
        "この接続先に切り替えたときのモデル",
        Kind::Text
    ),
    field(
        "approval_model",
        "承認の判定モデル",
        "この接続先での approval_mode = auto の判定モデル",
        Kind::Text
    ),
);

const PRESET: &[Field] = &[
    field("description", "説明", "", Kind::Text),
    field("provider", "接続先", "", Kind::Text),
    field("model", "モデル", "", Kind::Text),
    field(
        "reasoning_effort",
        "推論の強さ",
        "",
        Kind::Choice {
            options: REASONING_EFFORTS,
        },
    ),
];

const CHECK: &[Field] = &[
    field("program", "プログラム", "", Kind::Text),
    field("args", "引数", "", Kind::Strings),
    field("description", "説明", "", Kind::Text),
    field("timeout_secs", "タイムアウト（秒）", "", Kind::Integer),
];

const ENVIRONMENT: &[Field] = &[
    field("workspace", "作業フォルダ", "相対パスは設定ファイルのフォルダから。空欄は CLI の実行フォルダ", Kind::Text),
    field("preset", "プリセット", "", Kind::Text),
    field("provider", "接続先", "", Kind::Text),
    field("model", "モデル", "", Kind::Text),
    field("reasoning_effort", "推論の強さ", "", Kind::Choice { options: REASONING_EFFORTS }),
    field("allow_writes", "ファイルの書き込み", "", Kind::Bool),
    field("allow_exec", "コマンドの実行", "workspace_exec でシェルのコマンドを実行します（サンドボックスなし）", Kind::Bool),
    field("approval_mode", "承認モード", "環境の既定は deny（Webhook の作業には尋ねる相手がいないため）", Kind::Choice { options: APPROVAL_MODES }),
    field("auto_approve_mcp", "MCP を自動で承認", "", Kind::Bool),
    field("allowed_tools", "使ってよい tool", "空欄は制限なし", Kind::Strings),
    field("disabled_tools", "使わない tool", "", Kind::Strings),
    field("instructions", "追加の指示", "", Kind::LongText),
    field("checks", "確認コマンド", "workspace_check で実行できるコマンド。OS の権限でプロジェクトのコードを動かすので、信頼できる作業フォルダにだけ登録してください", Kind::Named { fields: CHECK }),
];

const TOOL_CATALOG: &[Field] = &[
    field("name", "名前", "", Kind::Text),
    field("description", "説明", "", Kind::Text),
];

const MCP_SERVER: &[Field] = &[
    field("label", "名前", "", Kind::Text),
    field(
        "transport",
        "接続方法",
        "responses は API 側が接続、stdio と streamable_http は ano が接続します",
        Kind::Choice {
            options: &["responses", "stdio", "streamable_http"],
        },
    ),
    field("url", "URL", "${VAR} で環境変数を埋め込めます", Kind::Text),
    field(
        "tunnel_id",
        "トンネル ID",
        "Secure MCP Tunnel を使うとき（url の代わり）",
        Kind::Text,
    ),
    field(
        "command",
        "コマンド",
        "stdio で起動するコマンド",
        Kind::Text,
    ),
    field("args", "引数", "", Kind::Strings),
    field("cwd", "作業フォルダ", "", Kind::Text),
    field(
        "env_vars",
        "環境変数",
        "子プロセスの変数名=ano 側の環境変数の名前",
        Kind::Pairs,
    ),
    field("description", "説明", "", Kind::Text),
    field(
        "authorization_env",
        "認証トークンの環境変数",
        "",
        Kind::Text,
    ),
    field(
        "oauth",
        "OAuth",
        "先に ano mcp login で認証します",
        Kind::Bool,
    ),
    field(
        "oauth_scopes",
        "OAuth のスコープ",
        "空欄はサーバーの案内どおり",
        Kind::Strings,
    ),
    field(
        "require_approval",
        "承認",
        "never は信頼できるサーバーだけに",
        Kind::Choice {
            options: &["always", "never"],
        },
    ),
    field(
        "allowed_tools",
        "使ってよい tool",
        "空欄は制限なし",
        Kind::Strings,
    ),
    field("disabled_tools", "使わない tool", "", Kind::Strings),
    field(
        "reuse_connection",
        "接続を使い回す",
        "状態を持つサーバーで作業ごとに分けるときはオフ",
        Kind::Bool,
    ),
    field(
        "tool_catalog",
        "tool の目録",
        "サーバーの一覧を毎回読まずに tool_search で見つけるための名前と説明",
        Kind::List {
            fields: TOOL_CATALOG,
            title: "name",
        },
    ),
];

const USER: &[Field] = &[
    field(
        "allowed_tools",
        "使ってよい tool",
        "空欄は制限なし。MCP は mcp:サーバー:tool の形",
        Kind::Strings,
    ),
    field("disabled_tools", "使わない tool", "", Kind::Strings),
];

const WEBHOOK: &[Field] = &[
    field("bind", "待ち受けるアドレス", "", Kind::Text),
    field("path", "パス", "", Kind::Text),
    field("secret_env", "署名の秘密の環境変数", "", Kind::Text),
    field(
        "allow_unauthenticated",
        "署名なしを受け付ける",
        "ループバックで待ち受けるときだけ有効",
        Kind::Bool,
    ),
    field(
        "signature_tolerance_secs",
        "署名の有効期間（秒）",
        "",
        Kind::Integer,
    ),
    field("max_body_bytes", "本文の上限（バイト）", "", Kind::Integer),
    field(
        "max_concurrent_jobs",
        "同時に動かすジョブ",
        "",
        Kind::Integer,
    ),
    field(
        "max_pending_jobs",
        "待たせるジョブの上限",
        "",
        Kind::Integer,
    ),
    field(
        "max_retained_jobs",
        "結果を残すジョブの数",
        "",
        Kind::Integer,
    ),
    field(
        "job_timeout_secs",
        "ジョブのタイムアウト（秒）",
        "",
        Kind::Integer,
    ),
];

const APPROVALS: &[Field] = &[
    field(
        "public_url",
        "公開 URL",
        "スマホからこのサーバーに届く URL。空欄なら承認を尋ねません",
        Kind::Text,
    ),
    field(
        "timeout_secs",
        "答えを待つ時間（秒）",
        "過ぎると拒否します",
        Kind::Integer,
    ),
];

const NOTIFIER: &[Field] = &[
    field(
        "kind",
        "種類",
        "",
        Kind::Choice {
            options: &["ntfy", "discord", "slack", "webhook"],
        },
    ),
    field("url", "URL", "", Kind::Text),
    field(
        "url_env",
        "URL の環境変数",
        "Discord・Slack の Incoming Webhook の URL はこちらで渡します",
        Kind::Text,
    ),
    field("token_env", "トークンの環境変数", "", Kind::Text),
    field(
        "secret_env",
        "署名の秘密の環境変数",
        "webhook の種類で使います",
        Kind::Text,
    ),
    field("timeout_secs", "タイムアウト（秒）", "", Kind::Integer),
];

const SOURCE: &[Field] = &[
    field("kind", "種類", "webhook は POST /events/<名前>、schedule は決まった時刻、routines は会話で作ったルーティン", Kind::Choice { options: &["webhook", "schedule", "routines"] }),
    field("trust", "信頼", "trusted は自分のサービスのイベント。種類ごとに既定があります", Kind::Choice { options: &["trusted", "untrusted"] }),
    field("cron", "cron", "schedule: 分 時 日 月 曜日（サーバーのローカル時刻）", Kind::Text),
    field("title", "題名", "schedule: イベントの題名", Kind::Text),
    field("body", "本文", "schedule: イベントの本文", Kind::LongText),
    field("secret_env", "署名の秘密の環境変数", "webhook: このソース専用の秘密（空欄はサーバーの秘密）", Kind::Text),
    field("signature_tolerance_secs", "署名の有効期間（秒）", "webhook", Kind::Integer),
    field("user", "ユーザー", "routines: ルーティンを読むユーザー", Kind::Text),
];

const RULE: &[Field] = &[
    field("name", "名前", "", Kind::Text),
    field(
        "sources",
        "ソース",
        "このルールに当てるソースの名前",
        Kind::Strings,
    ),
    field("kinds", "イベントの種類", "空欄はすべて", Kind::Strings),
    field("authors", "送り手", "空欄はすべて", Kind::Strings),
    field(
        "action",
        "動作",
        "auto は振り分け役が選び、ignore は無視、notify は通知、run は作業を実行",
        Kind::Choice {
            options: &["auto", "ignore", "notify", "run"],
        },
    ),
    field(
        "guidance",
        "判断の指針",
        "通知するかの判断に添える言葉",
        Kind::LongText,
    ),
    field(
        "task",
        "作業",
        "action = run で実行する依頼",
        Kind::LongText,
    ),
    field("goal", "完了条件", "", Kind::Text),
    field("environment", "環境", "作業を実行する環境", Kind::Text),
    field("user", "ユーザー", "", Kind::Text),
    field("notify", "通知先", "", Kind::Strings),
    field(
        "notify_on",
        "通知する結果",
        "例: completed, failed, timed_out",
        Kind::Strings,
    ),
    field(
        "notify_user",
        "notify_user を使わせる",
        "作業の中からエージェントが通知できるようにします",
        Kind::Bool,
    ),
];

const AUTOMATION: &[Field] = &[
    field(
        "state_file",
        "状態ファイル",
        "空欄は OS 標準のデータディレクトリ",
        Kind::Text,
    ),
    field(
        "approvals",
        "作業の承認",
        "作業の承認を、通知先へ承認ページのリンクで尋ねます",
        Kind::Table { fields: APPROVALS },
    ),
    field("notifiers", "通知先", "", Kind::Named { fields: NOTIFIER }),
    field(
        "sources",
        "ソース",
        "ano serve が受け取るイベントの出どころ",
        Kind::Named { fields: SOURCE },
    ),
    field(
        "rules",
        "ルール",
        "上から順に照合し、最初に一致したルールを使います",
        Kind::List {
            fields: RULE,
            title: "name",
        },
    ),
];

const HISTORY: &[Field] = &[
    field("enabled", "使う", "chronotope のサーバーが要るため、ほかの機能と違って既定は無効です。有効にすると記事の出来事も動きます", Kind::Bool),
    field("base_url", "chronotope の URL", "", Kind::Text),
    field("principal", "主体", "", Kind::Text),
    field("token_env", "トークンの環境変数", "", Kind::Text),
    field("timeout_secs", "タイムアウト（秒）", "", Kind::Integer),
    field("data_dir", "未送信の保存先", "空欄は OS 標準のデータディレクトリ", Kind::Text),
];

/// `enabled` and `dir`, which most features have.
macro_rules! feature {
    ($help:expr) => {
        &[
            field("enabled", "使う", $help, Kind::Bool),
            field(
                "dir",
                "保存先",
                "空欄は OS 標準のデータディレクトリ",
                Kind::Text,
            ),
        ]
    };
}

/// Every table of the config file, in the order the page shows them.
pub const SCHEMA: &[Field] = &[
    field("agent", "エージェント", "モデル・承認・実行の上限", Kind::Table { fields: AGENT }),
    field("api", "既定の接続先 [api]", "[providers] で名前を付けない接続先", Kind::Table { fields: API }),
    field("providers", "接続先", "名前を付けた接続先。--provider・/provider・プリセットで選びます", Kind::Named { fields: PROVIDER }),
    field("presets", "プリセット", "接続先・モデル・推論の強さの組", Kind::Named { fields: PRESET }),
    field("environments", "環境", "ano run --environment と Webhook の作業が使う権限と作業フォルダ", Kind::Named { fields: ENVIRONMENT }),
    field("mcp_servers", "MCP サーバー", "", Kind::List { fields: MCP_SERVER, title: "label" }),
    field("users", "ユーザー", "ユーザーごとに使える tool", Kind::Named { fields: USER }),
    field("history", "会話の履歴（chronotope）", "会話の原文を chronotope に保存し、過去の会話を調べられるようにします", Kind::Table { fields: HISTORY }),
    field("skills", "スキル", "", Kind::Table { fields: feature!("上手くいった手順をスキルとして保存し、以後の実行で参照します") }),
    field("character", "キャラクター", "", Kind::Table { fields: feature!("エージェントが名乗る名前・性格と口調を指示に加えます") }),
    field("memory", "記憶", "", Kind::Table { fields: feature!("会話で話した自分についての事実を記憶し、以後の実行に載せます（保存は承認が必要）") }),
    field("objectives", "目標", "", Kind::Table { fields: feature!("時間をかけて取り組む目標と進捗を記録します") }),
    field("routines", "ルーティン", "", Kind::Table { fields: feature!("会話で頼んだリマインダーや定期作業を保存します（実行するのは ano serve）") }),
    field("feed", "フィード", "", Kind::Table { fields: feature!("エージェントが残す提案・記事・報告の置き場です") }),
    field("spaces", "スペース", "", Kind::Table { fields: feature!("頼まれてエージェントが作る小さなページです") }),
    field("events", "記事の出来事", "", Kind::Table { fields: feature!("記事が告げる日付の決まった出来事を保存します。会話の履歴が無効の間は動きません") }),
    field("agenda", "予定の候補", "", Kind::Table { fields: &[field("enabled", "使う", "会話で話した予定をリマインダーの候補としてフィードに残します。フィードかルーティンが無効の間は動きません", Kind::Bool)] }),
    field("webhook", "Webhook（ano serve）", "", Kind::Table { fields: WEBHOOK }),
    field("automation", "自動化（ano serve）", "イベントを受けて無視・通知・作業のどれにするかを決めます", Kind::Table { fields: AUTOMATION }),
];

/// The value each key takes when the file leaves it out, in the shape of the
/// file. Entries of named tables and arrays of tables are under `*`.
pub fn defaults() -> Value {
    let config = AppConfig::default();
    let agent = &config.agent;
    let api = &config.api;
    let webhook = &config.webhook;
    let history = &config.history;
    let feature = |enabled: bool| json!({"enabled": enabled});
    json!({
        "agent": {
            "model": agent.settings.model,
            "approval_mode": agent.approval_mode,
            "project_instructions": agent.project_instructions,
            "max_tool_rounds": agent.settings.max_tool_rounds,
            "tool_discovery_limit": agent.settings.tool_discovery_limit,
            "parallel_tool_calls": agent.settings.parallel_tool_calls,
            "max_parallel_tool_calls": agent.settings.max_parallel_tool_calls,
            "tool_timeout_secs": agent.settings.tool_timeout_secs,
            "max_tool_output_bytes": agent.settings.max_tool_output_bytes,
            "compaction": agent.settings.compaction,
        },
        "api": {
            "base_url": api.base_url,
            "api_key_env": api.api_key_env,
            "auth": api.auth,
            "wire_api": api.wire_api,
            "stream": api.stream,
            "timeout_secs": api.timeout_secs,
            "max_retries": api.max_retries,
        },
        // stream, timeout_secs, and max_retries come from [api]; the page
        // shows the file's [api] values when it sets them.
        "providers": {"*": {
            "enabled": true,
            "auth": api.auth,
            "wire_api": api.wire_api,
            "stream": api.stream,
            "timeout_secs": api.timeout_secs,
            "max_retries": api.max_retries,
        }},
        "environments": {"*": {
            "allow_writes": false,
            "allow_exec": false,
            "auto_approve_mcp": false,
            "approval_mode": "deny",
            "checks": {"*": {"timeout_secs": 90}},
        }},
        "mcp_servers": {"*": {
            "transport": "responses",
            "require_approval": "always",
            "oauth": false,
            "reuse_connection": true,
        }},
        "history": {
            "enabled": history.enabled,
            "base_url": history.base_url,
            "principal": history.principal,
            "timeout_secs": history.timeout_secs,
        },
        "skills": feature(config.skills.enabled),
        "character": feature(config.character.enabled),
        "memory": feature(config.memory.enabled),
        "objectives": feature(config.objectives.enabled),
        "routines": feature(config.routines.enabled),
        "feed": feature(config.feed.enabled),
        "spaces": feature(config.spaces.enabled),
        // Their own defaults: they wait for what they need.
        "events": feature(crate::infrastructure::events::EventSettings::default().enabled),
        "agenda": feature(crate::infrastructure::agenda::AgendaSettings::default().enabled),
        "webhook": {
            "bind": webhook.bind,
            "path": webhook.path,
            "secret_env": webhook.secret_env,
            "allow_unauthenticated": webhook.allow_unauthenticated,
            "signature_tolerance_secs": webhook.signature_tolerance_secs,
            "max_body_bytes": webhook.max_body_bytes,
            "max_concurrent_jobs": webhook.max_concurrent_jobs,
            "max_pending_jobs": webhook.max_pending_jobs,
            "max_retained_jobs": webhook.max_retained_jobs,
            "job_timeout_secs": webhook.job_timeout_secs,
        },
        "automation": {
            "approvals": {"timeout_secs": 600},
            "notifiers": {"*": {"timeout_secs": 15}},
            "rules": {"*": {"action": "auto"}},
        },
    })
}
