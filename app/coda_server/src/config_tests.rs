use std::ffi::OsString;

use super::*;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: this test owns its unique environment variable name.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: this guard restores the unique variable it owns.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn wildcard_basics() {
    assert!(wildcard_match("git *", "git status"));
    assert!(wildcard_match("git *", "git push --force"));
    assert!(!wildcard_match("git *", "gitk"));
    assert!(!wildcard_match("git *", "git"));
    assert!(wildcard_match("cargo *", "cargo test --release"));
    assert!(wildcard_match("rm -rf *", "rm -rf /"));
    assert!(!wildcard_match("rm -rf *", "rm file.txt"));
}

#[test]
fn wildcard_no_space() {
    assert!(wildcard_match("git*", "git"));
    assert!(wildcard_match("git*", "gitk"));
    assert!(wildcard_match("git*", "git status"));
}

#[test]
fn wildcard_question_mark() {
    assert!(wildcard_match("l?", "ls"));
    assert!(!wildcard_match("l?", "lss"));
}

#[test]
fn wildcard_exact() {
    assert!(wildcard_match("ls", "ls"));
    assert!(!wildcard_match("ls", "lsof"));
}

#[test]
fn parse_empty_config() {
    let permissions = parse_permissions("").unwrap();
    assert!(permissions.shell_allow.is_empty());
    assert!(permissions.shell_deny.is_empty());
    // The mode decides the baseline; a silent config adds no locks of its own.
    assert!(permissions.approval_required_tools.is_empty());
}

/// Every config needs a database, so fixtures that are not about the
/// database still have to carry one.
const DATABASE: &str = r#"
[database]
url = "postgres://localhost/coda"
"#;

const PROVIDERS: &str = r#"
[[providers]]
id = "deepseek"
kind = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", name = "DeepSeek R1", context_window = 128000, reasoning_efforts = ["low", "medium", "high"] },
]
"#;

#[test]
fn parse_server_config_resolves_workspaces() {
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "projects/coda"

[[workspaces]]
id = "scratch"
path = "/tmp/scratch"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(
        config.workspaces,
        vec![
            WorkspaceConfig {
                id: "coda".to_string(),
                path: PathBuf::from("/srv/projects/coda"),
            },
            WorkspaceConfig {
                id: "scratch".to_string(),
                path: PathBuf::from("/tmp/scratch"),
            },
        ]
    );
    assert_eq!(
        config.providers,
        vec![ProviderConfig {
            id: "deepseek".to_string(),
            kind: ProviderKind::Deepseek,
            api_key: "sk-test".to_string(),
            base_url: "https://api.deepseek.com/v1".to_string(),
            include_usage: true,
            models: vec![ModelConfig {
                id: "deepseek-reasoner".to_string(),
                name: "DeepSeek R1".to_string(),
                context_window: 128_000,
                max_completion_tokens: None,
                reasoning_efforts: vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                ],
                default_reasoning_effort: None,
                input_modalities: vec![Modality::Text],
                auto_compact_threshold: None,
            }],
        }]
    );
}

#[test]
fn parse_server_config_expands_env_api_key() {
    let _env = EnvVarGuard::set("CODA_TEST_KEY", "secret-from-env");
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "deepseek"
api_key = "${CODA_TEST_KEY}"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", context_window = 128000 },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();
    assert_eq!(config.providers[0].api_key, "secret-from-env");
    assert_eq!(config.providers[0].kind, ProviderKind::Generic);
    assert_eq!(config.providers[0].models.len(), 1);
    assert_eq!(config.providers[0].models[0].id, "deepseek-reasoner");
    assert_eq!(config.providers[0].models[0].name, "deepseek-reasoner");
    assert!(config.providers[0].models[0].reasoning_efforts.is_empty());
}

#[test]
fn parse_server_config_expands_env_database_url() {
    let _env = EnvVarGuard::set("CODA_TEST_DATABASE_URL", "postgres://user:pw@db:5432/coda");
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}
[database]
url = "${{CODA_TEST_DATABASE_URL}}"

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();
    assert_eq!(config.database.url, "postgres://user:pw@db:5432/coda");
}

