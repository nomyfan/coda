use super::*;
use coda_agent::{PendingApproval, ToolCallResolution};

#[test]
fn task_params_omits_empty_images() {
    let json = serde_json::to_string(&TaskParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        task: "hello".into(),
        images: vec![],
    })
    .unwrap();
    assert_eq!(
        json,
        r#"{"workspace_id":"coda","session_id":"s1","task":"hello"}"#
    );
}

#[test]
fn open_session_params_defaults_takeover_off() {
    // Clients omit `takeover`; it defaults off (no silent eviction).
    let params: OpenSessionParams =
        serde_json::from_str(r#"{"workspace_id":"coda","session_id":"s1"}"#).unwrap();
    assert!(!params.takeover);
    assert!(params.provider_id.is_none());
    assert!(params.reasoning_effort.is_none());
}

#[test]
fn resume_params_roundtrips() {
    let params = ResumeParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        agent_name: "coda".into(),
        thread_id: "t1".into(),
        decision: ResumeDecision {
            parent_message_id: MessageId::new(),
            resolutions: vec![("call_1".into(), ToolCallResolution::Execute)],
        },
    };
    let back: ResumeParams =
        serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
    assert_eq!(back.agent_name, "coda");
    assert_eq!(back.thread_id, "t1");
    assert_eq!(back.decision.resolutions.len(), 1);
}

