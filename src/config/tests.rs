use super::{
    edit::{with_mcp_tool_filters, with_provider_changes, with_provider_renamed},
    expand_home, remove_provider, save_mcp_tool_filters, update_provider, AppConfig, ModelRequest,
    SettingValue,
};
use std::path::Path;

const PROVIDERS: &str = "[agent]\nmodel = 'gpt-main'\napproval_model = 'gpt-mini'\n[api]\ntimeout_secs = 30\n[providers.local]\nbase_url = 'http://192.168.1.10:1234/v1'\nmodel = 'qwen/qwen3'\n[providers.bare]\nbase_url = 'http://127.0.0.1:8000/v1'\ntimeout_secs = 5\n";

fn request(provider: Option<&str>, model: Option<&str>) -> ModelRequest {
    ModelRequest {
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        ..ModelRequest::default()
    }
}

#[test]
fn a_provider_brings_its_model_and_later_layers_override_it() {
    let config = AppConfig::parse(PROVIDERS).unwrap();
    let chosen = |requests: &[ModelRequest]| {
        let selection = config.select_model(requests).unwrap();
        (
            selection.choice.provider,
            selection.choice.model,
            selection.approval_model,
        )
    };
    let owned = |p: &str, m: &str, a: Option<&str>| (p.into(), m.into(), a.map(Into::into));
    assert_eq!(chosen(&[]), owned("api", "gpt-main", Some("gpt-mini")));
    // The reviewer of [agent] belongs to [api]; another provider reviews
    // with its own model unless it names one.
    assert_eq!(
        chosen(&[request(Some("local"), None)]),
        owned("local", "qwen/qwen3", None)
    );
    assert_eq!(
        chosen(&[
            request(Some("local"), None),
            request(None, Some("qwen3:30b"))
        ]),
        owned("local", "qwen3:30b", None)
    );
    // A provider without a model keeps the model of lower layers.
    assert_eq!(
        chosen(&[
            request(None, Some("env-model")),
            request(Some("bare"), None)
        ]),
        owned("bare", "env-model", None)
    );
    assert_eq!(
        chosen(&[request(Some("local"), None), request(Some("api"), None)]),
        owned("api", "gpt-main", Some("gpt-mini"))
    );
    assert!(config
        .select_model(&[request(Some("missing"), None)])
        .is_err());
    assert!(config.select_model(&[request(None, Some(" "))]).is_err());
}

const PRESETS: &str = "[agent]\nmodel = 'gpt-main'\nreasoning_effort = 'medium'\n[providers.lan]\nbase_url = 'http://127.0.0.1:9/v1'\nmodel = 'qwen'\n[presets.quick]\nprovider = 'lan'\nreasoning_effort = 'low'\n[presets.deep]\nmodel = 'gpt-big'\nreasoning_effort = 'high'\ndescription = 'Hard problems'\n[presets.lighter]\nreasoning_effort = 'minimal'\n[environments.dev]\npreset = 'quick'\nmodel = 'qwen-small'\n";

fn choice(selection: super::ModelSelection) -> (String, String, Option<String>) {
    let choice = selection.choice;
    (choice.provider, choice.model, choice.reasoning_effort)
}

fn owned(provider: &str, model: &str, effort: &str) -> (String, String, Option<String>) {
    (provider.into(), model.into(), Some(effort.into()))
}

#[test]
fn presets_apply_their_provider_model_and_effort_over_lower_layers() {
    let config = AppConfig::parse(PRESETS).unwrap();
    let chosen = |requests: &[ModelRequest]| choice(config.select_model(requests).unwrap());
    let preset = ModelRequest::preset;
    assert_eq!(chosen(&[]), owned("api", "gpt-main", "medium"));
    // A provider brings its model; the effort is the preset's.
    assert_eq!(chosen(&[preset("quick")]), owned("lan", "qwen", "low"));
    // A preset without a provider keeps the provider under it.
    assert_eq!(
        chosen(&[preset("quick"), preset("deep")]),
        owned("lan", "gpt-big", "high")
    );
    // One that sets only the effort keeps the model.
    assert_eq!(
        chosen(&[preset("quick"), preset("lighter")]),
        owned("lan", "qwen", "minimal")
    );
    // The fields of a layer apply over its preset.
    let with_effort = ModelRequest {
        reasoning_effort: Some("high".into()),
        ..preset("quick")
    };
    assert_eq!(chosen(&[with_effort]), owned("lan", "qwen", "high"));
    assert_eq!(
        chosen(&[config.environment_request("dev").unwrap()]),
        owned("lan", "qwen-small", "low")
    );
    // `default` adds nothing to the layers under it.
    assert_eq!(
        chosen(&[preset("quick"), preset("default")]),
        owned("lan", "qwen", "low")
    );
    assert!(config.select_model(&[preset("missing")]).is_err());
    let invalid = ModelRequest {
        reasoning_effort: Some("hight".into()),
        ..ModelRequest::default()
    };
    assert!(config.select_model(&[invalid]).is_err());
}