#[test]
fn parse_server_config_requires_a_database() {
    // PostgreSQL is the only backend, so there is nothing sensible to fall
    // back to — starting without it would only fail later, per session.
    let err = parse_server_config(
        &format!(
            r#"{PROVIDERS}
[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap_err();
    assert!(
        matches!(&err, ConfigError::Parse(message) if message.contains("[database]")),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_server_config_accepts_arbitrary_reasoning_efforts() {
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", context_window = 128000, reasoning_efforts = ["off", "ultra", "max"] },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();
    assert_eq!(
        config.providers[0].models[0].reasoning_efforts,
        vec!["off", "ultra", "max"]
    );
}

#[test]
fn parse_server_config_parses_default_reasoning_effort() {
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", context_window = 128000, reasoning_efforts = ["low", "medium", "high"], default_reasoning_effort = "medium" },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();
    assert_eq!(
        config.providers[0].models[0].default_reasoning_effort,
        Some("medium".to_string())
    );
}

#[test]
fn parse_server_config_rejects_invalid_default_reasoning_effort() {
    let err = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner", context_window = 128000, reasoning_efforts = ["low", "high"], default_reasoning_effort = "medium" },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("default_reasoning_effort 'medium' is not in reasoning_efforts")
    );
}

#[test]
fn parse_server_config_parses_input_modalities() {
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openai"
api_key = "sk-test"
base_url = "https://api.openai.com/v1"
models = [
  { id = "gpt-4o", context_window = 128000, input_modalities = ["text", "image"] },
  { id = "o1", context_window = 128000 },
  { id = "img-only", context_window = 128000, input_modalities = ["image", "image"] },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();
    let models = &config.providers[0].models;
    assert_eq!(
        models[0].input_modalities,
        vec![Modality::Text, Modality::Image]
    );
    // Absent key defaults to text-only.
    assert_eq!(models[1].input_modalities, vec![Modality::Text]);
    // Normalized: text is always present and duplicates are dropped.
    assert_eq!(
        models[2].input_modalities,
        vec![Modality::Text, Modality::Image]
    );
}

#[test]
fn parse_server_config_rejects_unknown_input_modality() {
    let err = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openai"
api_key = "sk-test"
base_url = "https://api.openai.com/v1"
models = [
  { id = "gpt-4o", context_window = 128000, input_modalities = ["audio"] },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown input modality 'audio'"));
}

#[test]
fn parse_server_config_requires_context_window() {
    let err = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "deepseek"
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
models = [
  { id = "deepseek-reasoner" },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("context_window must be a positive integer")
    );
}

#[test]
fn parse_server_config_accepts_openrouter_and_optional_completion_limit() {
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openrouter"
kind = "openrouter"
api_key = "sk-test"
base_url = "https://openrouter.ai/api/v1"
models = [
  { id = "x-ai/grok-4.5", context_window = 500000, max_completion_tokens = 16384 },
  { id = "z-ai/glm-5.2", context_window = 1048576 },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(config.providers[0].kind, ProviderKind::OpenRouter);
    assert_eq!(
        config.providers[0].models[0].max_completion_tokens,
        Some(16_384)
    );
    assert_eq!(config.providers[0].models[1].max_completion_tokens, None);
}

#[test]
fn parse_server_config_rejects_unknown_provider_kind() {
    let error = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "gateway"
kind = "other"
api_key = "sk-test"
base_url = "https://example.com/v1"
models = [{ id = "test", context_window = 1000 }]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("unknown kind 'other'"));
}

#[test]
fn parse_server_config_rejects_invalid_completion_limits() {
    for value in ["0", "-1", "1001", "\"large\""] {
        let config = format!(
            r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openrouter"
kind = "openrouter"
api_key = "sk-test"
base_url = "https://openrouter.ai/api/v1"
models = [
  {{ id = "test", context_window = 1000, max_completion_tokens = {value} }},
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        );
        let error = parse_server_config(&config, Path::new("/srv")).unwrap_err();
        assert!(error.to_string().contains("max_completion_tokens"));
    }
}

#[test]
fn parse_server_config_accepts_optional_auto_compact_threshold() {
    let config = parse_server_config(
        r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openrouter"
kind = "openrouter"
api_key = "sk-test"
base_url = "https://openrouter.ai/api/v1"
models = [
  { id = "x-ai/grok-4.5", context_window = 500000, auto_compact_threshold = 400000 },
  { id = "z-ai/glm-5.2", context_window = 1048576 },
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#,
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(
        config.providers[0].models[0].auto_compact_threshold,
        Some(400_000)
    );
    assert_eq!(config.providers[0].models[1].auto_compact_threshold, None);
}

#[test]
fn parse_server_config_rejects_invalid_auto_compact_threshold() {
    for value in ["0", "-1", "1001", "\"large\""] {
        let config = format!(
            r#"
[database]
url = "postgres://localhost/coda"

[[providers]]
id = "openrouter"
kind = "openrouter"
api_key = "sk-test"
base_url = "https://openrouter.ai/api/v1"
models = [
  {{ id = "test", context_window = 1000, auto_compact_threshold = {value} }},
]

[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        );
        let error = parse_server_config(&config, Path::new("/srv")).unwrap_err();
        assert!(error.to_string().contains("auto_compact_threshold"));
    }
}

#[test]
fn parse_server_config_rejects_duplicate_ids() {
    let err = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/a"

[[workspaces]]
id = "coda"
path = "/tmp/b"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap_err();

    assert!(err.to_string().contains("duplicate workspace id"));
}

#[test]
fn parse_server_config_defaults_relay_limits() {
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(config.relay, RelayConfig::default());
}

#[test]
fn parse_server_config_overrides_relay_limits() {
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"

[relay]
max_log_events = 100
max_message_tier_events = 50
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(
        config.relay,
        RelayConfig {
            max_log_events: 100,
            max_message_tier_events: 50,
        }
    );
}

#[test]
fn parse_server_config_rejects_non_positive_relay_limit() {
    let err = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"

[relay]
max_log_events = 0
"#
        ),
        Path::new("/srv"),
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("relay.max_log_events must be a positive integer")
    );
}

#[test]
fn parse_server_config_defaults_keepalive() {
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(config.keepalive, KeepaliveConfig::default());
    assert_eq!(config.keepalive.interval, Duration::from_secs(30));
}

#[test]
fn parse_server_config_overrides_keepalive_interval() {
    let config = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"

[keepalive]
interval_secs = 10
"#
        ),
        Path::new("/srv"),
    )
    .unwrap();

    assert_eq!(config.keepalive.interval, Duration::from_secs(10));
}

#[test]
fn parse_server_config_rejects_non_positive_keepalive_interval() {
    let err = parse_server_config(
        &format!(
            r#"{PROVIDERS}{DATABASE}
[[workspaces]]
id = "coda"
path = "/tmp/coda"

[keepalive]
interval_secs = 0
"#
        ),
        Path::new("/srv"),
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("keepalive.interval_secs must be a positive integer")
    );
}

#[test]
fn parse_full_config() {
    let toml = r#"
[permissions.tools]
approval_required = ["ask_user", "write_todos"]

[permissions.shell]
allow = ["git *", "cargo *"]
deny = ["rm -rf *"]
"#;
    let permissions = parse_permissions(toml).unwrap();
    assert_eq!(permissions.shell_allow, vec!["git *", "cargo *"]);
    assert_eq!(permissions.shell_deny, vec!["rm -rf *"]);
    assert_eq!(
        permissions.approval_required_tools,
        vec!["ask_user", "write_todos"]
    );
}

#[test]
fn parse_permissions_rejects_non_array_approval_required() {
    let err =
        parse_permissions("[permissions.tools]\napproval_required = \"write_file\"\n").unwrap_err();
    assert!(
        err.to_string()
            .contains("permissions approval_required must be an array")
    );
}

#[test]
fn parse_permissions_rejects_non_string_approval_required_item() {
    let err = parse_permissions("[permissions.tools]\napproval_required = [\"write_file\", 1]\n")
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("permissions approval_required must be strings")
    );
}

#[test]
fn config_load_nonexistent() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("ls")));
}

#[test]
fn config_deny_overrides_allow() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    {
        let mut inner = config.inner.lock().unwrap();
        inner.allow.push("rm *".to_string());
        inner.deny.push("rm -rf *".to_string());
    }
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &shell_call("rm file.txt")));
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("rm -rf /")));
}