#[test]
fn session_ref_roundtrips() {
    let json = serde_json::to_string(&SessionRef {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
    })
    .unwrap();
    assert_eq!(json, r#"{"workspace_id":"coda","session_id":"s1"}"#);
}

#[test]
fn add_allow_pattern_params_roundtrips() {
    let params: AddAllowPatternParams =
        serde_json::from_str(r#"{"workspace_id":"coda","pattern":"git *"}"#).unwrap();
    assert_eq!(params.workspace_id, "coda");
    assert_eq!(params.pattern, "git *");
}

#[test]
fn rename_session_params_and_result_roundtrip() {
    let params: RenameSessionParams = serde_json::from_str(
        r#"{"workspace_id":"coda","session_id":"s1","name":"  Investigation  "}"#,
    )
    .unwrap();
    assert_eq!(params.name.as_deref(), Some("  Investigation  "));

    let result = SessionName {
        name: Some("Investigation".into()),
    };
    let back: SessionName = serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(back.name.as_deref(), Some("Investigation"));
}

#[test]
fn fork_params_default_to_a_full_copy() {
    // The list entry sends no cut at all, so an absent `cut_message_id` has
    // to mean "copy everything" rather than failing to parse.
    let params: ForkSessionParams =
        serde_json::from_str(r#"{"workspace_id":"coda","session_id":"s1"}"#).unwrap();
    assert_eq!(params.cut_message_id, None);

    let cut = MessageId::new();
    let params = ForkSessionParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        cut_message_id: Some(cut),
    };
    let back: ForkSessionParams =
        serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
    assert_eq!(back.cut_message_id, Some(cut));

    let result = ForkAccepted {
        session_id: "s2".into(),
        name: None,
        workspaces: vec![],
    };
    let back: ForkAccepted =
        serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(back.session_id, "s2");
    assert_eq!(back.name, None);
}

#[test]
fn rewind_params_and_result_roundtrip() {
    // `images` is omitted when empty, exactly as `task` does it — an edited
    // message travels on the same shape as an original one.
    let params = RewindParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        message_id: MessageId::new(),
        task: "ask it differently".into(),
        images: vec![],
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(!json.contains("images"));
    let back: RewindParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back.message_id, params.message_id);
    assert_eq!(back.task, "ask it differently");

    let result = RewindAccepted {
        message_id: MessageId::new(),
        messages: vec![],
    };
    let back: RewindAccepted =
        serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(back.message_id, result.message_id);
    assert!(back.messages.is_empty());
}

#[test]
fn set_model_params_defaults_effort_to_none() {
    let params: SetModelParams = serde_json::from_str(
        r#"{"workspace_id":"coda","session_id":"s1","provider_id":"deepseek"}"#,
    )
    .unwrap();
    assert_eq!(params.provider_id, "deepseek");
    assert!(params.reasoning_effort.is_none());
}

#[test]
fn snapshot_serializes_without_type_tag() {
    let msg = Snapshot {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        messages: vec![],
        pending_approvals: vec![],
        provider_id: "deepseek".into(),
        reasoning_effort: Some("high".into()),
        permission_mode: PermissionMode::Yolo,
        turn_running: true,
        compacting: false,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert_eq!(
        json,
        r#"{"workspace_id":"coda","session_id":"s1","messages":[],"pending_approvals":[],"provider_id":"deepseek","reasoning_effort":"high","permission_mode":"yolo","turn_running":true,"compacting":false}"#
    );
}

#[test]
fn snapshot_without_turn_running_defaults_to_false() {
    let json = r#"{"workspace_id":"coda","session_id":"s1","messages":[],"pending_approvals":[],"provider_id":"deepseek","reasoning_effort":null}"#;
    let snapshot: Snapshot = serde_json::from_str(json).unwrap();
    assert!(!snapshot.turn_running);
    assert_eq!(snapshot.permission_mode, PermissionMode::AcceptEdits);
}

#[test]
fn permission_mode_round_trips_over_the_wire() {
    for (mode, tag) in [
        (PermissionMode::Explore, "\"explore\""),
        (PermissionMode::AcceptEdits, "\"accept_edits\""),
        (PermissionMode::Yolo, "\"yolo\""),
    ] {
        assert_eq!(serde_json::to_string(&mode).unwrap(), tag);
        assert_eq!(serde_json::from_str::<PermissionMode>(tag).unwrap(), mode);
    }
}

#[test]
fn model_selection_roundtrips() {
    let result = ModelSelection {
        provider_id: "openai:gpt-4o".into(),
        reasoning_effort: None,
    };
    let back: ModelSelection =
        serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(back.provider_id, "openai:gpt-4o");
    assert!(back.reasoning_effort.is_none());
}

#[test]
fn pending_approval_wire_suggests_shell_allow_patterns() {
    let approval = PendingApproval {
        thread_id: "t1".into(),
        agent_name: "coda".into(),
        parent_message_id: MessageId::new(),
        calls: vec![
            ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: Some(r##"{"command":"# Run tests\ncargo test"}"##.into()),
            },
            ToolCall {
                id: "call_read".into(),
                name: "read_file".into(),
                arguments: Some(r#"{"path":"README.md"}"#.into()),
            },
        ],
        suspended_at: jiff::Timestamp::default(),
    };

    let wire = PendingApprovalWire::from_agent(approval);

    assert_eq!(
        wire.suggested_shell_allow_patterns.get("call_shell"),
        Some(&"cargo test".to_string())
    );
    assert!(
        !wire
            .suggested_shell_allow_patterns
            .contains_key("call_read")
    );
}

#[test]
fn pending_approval_wire_skips_compound_shell_calls() {
    let approval = PendingApproval {
        thread_id: "t1".into(),
        agent_name: "coda".into(),
        parent_message_id: MessageId::new(),
        calls: vec![ToolCall {
            id: "call_shell".into(),
            name: "shell".into(),
            arguments: Some(r##"{"command":"# Navigate\ncd /work/coda && cargo test"}"##.into()),
        }],
        suspended_at: jiff::Timestamp::default(),
    };

    let wire = PendingApprovalWire::from_agent(approval);

    assert!(wire.suggested_shell_allow_patterns.is_empty());
}

#[test]
fn pending_approval_wire_skips_shell_calls_with_only_comments() {
    let approval = PendingApproval {
        thread_id: "t1".into(),
        agent_name: "coda".into(),
        parent_message_id: MessageId::new(),
        calls: vec![ToolCall {
            id: "call_shell".into(),
            name: "shell".into(),
            arguments: Some(r##"{"command":"# just a comment"}"##.into()),
        }],
        suspended_at: jiff::Timestamp::default(),
    };

    let wire = PendingApprovalWire::from_agent(approval);

    assert!(wire.suggested_shell_allow_patterns.is_empty());
}

#[test]
fn pending_approval_wire_skips_unresolvable_shell_calls() {
    let approval = PendingApproval {
        thread_id: "t1".into(),
        agent_name: "coda".into(),
        parent_message_id: MessageId::new(),
        calls: vec![ToolCall {
            id: "call_shell".into(),
            name: "shell".into(),
            arguments: Some(r##"{"command":"git status > /tmp/out"}"##.into()),
        }],
        suspended_at: jiff::Timestamp::default(),
    };

    let wire = PendingApprovalWire::from_agent(approval);

    assert!(wire.suggested_shell_allow_patterns.is_empty());
}

#[test]
fn set_model_params_roundtrips() {
    let params = SetModelParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        provider_id: "deepseek".into(),
        reasoning_effort: None,
    };
    let back: SetModelParams =
        serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
    assert_eq!(back.provider_id, "deepseek");
    assert!(back.reasoning_effort.is_none());
}

#[test]
fn workspace_catalog_roundtrips() {
    let msg = WorkspaceCatalog {
        workspaces: vec![WorkspaceSummaryWire {
            id: "coda".into(),
            path: "/work/coda".into(),
            sessions: vec![SessionSummaryWire {
                id: "s1".into(),
                name: Some("Investigation".into()),
                updated_at_ms: Some(42),
                first_user_message: Some("inspect the repo".into()),
                has_pending_approval: true,
            }],
        }],
    };
    let back: WorkspaceCatalog =
        serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    assert_eq!(back.workspaces[0].id, "coda");
    assert_eq!(back.workspaces[0].sessions[0].id, "s1");
    assert_eq!(
        back.workspaces[0].sessions[0].name.as_deref(),
        Some("Investigation")
    );
    assert!(back.workspaces[0].sessions[0].has_pending_approval);
}

#[test]
fn provider_catalog_roundtrips() {
    let msg = ProviderCatalog {
        providers: vec![ProviderInfoWire {
            id: "deepseek:deepseek-reasoner".into(),
            provider: "deepseek".into(),
            model: "deepseek-reasoner".into(),
            context_window: 128_000,
            reasoning_efforts: vec!["low".into(), "high".into()],
            default_reasoning_effort: Some("high".into()),
            input_modalities: vec![Modality::Text, Modality::Image],
        }],
        default_provider: "deepseek:deepseek-reasoner".into(),
    };
    let back: ProviderCatalog =
        serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    assert_eq!(back.providers[0].id, "deepseek:deepseek-reasoner");
    assert_eq!(back.providers[0].provider, "deepseek");
    assert_eq!(back.providers[0].context_window, 128_000);
    assert_eq!(back.providers[0].reasoning_efforts.len(), 2);
    assert_eq!(
        back.providers[0].default_reasoning_effort,
        Some("high".into())
    );
    assert_eq!(back.default_provider, "deepseek:deepseek-reasoner");
}

#[test]
fn event_params_roundtrips() {
    let msg = EventParams {
        workspace_id: "coda".into(),
        session_id: "s1".into(),
        event: WireEvent::LlmContentChunk {
            agent_name: "coda".into(),
            thread_id: "t1".into(),
            content: "hi".into(),
        },
    };
    let back: EventParams = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    assert_eq!(back.workspace_id, "coda");
    assert_eq!(back.session_id, "s1");
    assert!(matches!(
        back.event,
        WireEvent::LlmContentChunk { content, .. } if content == "hi"
    ));
}

#[test]
fn list_files_params_default_to_the_whole_workspace() {
    // Right after typing `@` the client sends no query and no limit; both fall
    // back to server-side defaults rather than being required of the picker.
    let params: ListFilesParams = serde_json::from_str(r#"{"workspace_id":"coda"}"#).unwrap();
    assert_eq!(params.query, "");
    assert!(params.limit.is_none());

    let params: ListFilesParams =
        serde_json::from_str(r#"{"workspace_id":"coda","query":"comp","limit":10}"#).unwrap();
    assert_eq!(params.query, "comp");
    assert_eq!(params.limit, Some(10));
}

#[test]
fn file_catalog_roundtrips() {
    let catalog = FileCatalog {
        files: vec![FileEntry {
            path: "src/main.rs".into(),
            is_dir: false,
        }],
        truncated: true,
    };
    let back: FileCatalog =
        serde_json::from_str(&serde_json::to_string(&catalog).unwrap()).unwrap();
    assert_eq!(back.files[0].path, "src/main.rs");
    assert!(!back.files[0].is_dir);
    assert!(back.truncated);
}

#[test]
fn skill_catalog_roundtrips() {
    let catalog = SkillCatalog {
        skills: vec![SkillInfoWire {
            name: "code-review".into(),
            description: "Review the current diff".into(),
        }],
    };
    let back: SkillCatalog =
        serde_json::from_str(&serde_json::to_string(&catalog).unwrap()).unwrap();
    assert_eq!(back.skills[0].name, "code-review");
    assert_eq!(back.skills[0].description, "Review the current diff");
}