#[test]
fn the_default_preset_and_the_presets_of_roles() {
    let text = format!("{PRESETS}[agent.roles]\ndelegate = 'lighter'\nreview = 'default'\n")
        .replace(
            "delegate = 'lighter'",
            "default = 'quick'\ndelegate = 'lighter'",
        );
    let config = AppConfig::parse(&text).unwrap();
    assert_eq!(config.default_provider(), "lan");
    assert_eq!(config.listed_provider_names().collect::<Vec<_>>(), ["lan"]);
    let default = config.select_model(&[]).unwrap();
    assert_eq!(choice(default.clone()), owned("lan", "qwen", "low"));

    // A role applies its preset over the main agent's current choice...
    let current = config
        .select_model(&[ModelRequest::preset("deep")])
        .unwrap()
        .choice;
    let role = |preset: &str| choice(config.select_role(&[], &current, preset).unwrap());
    assert_eq!(role("lighter"), owned("lan", "gpt-big", "minimal"));
    // ...and `default` returns to the configured choice.
    assert_eq!(role("default"), owned("lan", "qwen", "low"));
    let base = [config.environment_request("dev").unwrap()];
    assert_eq!(
        choice(config.select_role(&base, &current, "default").unwrap()),
        owned("lan", "qwen-small", "low")
    );
}

#[test]
fn presets_and_their_references_are_validated() {
    for text in [
        "[presets.default]\nmodel = 'x'",
        "[presets.'a b']\nmodel = 'x'",
        "[presets.empty]\ndescription = 'nothing'",
        "[presets.far]\nprovider = 'missing'",
        "[presets.blank]\nmodel = ' '",
        "[presets.hard]\nreasoning_effort = 'hight'",
        "[presets.odd]\nmodel = 'x'\nunknown = 1",
        "[agent]\npreset = 'quick'",
        "[agent.roles]\ndefault = 'missing'",
        "[agent.roles]\nreview = 'missing'",
        "[agent.roles]\nplanner = 'default'",
        "[environments.dev]\npreset = 'missing'",
        "[environments.dev]\nreasoning_effort = 'hight'",
    ] {
        assert!(AppConfig::parse(text).is_err(), "{text}");
    }
    assert!(AppConfig::parse("[agent.roles]\napproval = 'default'").is_ok());
    // Codex models on a ChatGPT subscription go beyond xhigh.
    for effort in ["max", "ultra"] {
        let text = format!(
            "[agent]\nreasoning_effort = '{effort}'\n[presets.top]\nreasoning_effort = '{effort}'"
        );
        assert!(AppConfig::parse(&text).is_ok(), "{effort}");
    }
    assert!(AppConfig::parse(PRESETS).is_ok());
}

#[test]
fn renaming_a_provider_updates_the_presets_that_use_it() {
    let renamed = with_provider_renamed(PRESETS, "lan", "desktop").unwrap();
    let config = AppConfig::parse(&renamed).unwrap();
    assert_eq!(config.presets["quick"].provider.as_deref(), Some("desktop"));
    assert!(remove_provider_text(&renamed, "desktop").is_err());
}

/// Whether removing the provider `name` from `text` leaves a valid config.
fn remove_provider_text(text: &str, name: &str) -> anyhow::Result<AppConfig> {
    let mut document: toml_edit::DocumentMut = text.parse()?;
    document["providers"]
        .as_table_like_mut()
        .unwrap()
        .remove(name);
    AppConfig::parse(&document.to_string())
}