#[test]
fn config_non_shell_tools_skip() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    let call = ToolCall {
        id: "1".into(),
        name: "read_file".into(),
        arguments: None,
    };
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &call));
}

#[test]
fn explore_auto_approves_only_read_only_tools() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    for tool in [
        "ls",
        "read_file",
        "glob",
        "grep",
        "read_todos",
        "write_todos",
    ] {
        assert!(
            !config.requires_approval(PermissionMode::Explore, &tool_call(tool)),
            "{tool} should run unattended under explore"
        );
    }
    for tool in ["write_file", "edit_file", "mcp__time__now"] {
        assert!(
            config.requires_approval(PermissionMode::Explore, &tool_call(tool)),
            "{tool} should ask under explore"
        );
    }
}

#[test]
fn accept_edits_adds_the_file_writers() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &tool_call("write_file")));
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &tool_call("edit_file")));
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &tool_call("read_file")));
    // Still an allow-list: anything it does not name keeps asking.
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &tool_call("mcp__time__now")));
}

#[test]
fn yolo_auto_approves_everything_it_can() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    for tool in ["write_file", "mcp__time__now", "some_unknown_tool"] {
        assert!(
            !config.requires_approval(PermissionMode::Yolo, &tool_call(tool)),
            "{tool} should run unattended under yolo"
        );
    }
    // An empty allow-list would stop every command under the other modes;
    // yolo runs them anyway.
    assert!(!config.requires_approval(PermissionMode::Yolo, &shell_call("rm -rf /tmp/x")));
    assert!(config.requires_approval(PermissionMode::Explore, &shell_call("rm -rf /tmp/x")));
    // Except `ask_user`, which asks a question rather than for permission.
    assert!(config.requires_approval(PermissionMode::Yolo, &tool_call("ask_user")));
}

