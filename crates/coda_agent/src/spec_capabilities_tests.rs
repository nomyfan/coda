use super::*;

fn spec(name: &str, capabilities: Capabilities, subagents: &[&str]) -> AgentSpec {
    AgentSpec {
        capabilities,
        name: name.into(),
        description: String::new(),
        system_prompt: "".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: subagents.iter().map(|name| (*name).into()).collect(),
    }
}

#[test]
fn rejects_reserved_subagent_definitions_before_build_even_when_unreachable() {
    for name in SYNTHETIC_RESERVED_TOOL_NAMES {
        for capabilities in [Capabilities::all(), Capabilities::none()] {
            for referenced in [false, true] {
                let children = [*name];
                let root = spec(
                    "coda",
                    capabilities.clone(),
                    if referenced { &children } else { &[] },
                );
                let child = spec(name, capabilities.clone(), &[]);
                assert!(matches!(AgentTeam::new(root, vec![child]),
                    Err(BuildError::ReservedSubagentName { name: rejected }) if rejected == *name));
            }
        }
    }
}

#[test]
fn rejects_reserved_targets_from_nested_agents_including_a_root_reference() {
    for name in SYNTHETIC_RESERVED_TOOL_NAMES {
        // Rust callers may give the root any name; referring to it as a child
        // must not bypass the reserved sub-agent namespace check.
        let root = spec(name, Capabilities::none(), &["worker"]);
        let child = spec("worker", Capabilities::none(), &[name]);
        assert!(matches!(AgentTeam::new(root, vec![child]),
            Err(BuildError::ReservedSubagentName { name: rejected }) if rejected == *name));
    }
}

#[test]
fn tools_and_shell_schema_follow_capabilities_and_available_resources() {
    let registry = Arc::new(BackgroundTasks::temporary().unwrap());
    for background in [false, true] {
        for ptc in [false, true] {
            for resource in [None, Some(registry.clone())] {
                let capabilities = [
                    background.then_some(Capability::Background),
                    ptc.then_some(Capability::Ptc),
                ]
                .into_iter()
                .flatten()
                .collect();
                let mut root = spec("coda", capabilities, &[]);
                root.tools = vec![Box::new(coda_tools::ShellToolSpec)];
                let programs = AgentTeam::new(root, vec![]).unwrap().build(
                    ".",
                    coda_tools::shared_file_locks(),
                    resource.clone(),
                );
                let tools = &programs["coda"].tools;
                let enabled = background && resource.is_some();
                assert_eq!(tools.get("task_output").is_some(), enabled);
                assert_eq!(tools.get("task_kill").is_some(), enabled);
                assert_eq!(
                    tools.get("shell").unwrap().parameter_schema()["properties"]
                        .get("run_in_background")
                        .is_some(),
                    enabled
                );
                assert_eq!(tools.get("run_javascript").is_some(), ptc);
            }
        }
    }
}

#[test]
fn shared_child_uses_its_own_capabilities_and_empty_tools_stay_empty() {
    let registry = Arc::new(BackgroundTasks::temporary().unwrap());
    let team = AgentTeam::new(
        spec("coda", Capabilities::all(), &["a", "b"]),
        vec![
            spec("a", Capabilities::none(), &["c"]),
            spec("b", Capabilities::all(), &["c"]),
            spec("c", [Capability::Ptc].into_iter().collect(), &[]),
        ],
    )
    .unwrap();
    let programs = team.build(".", coda_tools::shared_file_locks(), Some(registry));
    assert!(programs["a"].tools.descriptors().is_empty());
    assert!(programs["b"].tools.get("task_output").is_some());
    assert!(programs["c"].tools.get("task_output").is_none());
    assert!(programs["c"].tools.get("run_javascript").is_some());
    assert!(programs["c"].tools.get("shell").is_none());
    assert!(programs["a"].subagents.get("c").is_some());
    assert!(programs["b"].subagents.get("c").is_some());
}