#[test]
fn providers_inherit_transport_settings_but_not_the_endpoint() {
    let config = AppConfig::parse(PROVIDERS).unwrap();
    let local = config.provider_settings("local").unwrap();
    assert_eq!(local.base_url, "http://192.168.1.10:1234/v1");
    assert_eq!(local.timeout_secs, 30);
    assert_eq!(local.api_key_env, "OPENAI_API_KEY");
    // OPENAI_BASE_URL only redirects [api].
    assert!(!local.use_base_url_env);
    assert!(config.provider_settings("api").unwrap().use_base_url_env);
    assert_eq!(config.provider_settings("bare").unwrap().timeout_secs, 5);
    assert_eq!(
        config.provider_names().collect::<Vec<_>>(),
        ["api", "bare", "local"]
    );
}

#[test]
fn provider_names_and_references_are_validated() {
    for text in [
        "[providers.api]\nbase_url = 'http://127.0.0.1:1/v1'",
        "[agent]\nprovider = 'missing'",
        "[providers.'a b']\nbase_url = 'http://127.0.0.1:1/v1'",
        "[providers.local]\nmodel = ''",
        "[providers.local]\ntimeout_secs = 0",
        "[providers.local]\nunknown = 1",
        "[environments.dev]\nprovider = 'missing'",
        "[environments.dev]\nprovider = ''",
    ] {
        assert!(AppConfig::parse(text).is_err(), "{text}");
    }
    assert!(AppConfig::parse("[providers.local]\n[environments.dev]\nprovider = 'local'").is_ok());
    assert!(AppConfig::parse("[environments.dev]\nprovider = 'api'").is_ok());
}

#[test]
fn saves_mcp_tool_filters_keeping_comments_and_other_servers() {
    let text = "# servers\n[[mcp_servers]]\nlabel = 'docs'\ntransport = 'stdio'\ncommand = 'node'\n\n[[mcp_servers]]\n# keep this\nlabel = 'files'\ntransport = 'stdio'\ncommand = 'node'\nallowed_tools = ['read', 'write'] # trusted\n\n[users.alice]\ndisabled_tools = ['files:write']\n";
    let mut config = AppConfig::parse(text).unwrap();
    let files = &mut config.mcp_servers[1];
    files.set_tools_enabled(["write"], false);
    files.disabled_tools.push("delete".into());

    let updated = with_mcp_tool_filters(text, files).unwrap();
    assert_eq!(
        updated,
        text.replace(
            "allowed_tools = ['read', 'write'] # trusted\n",
            "allowed_tools = [\"read\"] # trusted\ndisabled_tools = [\"delete\"]\n"
        )
    );

    files.disabled_tools.clear();
    let restored = with_mcp_tool_filters(&updated, files).unwrap();
    assert!(!restored.contains("disabled_tools = [\""));
    assert!(restored.contains("[users.alice]\ndisabled_tools = ['files:write']"));
}

#[test]
fn saves_mcp_tool_filters_of_inline_tables() {
    let text = "mcp_servers = [{ label = 'files', transport = 'stdio', command = 'node' }]\n";
    let mut config = AppConfig::parse(text).unwrap();
    config.mcp_servers[0].set_tools_enabled(["delete"], false);
    let updated = with_mcp_tool_filters(text, &config.mcp_servers[0]).unwrap();
    let reloaded = AppConfig::parse(&updated).unwrap();
    assert_eq!(reloaded.mcp_servers[0].disabled_tools, ["delete"]);
}

#[test]
fn saving_mcp_tool_filters_needs_the_server_in_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let text = "[[mcp_servers]]\nlabel = 'files'\ntransport = 'stdio'\ncommand = 'node'\n";
    std::fs::write(&path, text).unwrap();
    let mut server = AppConfig::parse(text).unwrap().mcp_servers.remove(0);
    server.set_tools_enabled(["delete"], false);
    save_mcp_tool_filters(&path, &server).unwrap();
    let saved = AppConfig::load(&path).unwrap();
    assert_eq!(saved.mcp_servers[0].disabled_tools, ["delete"]);

    server.label = "missing".into();
    assert!(save_mcp_tool_filters(&path, &server).is_err());
}

#[test]
fn default_config_is_valid() {
    let config = AppConfig::default();
    config.validate().unwrap();
    assert_eq!(config.agent.settings.max_tool_rounds, 100);
}

#[test]
fn example_config_parses_and_validates() {
    let config = AppConfig::parse(include_str!("../../config.example.toml")).unwrap();
    assert!(config.environments.contains_key("default"));
}

#[test]
fn rejects_misspelled_policy_fields() {
    let error = AppConfig::parse("[users.alice]\ndisable_tools = [\"echo\"]\n").unwrap_err();
    assert!(format!("{error:#}").contains("disable_tools"));
}