#[test]
fn yolo_still_respects_the_deny_list() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    config.inner.lock().unwrap().deny.push("git push *".into());
    assert!(config.requires_approval(PermissionMode::Yolo, &shell_call("git push origin main")));
    assert!(config.requires_approval(PermissionMode::Yolo, &shell_call("ls && git push --force")));
    assert!(!config.requires_approval(PermissionMode::Yolo, &shell_call("git pull")));
}

#[test]
fn delegation_is_never_gated() {
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    // Each tool the sub-agent then calls is checked against this same policy,
    // so gating the delegation itself would only double-charge the user.
    for mode in [
        PermissionMode::Explore,
        PermissionMode::AcceptEdits,
        PermissionMode::Yolo,
    ] {
        assert!(!config.requires_approval(mode, &tool_call("agent__explore")));
    }
}

#[test]
fn interactive_tools_always_require_approval() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "[permissions.tools]\napproval_required = []\n",
    )
    .unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &tool_call("ask_user")));
}

#[test]
fn configured_tools_tighten_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "[permissions.tools]\napproval_required = [\"write_todos\"]\n",
    )
    .unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    // The workspace's own lock outranks the mode — yolo included.
    assert!(config.requires_approval(PermissionMode::Explore, &tool_call("write_todos")));
    assert!(config.requires_approval(PermissionMode::Yolo, &tool_call("write_todos")));
    // Everything it does not name is left to the mode.
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &tool_call("write_file")));
}

#[test]
fn configured_tool_patterns_require_approval() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "[permissions.tools]\napproval_required = [\"mcp__time__*\"]\n",
    )
    .unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    // Read the pattern under yolo: that is the mode where being on the
    // tightening list is the only thing that can still stop a tool, so a match
    // and a miss are told apart cleanly.
    assert!(config.requires_approval(
        PermissionMode::Yolo,
        &tool_call("mcp__time__get_current_time")
    ));
    assert!(config.requires_approval(PermissionMode::Yolo, &tool_call("mcp__time__convert_time")));
    assert!(!config.requires_approval(
        PermissionMode::Yolo,
        &tool_call("mcp__filesystem__read_file")
    ));
}

#[test]
fn approval_required_defaults_to_empty() {
    // The mode carries the baseline now, so a workspace that says nothing
    // adds nothing: `edit_file` is decided by the mode alone.
    let config = ToolApprovalConfig::load(Path::new("/nonexistent")).unwrap();
    assert!(
        config
            .inner
            .lock()
            .unwrap()
            .approval_required_tools
            .is_empty()
    );
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &tool_call("edit_file")));
    assert!(config.requires_approval(PermissionMode::Explore, &tool_call("edit_file")));
}

#[test]
fn derive_pattern_works() {
    assert_eq!(
        ToolApprovalConfig::derive_pattern("git status --short"),
        "git status *"
    );
    assert_eq!(ToolApprovalConfig::derive_pattern("ls"), "ls");
    assert_eq!(
        ToolApprovalConfig::derive_pattern("cargo test"),
        "cargo test"
    );
    assert_eq!(
        ToolApprovalConfig::derive_pattern("cargo test --release"),
        "cargo test *"
    );
    assert_eq!(
        ToolApprovalConfig::derive_pattern("# Run tests\ncargo test --release"),
        "cargo test *"
    );
    assert_eq!(
        ToolApprovalConfig::derive_pattern("\n  \n# Run tests\ncargo test --release"),
        "cargo test *"
    );
    assert_eq!(
        ToolApprovalConfig::derive_pattern("# just a comment"),
        "# just *"
    );
}

#[test]
fn add_allow_pattern_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    config.add_allow_pattern("git *").unwrap();
    config.add_allow_pattern("cargo *").unwrap();
    // duplicate should be ignored
    config.add_allow_pattern("git *").unwrap();

    let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("git status")));
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("cargo test")));
    assert!(reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("rm file")));
}

