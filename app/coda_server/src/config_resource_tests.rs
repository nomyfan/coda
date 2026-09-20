use super::*;

const BASE: &str = r#"
[[providers]]
id = "test"
api_key = "test"
base_url = "https://example.com"
models = [{ id = "model", context_window = 64000 }]
[[workspaces]]
id = "test"
path = "."
[database]
url = "postgres://test"
"#;

#[test]
fn resources_default_and_resolve_relative_to_config() {
    let dir = tempfile::tempdir().unwrap();
    let default = parse_server_config(BASE, dir.path()).unwrap();
    assert_eq!(default.resources, ResourceLimits::default());
    let text = format!(
        "{BASE}\n[resources.output]\nroot = 'saved-output'\n[resources.ptc]\ntimeout_secs = 240\n"
    );
    let config = parse_server_config(&text, dir.path()).unwrap();
    assert_eq!(
        config.resources.output.root,
        dir.path().join("saved-output")
    );
    assert_eq!(config.resources.ptc.timeout_secs, 240);
    assert_eq!(config.resources.ptc.heap_bytes, 64 * 1024 * 1024);
}

#[test]
fn model_output_overrides_inherit_each_unspecified_field() {
    let text = BASE.replace(
        "context_window = 64000",
        "context_window = 64000, output_limits = {single_call_bytes = 8192}",
    );
    let text = format!(
        "{text}\n[resources.output.model]\nsingle_call_bytes = 32768\nbatch_call_bytes = 131072\n"
    );
    let config = parse_server_config(&text, Path::new("/tmp")).unwrap();
    assert_eq!(
        config.providers[0].models[0].output_limits,
        ModelOutputLimits {
            single_call_bytes: 8192,
            batch_call_bytes: 131072
        }
    );
}

#[test]
fn invalid_resource_tables_and_removed_fields_fail_loading() {
    for suffix in [
        "[resources.ptc]\nmax_calls = 0",
        "[resources.ptc]\ntotal_result_bytes = 16777216",
        "[resources]\noutput = 'invalid'",
        "[resources.output]\ncapture_memory_bytes = 262144",
        "[resources.ptc]\nhost_buffer_bytes = 1048576",
        "[resources.output.model]\nsingle_call_bytes = 65536\nbatch_call_bytes = 8192",
    ] {
        assert!(
            parse_server_config(&format!("{BASE}\n{suffix}"), Path::new("/tmp")).is_err(),
            "accepted {suffix}"
        );
    }
}

#[test]
fn global_path_budget_is_checked_after_resolving_relative_root() {
    let base_dir = PathBuf::from(format!("/tmp/{}", "x".repeat(500)));
    let text = format!(
        "{BASE}\n[resources.output]\nroot = 'output'\n[resources.output.model]\nsingle_call_bytes = 1024\n"
    );
    let error = parse_server_config(&text, &base_dir)
        .unwrap_err()
        .to_string();
    assert!(error.contains("resources.output.model.single_call_bytes"));

    let minimum = ModelOutputLimits::minimum_response_bytes(&base_dir.join("output")).unwrap();
    let text = text.replace(
        "single_call_bytes = 1024",
        &format!("single_call_bytes = {minimum}"),
    );
    let config = parse_server_config(&text, &base_dir).unwrap();
    assert_eq!(config.resources.output.model.single_call_bytes, minimum);
    assert_eq!(
        config.providers[0].models[0]
            .output_limits
            .single_call_bytes,
        minimum
    );
}

#[test]
fn every_model_override_must_fit_the_resolved_output_paths() {
    let base_dir = PathBuf::from(format!("/tmp/{}", "x".repeat(500)));
    let text = BASE.replace(
        "models = [{ id = \"model\", context_window = 64000 }]",
        "models = [{ id = \"inherited\", context_window = 64000 }, { id = \"small\", context_window = 64000, output_limits = {single_call_bytes = 1024} }]",
    );
    let text = format!("{text}\n[resources.output]\nroot = 'output'\n");
    let error = parse_server_config(&text, &base_dir)
        .unwrap_err()
        .to_string();
    assert!(error.contains("provider 'test' model 'small' output_limits.single_call_bytes"));

    let minimum = ModelOutputLimits::minimum_response_bytes(&base_dir.join("output")).unwrap();
    let text = text.replace(
        "single_call_bytes = 1024",
        &format!("single_call_bytes = {minimum}"),
    );
    let config = parse_server_config(&text, &base_dir).unwrap();
    assert_eq!(
        config.providers[0].models[0].output_limits,
        ModelOutputLimits::default()
    );
    assert_eq!(
        config.providers[0].models[1]
            .output_limits
            .single_call_bytes,
        minimum
    );
    assert_eq!(
        config.providers[0].models[1].output_limits.batch_call_bytes,
        config.resources.output.model.batch_call_bytes
    );
}