#[test]
fn rejects_invalid_approval_mode() {
    let text = "[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://x.test\"\nrequire_approval = \"sometimes\"\n";
    assert!(AppConfig::parse(text).is_err());
}

#[test]
fn rejects_duplicate_or_ambiguous_mcp_labels() {
    let duplicate = "[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://x.test\"\n[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://y.test\"\n";
    assert!(AppConfig::parse(duplicate).is_err());

    let colon = "[[mcp_servers]]\nlabel = \"a:b\"\nurl = \"https://x.test\"\n";
    assert!(AppConfig::parse(colon).is_err());
}

#[test]
fn oauth_requires_streamable_http_without_a_static_token() {
    let server = |extra: &str| {
        format!("[[mcp_servers]]\nlabel = 'a'\ntransport = 'streamable_http'\nurl = 'https://x.test/mcp'\n{extra}")
    };
    assert!(AppConfig::parse(&server("oauth = true\noauth_scopes = ['read']")).is_ok());
    for text in [
        server("oauth = true\nauthorization_env = 'TOKEN'"),
        server("oauth_scopes = ['read']"),
        "[[mcp_servers]]\nlabel = 'a'\nurl = 'https://x.test/mcp'\noauth = true".into(),
    ] {
        assert!(AppConfig::parse(&text).is_err(), "accepted {text}");
    }
}

#[test]
fn missing_explicit_config_file_is_an_error() {
    assert!(AppConfig::load("definitely-missing-ano-config.toml").is_err());
    assert!(AppConfig::load_or_default("definitely-missing-ano-config.toml").is_ok());
}

#[test]
fn resolves_environment_paths_relative_to_config_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[environments.project]\nworkspace = 'repo'\n[[mcp_servers]]\nlabel = 'local'\ntransport = 'stdio'\ncommand = 'node'\ncwd = 'servers'\n").unwrap();
    let config = AppConfig::load(&path).unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    assert_eq!(
        config.environments["project"].workspace,
        Some(root.join("repo"))
    );
    assert_eq!(config.mcp_servers[0].cwd, Some(root.join("servers")));
}

#[test]
fn expands_home_in_workspace_cwd_and_command() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = AppConfig::parse("[environments.home]\nworkspace = '~'\n[environments.project]\nworkspace = '~/repo'\n[environments.literal]\nworkspace = '~user/repo'\n[[mcp_servers]]\nlabel = 'local'\ntransport = 'stdio'\ncommand = '~/bin/server'\ncwd = '~/servers'\n[[mcp_servers]]\nlabel = 'path'\ntransport = 'stdio'\ncommand = 'node'\n").unwrap();
    let home = Path::new("/home/alice");
    config.resolve_paths(directory.path(), Some(home)).unwrap();
    let workspace = |name: &str| config.environments[name].workspace.clone().unwrap();
    assert_eq!(workspace("home"), home);
    assert_eq!(workspace("project"), home.join("repo"));
    assert_eq!(workspace("literal"), directory.path().join("~user/repo"));
    assert_eq!(config.mcp_servers[0].cwd, Some(home.join("servers")));
    assert_eq!(
        config.mcp_servers[0].command.as_deref().map(Path::new),
        Some(home.join("bin/server").as_path())
    );
    assert_eq!(config.mcp_servers[1].command.as_deref(), Some("node"));
}

#[test]
fn unknown_home_is_an_error_only_when_needed() {
    assert!(expand_home(Path::new("~/servers"), None).is_err());
    assert_eq!(expand_home(Path::new("servers"), None).unwrap(), None);
}

#[test]
fn rejects_empty_models_and_zero_budgets() {
    for text in [
        "[agent]\nmodel = '  '",
        "[agent]\nmax_output_tokens = 0",
        "[agent]\nmax_total_tokens = 0",
        "[agent]\ncompact_threshold_bytes = 1023",
        "[agent]\ncompact_threshold_bytes = 16777217",
        "[api]\ntimeout_secs = 0",
        "[webhook]\njob_timeout_secs = 0",
        "[environments.project]\nmodel = ''",
    ] {
        assert!(AppConfig::parse(text).is_err(), "accepted {text}");
    }
}

#[test]
fn rejects_webhook_paths_that_conflict_with_management_routes() {
    for path in [
        "/jobs",
        "/jobs/task/cancel",
        "/healthz",
        "/tasks/{id}",
        "/:id",
        "/tasks?q=1",
    ] {
        let text = format!("[webhook]\npath = '{path}'");
        assert!(AppConfig::parse(&text).is_err(), "accepted {path}");
    }
    assert!(AppConfig::parse("[webhook]\npath = '/hooks/tasks'").is_ok());
}

