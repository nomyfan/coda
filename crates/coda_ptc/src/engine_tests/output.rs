use super::*;

#[test]
fn failed_js_conversion_discards_state_and_read_receipts() {
    use coda_core::output::{Channel, ReadReceipt};
    use rquickjs::IntoJs;
    let state = Arc::new(RecordingState::default());
    let outer = ToolCallContext::new(CancellationToken::new(), state.clone());
    let scope = HostCallScope::new(
        outer.clone(),
        HostEffectLimits {
            state_bytes: MIB,
            artifact_bytes: MIB,
        },
    );
    let staged = scope.begin_tool_call(CancellationToken::new());
    let ctx = staged.context();
    ctx.state.set("effect", serde_json::json!(true)).unwrap();
    ctx.record_reads(vec![ReadReceipt {
        consumer: "root".into(),
        task: coda_core::task::TaskId::new(),
        channel: Channel::Stdout,
        start: 0,
        end: 10,
        total: 10,
        terminal: true,
        complete: true,
    }]);
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    runtime.set_memory_limit(256 * KIB);
    context.with(|ctx| {
        assert!(
            BridgeResponse(Ok(Delivery {
                result: HostToolCallResult {
                    output: "x".repeat(MIB),
                    buffer_lease: None
                },
                staged,
            }))
            .into_js(&ctx)
            .is_err()
        );
        ctx.catch();
    });
    scope.commit_into_outer().unwrap();
    assert!(state.get("effect").is_none());
    assert!(outer.take_reads().is_empty());
}

#[tokio::test]
async fn intermediate_results_have_no_single_or_cumulative_flow_limit() {
    for (bytes, calls) in [(1024 * 1024, 100), (5 * 1024 * 1024, 1)] {
        let invoker = Arc::new(RawInvoker { bytes });
        let result = JsExecutor::new(PtcLimits::default()).run(format!("let n=0; for(let i=0;i<{calls};i++) {{ const s=await tools.read_file({{}}); if(s.length!=={bytes} || s[0]!== 'x' || s[s.length-1]!=='x') throw Error('changed data'); n+=s.length; }} return n;"), invoker.exposed_tools(), invoker, scope(), CancellationToken::new(), None).await.unwrap();
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.value, Some(serde_json::json!(bytes * calls)));
    }
}

