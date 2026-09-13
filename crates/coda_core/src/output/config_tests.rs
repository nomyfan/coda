use super::*;

#[test]
fn default_limits_leave_a_separate_log_reservation() {
    let limits = ResourceLimits::default();
    limits.validate().unwrap();
    assert_eq!(limits.log_buffer_bytes(), 320 * KIB);
    assert_eq!(
        limits.result_buffer_bytes() + limits.log_buffer_bytes(),
        limits.ptc.host_buffer_bytes
    );
}

#[test]
fn invalid_resource_relations_report_the_configuration_field() {
    let mut limits = ResourceLimits::default();
    limits.output.session_disk_bytes = limits.output.result_max_bytes - 1;
    assert!(
        limits
            .validate()
            .unwrap_err()
            .starts_with("resources.output.session_disk_bytes")
    );
    let mut limits = ResourceLimits::default();
    limits.output.capture_memory_bytes = 4 * MIB;
    limits.ptc.host_buffer_bytes = 4 * MIB;
    assert!(
        limits
            .validate()
            .unwrap_err()
            .starts_with("resources.ptc.host_buffer_bytes")
    );
    let mut limits = ResourceLimits::default();
    limits.ptc.max_concurrent_calls = limits.ptc.max_calls + 1;
    assert!(
        limits
            .validate()
            .unwrap_err()
            .starts_with("resources.ptc.max_concurrent_calls")
    );
}

#[test]
fn dispatch_budget_is_bounded_and_rejects_unrepresentable_batches() {
    let limits = ModelOutputLimits {
        single_bytes: 1024,
        batch_bytes: 4097,
    };
    assert_eq!(limits.allocate(1, 512).unwrap(), vec![1024]);
    assert_eq!(
        limits.allocate(5, 512).unwrap(),
        vec![820, 820, 819, 819, 819]
    );
    assert!(limits.allocate(9, 512).is_err());
    assert!(limits.allocate(usize::MAX, 512).is_err());
    assert!(limits.allocate(0, 512).unwrap().is_empty());
}

#[test]
fn removed_intermediate_limits_are_rejected() {
    assert!(
        serde_json::from_value::<ResourceLimits>(serde_json::json!({
            "ptc": {"total_result_bytes": 16777216}
        }))
        .is_err()
    );
}

#[test]
fn long_output_roots_require_room_for_all_channel_paths() {
    let mut limits = ResourceLimits::default();
    limits.output.root = PathBuf::from(format!("/tmp/{}", "x".repeat(500)));
    limits.output.model.single_bytes = 1024;
    let error = limits.validate().unwrap_err();
    assert!(error.starts_with("resources.output.model.single_bytes"));
    assert!(error.contains("complete output paths and metadata"));

    let minimum = ModelOutputLimits::minimum_response_bytes(&limits.output.root).unwrap();
    assert!(minimum > 4 * 505);
    limits.output.model.single_bytes = minimum;
    limits.validate().unwrap();
    limits.output.model.single_bytes -= 1;
    assert!(limits.validate().is_err());
}

#[test]
fn path_budget_counts_utf8_bytes_and_json_escaping_for_every_channel() {
    let plain = ModelOutputLimits::minimum_response_bytes(Path::new("/tmp/aaaa")).unwrap();
    let escaped = ModelOutputLimits::minimum_response_bytes(Path::new("/tmp/\"\\\n\t")).unwrap();
    // Each of the four path characters acquires one extra JSON escape byte,
    // in each of the four channel paths.
    assert_eq!(escaped - plain, 4 * 4);
    let unicode = ModelOutputLimits::minimum_response_bytes(Path::new("/tmp/中😀")).unwrap();
    let same_byte_length =
        ModelOutputLimits::minimum_response_bytes(Path::new("/tmp/aaaaaaa")).unwrap();
    assert_eq!(unicode, same_byte_length);
}

#[test]
fn batch_dispatch_can_use_the_same_path_metadata_minimum() {
    let root = Path::new("/tmp/output");
    let minimum = ModelOutputLimits::minimum_response_bytes(root).unwrap();
    let limits = ModelOutputLimits {
        single_bytes: minimum,
        batch_bytes: minimum * 2,
    };
    limits.validate(root).unwrap();
    assert_eq!(limits.allocate(2, minimum).unwrap(), vec![minimum; 2]);
    assert!(limits.allocate(3, minimum).is_err());
}