#[test]
fn add_allow_preserves_deny() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "[permissions.shell]\ndeny = [\"rm -rf *\"]\n").unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    config.add_allow_pattern("git *").unwrap();

    let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("git push")));
    assert!(reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("rm -rf /")));
}

#[test]
fn add_allow_not_persisted_on_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    // Place a file where the .coda directory needs to be, so create_dir_all fails.
    std::fs::write(dir.path().join(".coda"), "blocker").unwrap();
    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    let result = config.add_allow_pattern("git *");
    assert!(result.is_err());
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("git status")));
}

#[test]
fn write_handles_wrong_shaped_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "permissions = \"not a table\"\n").unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    config.add_allow_pattern("git *").unwrap();

    let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("git status")));
}

#[test]
fn write_handles_wrong_shaped_shell() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "[permissions]\nshell = \"not a table\"\n").unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    config.add_allow_pattern("cargo *").unwrap();

    let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("cargo test")));
}

#[test]
fn write_preserves_inline_table_deny() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".coda").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "[permissions]\nshell = { deny = [\"rm -rf *\"] }\n",
    )
    .unwrap();

    let config = ToolApprovalConfig::load(dir.path()).unwrap();
    config.add_allow_pattern("git *").unwrap();

    let reloaded = ToolApprovalConfig::load(dir.path()).unwrap();
    assert!(!reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("git push")));
    assert!(reloaded.requires_approval(PermissionMode::AcceptEdits, &shell_call("rm -rf /")));
}

#[test]
fn compound_of_allowed_commands_auto_approves() {
    let config = ToolApprovalConfig::default_for(Path::new("/tmp"));
    {
        let mut inner = config.inner.lock().unwrap();
        inner.allow.push("git *".to_string());
        inner.allow.push("cargo *".to_string());
        inner.allow.push("cd *".to_string());
    }
    // Sequencing, and-or, and pipes auto-approve when every constituent
    // simple command is allowed.
    assert!(!config.requires_approval(PermissionMode::AcceptEdits, &shell_call("git status")));
    assert!(!config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("cd app && cargo test")
    ));
    assert!(!config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git fetch; git status")
    ));
    assert!(!config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git log | cargo run")
    ));
    assert!(!config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status || git fetch")
    ));
    assert!(!config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status\ncargo test")
    ));
}

#[test]
fn compound_with_disallowed_command_requires_approval() {
    let config = ToolApprovalConfig::default_for(Path::new("/tmp"));
    {
        let mut inner = config.inner.lock().unwrap();
        inner.allow.push("git *".to_string());
    }
    // A single disallowed simple command anywhere in the compound gates it.
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status; rm -rf /")
    ));
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status && echo done")
    ));
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("git log | head")));
}

#[test]
fn unresolvable_constructs_require_approval() {
    let config = ToolApprovalConfig::default_for(Path::new("/tmp"));
    {
        let mut inner = config.inner.lock().unwrap();
        inner.allow.push("git *".to_string());
        inner.allow.push("echo *".to_string());
    }
    // Backgrounding, redirections, and substitutions can't be statically
    // vetted even when the visible command is allowed.
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status & echo done")
    ));
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status > /tmp/out")
    ));
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("git status < /dev/null")
    ));
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("echo `whoami`")));
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("echo $(whoami)")));
    // Compound commands (subshells, loops) fall back to approval.
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("(git status)")));
    assert!(config.requires_approval(
        PermissionMode::AcceptEdits,
        &shell_call("for f in *; do echo $f; done")
    ));
    // A syntactically invalid command is never auto-approved.
    assert!(config.requires_approval(PermissionMode::AcceptEdits, &shell_call("git status &&")));
}

#[test]
fn derive_pattern_with_tab() {
    let pattern = ToolApprovalConfig::derive_pattern("git\tstatus");
    assert_eq!(pattern, "git status");
    assert!(wildcard_match(&pattern, "git\tstatus"));
    assert!(wildcard_match(&pattern, "git status"));
}

#[test]
fn wildcard_whitespace_matches_any_whitespace() {
    assert!(wildcard_match("git *", "git\tstatus"));
    assert!(wildcard_match("rm -rf *", "rm\t-rf /"));
    assert!(!wildcard_match("git *", "gitk"));
}

fn shell_call(command: &str) -> ToolCall {
    let args = serde_json::json!({"command": command}).to_string();
    ToolCall {
        id: "test".into(),
        name: "shell".into(),
        arguments: Some(args),
    }
}

fn tool_call(name: &str) -> ToolCall {
    ToolCall {
        id: "test".into(),
        name: name.into(),
        arguments: None,
    }
}