struct LogPressureInvoker {
    reader: Arc<dyn coda_core::output::OutputReader>,
    observed_progress: Arc<AtomicBool>,
}
impl HostToolInvoker for LogPressureInvoker {
    fn exposed_tools(&self) -> Arc<[String]> {
        Arc::from(vec!["read_file".into(), "ls".into()])
    }
    fn call(
        &self,
        name: String,
        _: String,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>> {
        let reader = self.reader.clone();
        let progress = self.observed_progress.clone();
        Box::pin(async move {
            if name == "ls" {
                return Ok(HostToolCallResult {
                    output: "ready".into(),
                    buffer_lease: None,
                });
            }
            let CapturePurpose::Programmatic(budget) = ctx.output_purpose else {
                panic!()
            };
            let captured = || {
                reader
                    .snapshot()
                    .channels
                    .iter()
                    .find(|c| c.channel == coda_core::output::Channel::Log)
                    .unwrap()
                    .captured
            };
            // Wait for JS to enter its synchronous logger, after the ready Promise.
            while captured() == 0 {
                tokio::task::yield_now().await;
            }
            let lease = budget
                .reserve(budget.capacity(), &ctx.cancel)
                .await
                .unwrap();
            let start = captured();
            let monitor_budget = budget.clone();
            tokio::spawn(async move {
                while monitor_budget.available() == 0 {
                    let now = reader
                        .snapshot()
                        .channels
                        .iter()
                        .find(|c| c.channel == coda_core::output::Channel::Log)
                        .unwrap()
                        .captured;
                    if now >= start + 128 * 1024 {
                        progress.store(true, Ordering::Release);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            });
            Ok(HostToolCallResult {
                output: "x".repeat(budget.capacity() / 2),
                buffer_lease: Some(lease),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronous_logs_drain_while_pending_delivery_holds_all_non_log_memory() {
    use coda_core::output::{Channel, OutputData, OutputStore};
    for many in [false, true] {
        let store = coda_output::Store::standalone();
        let capture = store
            .begin(
                ToolCallContext::default().output_owner,
                vec![Channel::Result, Channel::Log],
                CapturePurpose::ModelResult,
            )
            .await
            .unwrap();
        let reader = capture.reader();
        let logs = coda_output::log::LogCollector::start(
            capture,
            8192,
            CancellationToken::new(),
            std::time::Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        let progressed = Arc::new(AtomicBool::new(false));
        let invoker = Arc::new(LogPressureInvoker {
            reader,
            observed_progress: progressed.clone(),
        });
        let limits = PtcLimits {
            host_buffer_bytes: 4 * MIB,
            ..PtcLimits::default()
        };
        let logging = if many {
            "for(let i=0;i<256;i++) console.log('L'.repeat(8192));"
        } else {
            "console.log('L'.repeat(2*1024*1024));"
        };
        let result = tokio::time::timeout(Duration::from_secs(10), JsExecutor::new(limits).run(format!("const pending = tools.read_file({{}}); await tools.ls({{}}); {logging} return (await pending).length;"), invoker.exposed_tools(), invoker, scope(), CancellationToken::new(), Some(logs.writer()))).await.unwrap().unwrap();
        assert!(result.ok, "{:?}", result.error);
        assert!(
            progressed.load(Ordering::Acquire),
            "log IO did not progress while result delivery held the budget"
        );
        let OutputData::Captured(output) = logs.finish("done").await else {
            panic!()
        };
        let reference = output.reference.unwrap();
        assert!(reference.complete);
        let log = reference
            .channels
            .iter()
            .find(|c| c.channel == Channel::Log)
            .unwrap();
        let content = std::fs::read(&log.path).unwrap();
        assert_eq!(
            content.iter().filter(|&&b| b == b'L').count(),
            2 * 1024 * 1024
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_preserves_explicit_logs_without_archiving_intermediates() {
    use coda_core::output::{Channel, OutputData, OutputStore};
    let store = coda_output::Store::standalone();
    let capture = store
        .begin(
            ToolCallContext::default().output_owner,
            vec![Channel::Result, Channel::Log],
            CapturePurpose::ModelResult,
        )
        .await
        .unwrap();
    let reader = capture.reader();
    let cancel = CancellationToken::new();
    let logs = coda_output::log::LogCollector::start(
        capture,
        8192,
        cancel.clone(),
        std::time::Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    let invoker = Arc::new(FakeInvoker::new(&["read_file"]));
    let run_cancel = cancel.clone();
    let writer = logs.writer();
    let run = tokio::spawn(async move {
        JsExecutor::new(PtcLimits::default()).run(
            "await tools.read_file({}); console.log('a'.repeat(300000) + 'middle-log' + 'z'.repeat(300000)); await new Promise(() => {});".into(),
            invoker.exposed_tools(), invoker, scope(), run_cancel, Some(writer)).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while reader
            .snapshot()
            .channels
            .iter()
            .find(|c| c.channel == Channel::Log)
            .unwrap()
            .captured
            < 600011
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    cancel.cancel();
    assert!(matches!(run.await.unwrap(), Err(JsEngineError::Aborted(_))));
    let OutputData::Captured(output) = logs.finish(r#"{"ok":false,"error":"ABORTED"}"#).await
    else {
        panic!()
    };
    let reference = output.reference.unwrap();
    assert!(reference.complete);
    assert_eq!(reference.channels.len(), 2);
    let path = &reference
        .channels
        .iter()
        .find(|c| c.channel == Channel::Log)
        .unwrap()
        .path;
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("middle-log")
    );
}
