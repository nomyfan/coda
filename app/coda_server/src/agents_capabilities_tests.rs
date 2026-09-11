use super::*;
use coda_agent::Capability;
use std::sync::Arc;

#[test]
fn capability_defaults_and_explicit_lists_are_identical_for_root_and_children() {
    for (declaration, expected) in [
        ("", Capabilities::all()),
        ("capabilities: []", Capabilities::none()),
        (
            "capabilities: [ptc, ptc]",
            [Capability::Ptc].into_iter().collect(),
        ),
        (
            "capabilities: [background]",
            [Capability::Background].into_iter().collect(),
        ),
    ] {
        let root = parse_root_agent_file(&format!("---\n{declaration}\n---\nroot")).unwrap();
        let child = parse_agent_file(
            "worker",
            &format!("---\ndescription: test\nmode: stateless\n{declaration}\n---\nchild"),
        )
        .unwrap();
        assert_eq!(root.capabilities, expected);
        assert_eq!(child.capabilities, expected);
    }
    assert_eq!(RootAgentFile::default().capabilities, Capabilities::all());
}

#[test]
fn malformed_capabilities_report_the_agent_at_the_file_boundary() {
    for declaration in [
        "capabilities: [unknown]",
        "capabilities: ptc",
        "capabilities: null",
        "capabilities: {}",
    ] {
        assert!(
            matches!(parse_root_agent_file(&format!("---\n{declaration}\n---\nbody")),
            Err(LoadError::Parse { agent, .. }) if agent == "coda")
        );
        assert!(matches!(parse_agent_file("worker", &format!(
            "---\ndescription: test\nmode: stateless\n{declaration}\n---\nbody"
        )), Err(LoadError::Parse { agent, .. }) if agent == "worker"));
    }
}

#[test]
fn runtime_tool_names_cannot_be_selected_or_excluded() {
    for name in SYNTHETIC_RESERVED_TOOL_NAMES {
        for rule in ["include", "exclude"] {
            let root =
                parse_root_agent_file(&format!("---\ntools:\n  {rule}: [{name}]\n---\nbody"))
                    .unwrap();
            assert!(
                matches!(resolve_tools(&ToolRegistry::new(), "coda", root.tools.as_ref()),
                Err(LoadError::CapabilityTool { tool, .. }) if tool == *name)
            );
        }
    }
    let root = parse_root_agent_file("---\ntools: [run_*]\n---\nbody").unwrap();
    assert!(
        resolve_tools(&ToolRegistry::new(), "coda", root.tools.as_ref())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shared_child_uses_global_defaults_independently_of_its_parents() {
    let root =
        parse_root_agent_file("---\ncapabilities: []\ntools: []\nsubagents: [a, b]\n---\nroot")
            .unwrap();
    let files = [
        ("a", "capabilities: []\ntools: []\nsubagents: [c]"),
        (
            "b",
            "capabilities: [background]\ntools:\n  exclude: [shell]\nsubagents: [c]",
        ),
        ("c", ""),
    ]
    .into_iter()
    .map(|(name, fields)| {
        parse_agent_file(
            name,
            &format!("---\ndescription: test\nmode: stateless\n{fields}\n---\n{name}"),
        )
        .unwrap()
    })
    .collect();
    let team = build_agent_team(
        ".",
        SharedSystemPrompt::new("root"),
        &HashMap::new(),
        &HashMap::new(),
        &ToolRegistry::new(),
        files,
        &root,
    )
    .unwrap();
    let registry = Arc::new(coda_execution::BackgroundTasks::temporary().unwrap());
    let agents = team.build(".", coda_tools::shared_file_locks(), Some(registry));
    assert!(agents["coda"].tools.descriptors().is_empty());
    assert!(agents["a"].tools.descriptors().is_empty());
    assert!(agents["b"].tools.get("shell").is_none());
    assert!(agents["b"].tools.get("task_output").is_some());
    assert!(agents["b"].tools.get("run_javascript").is_none());
    assert_eq!(agents["c"].capabilities, Capabilities::all());
    for name in [
        "shell",
        "read_file",
        "run_javascript",
        "task_output",
        "task_kill",
    ] {
        assert!(agents["c"].tools.get(name).is_some(), "{name}");
    }
    assert!(agents["a"].subagents.get("c").is_some());
    assert!(agents["b"].subagents.get("c").is_some());
}