#[test]
fn disabled_providers_and_models_cannot_be_selected() {
    let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[api]\ndisabled_models = ['gpt-old*']\n[providers.lan]\nenabled = false\nmodel = 'qwen'\n[providers.box]\nallowed_models = ['llama*']\nmodel = 'llama3'").unwrap();
    let error = config
        .select_model(&[request(Some("lan"), None)])
        .unwrap_err();
    assert!(
        error.to_string().contains("ano provider enable lan"),
        "{error}"
    );
    let error = config
        .select_model(&[request(None, Some("gpt-old-1"))])
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ano model enable gpt-old-1 --provider api"),
        "{error}"
    );
    assert!(config
        .select_model(&[request(Some("box"), Some("qwen"))])
        .is_err());
    assert_eq!(
        config
            .select_model(&[request(Some("box"), None)])
            .unwrap()
            .choice
            .model,
        "llama3"
    );
    assert!(!config.provider_enabled("lan"));
    assert!(config.provider_enabled("api"));
}

#[test]
fn fallbacks_must_name_other_known_providers() {
    assert!(
        AppConfig::parse("[api]\nfallback = ['lan']\n[providers.lan]\nfallback = ['api']").is_ok()
    );
    for text in [
        "[api]\nfallback = ['missing']",
        "[providers.lan]\nfallback = ['lan']",
        "[providers.lan]\ndisabled_models = ['']",
        "[providers.lan]\nallowed_models = ['qwen/*-4b']",
    ] {
        assert!(AppConfig::parse(text).is_err(), "{text}");
    }
}

#[test]
fn provider_changes_keep_the_rest_of_the_file() {
    let text = "# main endpoint\n[api]\nbase_url = 'http://127.0.0.1:1234/v1' # LM Studio\n\n[agent]\nmodel = 'qwen'\n";
    let text_value = |value: &str| Some(SettingValue::Text(value.into()));
    let added = with_provider_changes(
        text,
        "lan",
        true,
        &[
            ("base_url", text_value("http://192.168.1.10:1234/v1")),
            ("fallback", Some(SettingValue::List(vec!["api".into()]))),
        ],
    )
    .unwrap();
    assert!(added.starts_with(text), "{added}");
    assert!(
        added.contains(
            "[providers.lan]\nbase_url = \"http://192.168.1.10:1234/v1\"\nfallback = [\"api\"]"
        ),
        "{added}"
    );
    let config = AppConfig::parse(&added).unwrap();
    assert_eq!(config.providers["lan"].fallback, ["api"]);
    assert!(with_provider_changes(&added, "lan", true, &[]).is_err());
    assert!(with_provider_changes(text, "missing", false, &[]).is_err());

    let changed = with_provider_changes(
        &added,
        "lan",
        false,
        &[
            ("fallback", None),
            ("enabled", Some(SettingValue::Bool(false))),
        ],
    )
    .unwrap();
    let config = AppConfig::parse(&changed).unwrap();
    assert!(config.providers["lan"].fallback.is_empty());
    assert!(!config.provider_enabled("lan"));

    // `default` is [api], and its model is [agent].model.
    let default = with_provider_changes(
        text,
        "api",
        false,
        &[
            ("base_url", text_value("http://127.0.0.1:11434/v1")),
            ("model", text_value("llama3")),
        ],
    )
    .unwrap();
    assert!(
        default.contains("base_url = \"http://127.0.0.1:11434/v1\" # LM Studio"),
        "{default}"
    );
    assert!(default.contains("[agent]\nmodel = \"llama3\""), "{default}");
    assert!(with_provider_changes(text, "api", false, &[("enabled", None)]).is_err());
    assert!(with_provider_changes(text, "api", true, &[]).is_err());
}

#[test]
fn provider_files_are_created_and_references_block_removal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    update_provider(
        &path,
        "lan",
        true,
        &[("model", Some(SettingValue::Text("qwen".into())))],
    )
    .unwrap();
    let config = AppConfig::load(&path).unwrap();
    assert_eq!(config.provider_model("lan"), Some("qwen"));
    // An invalid result is never written.
    assert!(update_provider(
        &path,
        "lan",
        false,
        &[("timeout_secs", Some(SettingValue::Integer(0)))]
    )
    .is_err());
    update_provider(
        &path,
        "api",
        false,
        &[("fallback", Some(SettingValue::List(vec!["lan".into()])))],
    )
    .unwrap();
    let error = remove_provider(&path, "lan").unwrap_err();
    assert!(format!("{error:#}").contains("fallback"), "{error:#}");
    update_provider(&path, "api", false, &[("fallback", None)]).unwrap();
    remove_provider(&path, "lan").unwrap();
    assert!(AppConfig::load(&path).unwrap().providers.is_empty());
    assert!(remove_provider(&path, "lan").is_err());
}

#[test]
fn the_default_provider_brings_its_model_and_hides_an_unused_api() {
    let text = "[agent]\nmodel = 'gpt-main'\nprovider = 'lan'\n[providers.lan]\nmodel = 'qwen'\napproval_model = 'qwen-mini'\n[providers.bare]\n";
    let config = AppConfig::parse(text).unwrap();
    assert_eq!(config.default_provider(), "lan");
    let selection = config.select_model(&[]).unwrap();
    assert_eq!(selection.choice.provider, "lan");
    assert_eq!(selection.choice.model, "qwen");
    assert_eq!(selection.approval_model.as_deref(), Some("qwen-mini"));
    // A default provider without a model uses [agent].model.
    let bare = AppConfig::parse(&text.replace("provider = 'lan'", "provider = 'bare'")).unwrap();
    assert_eq!(bare.select_model(&[]).unwrap().choice.model, "gpt-main");

    assert_eq!(
        config.listed_provider_names().collect::<Vec<_>>(),
        ["bare", "lan"]
    );
    // `api` is still selectable, and listed while something uses it.
    assert_eq!(
        config
            .select_model(&[request(Some("api"), None)])
            .unwrap()
            .choice
            .model,
        "gpt-main"
    );
    let referenced = AppConfig::parse(&format!("{text}fallback = ['api']")).unwrap();
    assert_eq!(
        referenced.listed_provider_names().collect::<Vec<_>>(),
        ["api", "bare", "lan"]
    );
    assert_eq!(
        AppConfig::default()
            .listed_provider_names()
            .collect::<Vec<_>>(),
        ["api"]
    );
}

#[test]
fn renaming_api_moves_its_connection_to_a_named_provider() {
    let text = "[api]\n# the subscription\nauth = 'chatgpt' # subscription\nmodels = ['gpt-5.6-luna']\ntimeout_secs = 30\n\n[agent]\nmodel = 'gpt-5.6-luna'\napproval_model = 'gpt-5.6-mini'\n\n[providers.lan]\nbase_url = 'http://192.168.1.10:1234/v1'\nfallback = ['api']\n\n[environments.review]\nprovider = 'api'\n";
    let renamed = with_provider_renamed(text, "api", "chatgpt").unwrap();
    let config = AppConfig::parse(&renamed).unwrap();
    assert!(
        renamed.contains("auth = 'chatgpt' # subscription"),
        "{renamed}"
    );
    let chatgpt = &config.providers["chatgpt"];
    assert_eq!(
        chatgpt.auth,
        crate::infrastructure::openai::ApiAuth::Chatgpt
    );
    assert_eq!(chatgpt.models, ["gpt-5.6-luna"]);
    assert_eq!(chatgpt.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(chatgpt.approval_model.as_deref(), Some("gpt-5.6-mini"));
    // Shared transport settings stay, and every reference follows.
    assert_eq!(config.api.timeout_secs, 30);
    assert_eq!(
        config.api.auth,
        crate::infrastructure::openai::ApiAuth::ApiKey
    );
    assert_eq!(config.default_provider(), "chatgpt");
    assert_eq!(config.providers["lan"].fallback, ["chatgpt"]);
    assert_eq!(
        config.environments["review"].provider.as_deref(),
        Some("chatgpt")
    );
    assert!(!config.listed_provider_names().any(|name| name == "api"));

    let again = with_provider_renamed(&renamed, "chatgpt", "subscription").unwrap();
    let config = AppConfig::parse(&again).unwrap();
    assert_eq!(config.default_provider(), "subscription");
    assert_eq!(config.providers["lan"].fallback, ["subscription"]);
    assert!(with_provider_renamed(&again, "subscription", "lan").is_err());
    assert!(with_provider_renamed(&again, "subscription", "api").is_err());
    assert!(with_provider_renamed(&again, "missing", "other").is_err());
}
