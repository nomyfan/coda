use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use coda_core::tool::{HostCallScope, HostToolCallError, HostToolInvoker};
use rquickjs::{AsyncContext, AsyncRuntime, Function, convert::List, function::Async};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub struct PtcLimits {
    pub source_bytes: usize,
    pub heap_bytes: usize,
    pub stack_bytes: usize,
    pub wall_time: Duration,
    pub join_grace: Duration,
    pub max_calls: usize,
    pub max_concurrent_calls: usize,
    pub result_bytes: usize,
    pub total_result_bytes: usize,
    pub state_bytes: usize,
    pub artifact_bytes: usize,
    pub stdout_bytes: usize,
    pub final_bytes: usize,
}

impl Default for PtcLimits {
    fn default() -> Self {
        Self {
            source_bytes: 256 * 1024,
            heap_bytes: 64 * 1024 * 1024,
            stack_bytes: 512 * 1024,
            wall_time: Duration::from_secs(120),
            join_grace: Duration::from_secs(1),
            max_calls: 128,
            max_concurrent_calls: 16,
            result_bytes: 4 * 1024 * 1024,
            total_result_bytes: 16 * 1024 * 1024,
            state_bytes: 4 * 1024 * 1024,
            artifact_bytes: 32 * 1024 * 1024,
            stdout_bytes: 1024 * 1024,
            final_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsRunReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsErrorReport>,
    pub stdout: String,
    pub stdout_truncated: bool,
    pub completed_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsErrorReport {
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum JsEngineError {
    Initialization(String),
    WorkerUnresponsive,
    Aborted(String),
}

impl Display for JsEngineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initialization(message) => {
                write!(f, "JavaScript initialization failed: {message}")
            }
            Self::WorkerUnresponsive => write!(
                f,
                "JavaScript worker did not stop within the teardown grace period"
            ),
            Self::Aborted(message) => write!(f, "JavaScript execution aborted: {message}"),
        }
    }
}

impl std::error::Error for JsEngineError {}

struct BridgeRequest {
    name: String,
    arguments: String,
    reply: oneshot::Sender<Result<String, BridgeCallError>>,
}

struct BridgeCallError {
    code: &'static str,
    message: String,
}

impl BridgeCallError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct WorkerInput {
    code: String,
    exposed_tools: Arc<[String]>,
    bridge_tx: mpsc::Sender<BridgeRequest>,
    stdout: Arc<Mutex<BoundedLog>>,
    interrupt: Arc<AtomicBool>,
    cancel: CancellationToken,
    outstanding_calls: Arc<AtomicUsize>,
    limits: PtcLimits,
}

struct OutstandingCall(Arc<AtomicUsize>);

impl OutstandingCall {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for OutstandingCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct BoundedLog {
    entries: VecDeque<String>,
    bytes: usize,
    limit: usize,
    truncated: bool,
}

impl BoundedLog {
    fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, mut entry: String) {
        if entry.len() > self.limit {
            let mut start = entry.len() - self.limit;
            while !entry.is_char_boundary(start) {
                start += 1;
            }
            entry = entry[start..].to_string();
            self.entries.clear();
            self.bytes = 0;
            self.truncated = true;
        }
        let extra = entry.len() + usize::from(!self.entries.is_empty());
        while self.bytes.saturating_add(extra) > self.limit {
            let Some(front) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(front.len());
            if !self.entries.is_empty() {
                self.bytes = self.bytes.saturating_sub(1);
            }
            self.truncated = true;
        }
        if !self.entries.is_empty() {
            self.bytes += 1;
        }
        self.bytes += entry.len();
        self.entries.push_back(entry);
    }

    fn snapshot(&self) -> (String, bool) {
        (
            self.entries.iter().cloned().collect::<Vec<_>>().join("\n"),
            self.truncated,
        )
    }
}

pub struct JsExecutor {
    limits: PtcLimits,
    workers: Arc<tokio::sync::Semaphore>,
}

impl JsExecutor {
    pub fn new(limits: PtcLimits) -> Self {
        static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
        Self {
            limits,
            workers: WORKERS
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
                .clone(),
        }
    }

    pub async fn run(
        &self,
        code: String,
        exposed_tools: Arc<[String]>,
        invoker: Arc<dyn HostToolInvoker>,
        scope: HostCallScope,
        cancel: CancellationToken,
    ) -> Result<JsRunReport, JsEngineError> {
        let deadline_at = tokio::time::Instant::now() + self.limits.wall_time;
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(JsEngineError::Aborted(
                    "cancelled while waiting for worker capacity".to_string(),
                ));
            }
            _ = tokio::time::sleep_until(deadline_at) => {
                return Ok(deadline_report());
            }
            permit = self.workers.clone().acquire_owned() => {
                permit.map_err(|_| JsEngineError::Initialization(
                    "worker semaphore closed".to_string(),
                ))?
            }
        };

        let interrupt = Arc::new(AtomicBool::new(false));
        let outstanding_calls = Arc::new(AtomicUsize::new(0));
        let script_cancel = cancel.child_token();
        let stdout = Arc::new(Mutex::new(BoundedLog::new(self.limits.stdout_bytes)));
        let (bridge_tx, mut bridge_rx) = mpsc::channel(self.limits.max_concurrent_calls);
        let (worker_tx, mut worker_rx) = oneshot::channel();
        let limits = self.limits;
        let worker_interrupt = interrupt.clone();
        let worker_cancel = script_cancel.clone();
        let worker_stdout = stdout.clone();
        let worker_outstanding_calls = outstanding_calls.clone();
        std::thread::Builder::new()
            .name("coda-ptc".to_string())
            .spawn(move || {
                let _permit = permit;
                let result = run_worker(WorkerInput {
                    code,
                    exposed_tools,
                    bridge_tx,
                    stdout: worker_stdout,
                    interrupt: worker_interrupt,
                    cancel: worker_cancel,
                    outstanding_calls: worker_outstanding_calls,
                    limits,
                });
                let _ = worker_tx.send(result);
            })
            .map_err(|error| JsEngineError::Initialization(error.to_string()))?;

        let host_limit = Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_calls));
        let mut host_calls = tokio::task::JoinSet::new();
        let mut started_calls = 0usize;
        let completed_calls = Arc::new(AtomicUsize::new(0));
        let total_result_bytes = Arc::new(Mutex::new(0usize));
        let deadline = tokio::time::sleep_until(deadline_at);
        tokio::pin!(deadline);

        enum StopReason {
            Aborted,
            Deadline,
        }
        let mut stop_reason = None;
        let mut worker_result = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    interrupt.store(true, Ordering::Release);
                    script_cancel.cancel();
                    stop_reason = Some(StopReason::Aborted);
                    break None;
                }
                _ = &mut deadline => {
                    interrupt.store(true, Ordering::Release);
                    script_cancel.cancel();
                    stop_reason = Some(StopReason::Deadline);
                    break None;
                }
                result = &mut worker_rx => {
                    break Some(result.unwrap_or_else(|_| Err(JsEngineError::Initialization("worker exited without a result".to_string()))));
                }
                Some(request) = bridge_rx.recv() => {
                    if started_calls >= limits.max_calls {
                        let _ = request.reply.send(Err(BridgeCallError::new(
                            "CALL_LIMIT",
                            "maximum host tool calls exceeded",
                        )));
                        continue;
                    }
                    started_calls += 1;
                    let invoker = invoker.clone();
                    let scope = scope.clone();
                    let host_limit = host_limit.clone();
                    let total_result_bytes = total_result_bytes.clone();
                    let completed_calls = completed_calls.clone();
                    let call_cancel = script_cancel.child_token();
                    host_calls.spawn(async move {
                        let Ok(_permit) = host_limit.acquire_owned().await else {
                            let _ = request.reply.send(Err(BridgeCallError::new(
                                "ABORTED",
                                "host executor closed",
                            )));
                            return;
                        };
                        let staged_call = scope.begin_tool_call(call_cancel);
                        let result = invoker
                            .call(request.name, request.arguments, staged_call.context())
                            .await;
                        let response = match result {
                            Ok(result) if result.output.len() > limits.result_bytes => {
                                Err(BridgeCallError::new(
                                    "RESULT_LIMIT",
                                    format!(
                                        "tool result exceeds {} bytes",
                                        limits.result_bytes
                                    ),
                                ))
                            }
                            Ok(result) => {
                                let mut total = total_result_bytes.lock().unwrap();
                                let next = total.saturating_add(result.output.len());
                                if next > limits.total_result_bytes {
                                    Err(BridgeCallError::new(
                                        "RESULT_LIMIT",
                                        format!(
                                            "cumulative tool results exceed {} bytes",
                                            limits.total_result_bytes
                                        ),
                                    ))
                                } else {
                                    *total = next;
                                    staged_call.commit();
                                    Ok(result.output)
                                }
                            }
                            Err(error) => Err(bridge_call_error(error)),
                        };
                        completed_calls.fetch_add(1, Ordering::AcqRel);
                        let _ = request.reply.send(response);
                    });
                }
                Some(joined) = host_calls.join_next(), if !host_calls.is_empty() => {
                    let _ = joined;
                }
            }
        };

        bridge_rx.close();
        script_cancel.cancel();
        host_calls.abort_all();
        while host_calls.join_next().await.is_some() {}

        if let Some(reason) = stop_reason {
            match tokio::time::timeout(limits.join_grace, &mut worker_rx).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    return Err(JsEngineError::Initialization(
                        "worker exited without a result".to_string(),
                    ));
                }
                Err(_) => {
                    tracing::error!(
                        grace_ms = limits.join_grace.as_millis() as u64,
                        "JavaScript worker did not stop; detaching while retaining its permit"
                    );
                    return match reason {
                        StopReason::Aborted => Err(JsEngineError::Aborted(
                            "cancelled, but the worker did not stop within the teardown grace period"
                                .to_string(),
                        )),
                        StopReason::Deadline => Err(JsEngineError::WorkerUnresponsive),
                    };
                }
            }
            if matches!(reason, StopReason::Aborted) {
                return Err(JsEngineError::Aborted("cancelled by caller".to_string()));
            }
            worker_result = Some(Ok(deadline_report()));
        }
        let mut report = worker_result.expect("worker result or stop reason must be present")?;
        let (captured, truncated) = stdout.lock().unwrap().snapshot();
        report.stdout = captured;
        report.stdout_truncated = truncated;
        report.completed_calls = completed_calls.load(Ordering::Acquire);
        Ok(report)
    }
}

fn bridge_call_error(error: HostToolCallError) -> BridgeCallError {
    match error {
        HostToolCallError::Unavailable => {
            BridgeCallError::new("TOOL_UNAVAILABLE", "tool is no longer permitted")
        }
        HostToolCallError::InvalidParameters(message) => {
            BridgeCallError::new("INVALID_PARAMETERS", message)
        }
        HostToolCallError::Execution(message) => BridgeCallError::new("TOOL_ERROR", message),
        HostToolCallError::ResourceLimit(message) => {
            BridgeCallError::new("RESOURCE_LIMIT", message)
        }
        HostToolCallError::Aborted(message) => BridgeCallError::new("ABORTED", message),
    }
}

fn deadline_report() -> JsRunReport {
    JsRunReport {
        ok: false,
        value: None,
        error: Some(JsErrorReport {
            code: "DEADLINE_EXCEEDED".to_string(),
            message: "JavaScript execution exceeded its wall-clock deadline".to_string(),
        }),
        stdout: String::new(),
        stdout_truncated: false,
        completed_calls: 0,
    }
}

fn unawaited_calls_report(count: usize) -> JsRunReport {
    JsRunReport {
        ok: false,
        value: None,
        error: Some(JsErrorReport {
            code: "UNAWAITED_TOOL_CALLS".to_string(),
            message: format!(
                "JavaScript returned with {count} unfinished tool call(s); await every tool Promise"
            ),
        }),
        stdout: String::new(),
        stdout_truncated: false,
        completed_calls: 0,
    }
}

fn run_worker(input: WorkerInput) -> Result<JsRunReport, JsEngineError> {
    let WorkerInput {
        code,
        exposed_tools,
        bridge_tx,
        stdout,
        interrupt,
        cancel,
        outstanding_calls,
        limits,
    } = input;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JsEngineError::Initialization(error.to_string()))?;
    runtime.block_on(async move {
        let js_runtime = AsyncRuntime::new().map_err(js_init)?;
        js_runtime.set_memory_limit(limits.heap_bytes).await;
        js_runtime.set_max_stack_size(limits.stack_bytes).await;
        js_runtime
            .set_interrupt_handler(Some(Box::new(move || interrupt.load(Ordering::Acquire))))
            .await;
        let context = AsyncContext::full(&js_runtime).await.map_err(js_init)?;
        context
            .async_with(async move |ctx| {
                let bridge_outstanding_calls = outstanding_calls.clone();
                let call = Function::new(
                    ctx.clone(),
                    Async(move |name: String, arguments: String| {
                        let bridge_tx = bridge_tx.clone();
                        let outstanding = OutstandingCall::new(bridge_outstanding_calls.clone());
                        async move {
                            let _outstanding = outstanding;
                            let (reply, response) = oneshot::channel();
                            if bridge_tx
                                .send(BridgeRequest {
                                    name,
                                    arguments,
                                    reply,
                                })
                                .await
                                .is_err()
                            {
                                return List((
                                    false,
                                    "ABORTED".to_string(),
                                    "host bridge closed".to_string(),
                                ));
                            }
                            match response.await {
                                Ok(Ok(output)) => List((true, output, String::new())),
                                Ok(Err(error)) => {
                                    List((false, error.code.to_string(), error.message))
                                }
                                Err(_) => List((
                                    false,
                                    "ABORTED".to_string(),
                                    "host response dropped".to_string(),
                                )),
                            }
                        }
                    }),
                )
                .map_err(js_init)?;
                ctx.globals()
                    .set("__coda_call_tool", call)
                    .map_err(js_init)?;
                let log = Function::new(ctx.clone(), move |line: String| {
                    stdout.lock().unwrap().push(line);
                })
                .map_err(js_init)?;
                ctx.globals().set("__coda_log", log).map_err(js_init)?;
                let names = serde_json::to_string(&*exposed_tools)
                    .map_err(|error| JsEngineError::Initialization(error.to_string()))?;
                ctx.globals()
                    .set("__coda_tool_names", names)
                    .map_err(js_init)?;
                ctx.eval::<(), _>(include_str!("bootstrap.js"))
                    .map_err(js_init)?;

                let source = wrap_source(&code);
                // The wrapper is itself an async IIFE, so evaluate it directly
                // as a Promise. `eval_promise` is for source containing raw
                // top-level await and would add a second wrapper here.
                let promise = match ctx.eval::<rquickjs::Promise<'_>, _>(source) {
                    Ok(promise) => promise,
                    Err(error) => return Ok(exception_report("SYNTAX_ERROR", error.to_string())),
                };
                tokio::select! {
                    result = promise.into_future::<String>() => {
                        let report = match result {
                            Ok(encoded) => decode_report(encoded, limits.final_bytes),
                            Err(error) => Ok(exception_report("JS_EXCEPTION", error.to_string())),
                        };
                        let unfinished = outstanding_calls.load(Ordering::Acquire);
                        if unfinished == 0 {
                            report
                        } else {
                            Ok(unawaited_calls_report(unfinished))
                        }
                    },
                    _ = cancel.cancelled() => Ok(deadline_report()),
                }
            })
            .await
    })
}

fn wrap_source(code: &str) -> String {
    format!(
        r#"
(async () => {{
  try {{
    const value = await (async () => {{
{code}
    }})();
    return JSON.stringify({{ ok: true, value: value === undefined ? null : value }});
  }} catch (error) {{
    return JSON.stringify({{
      ok: false,
      error: {{
        code: String(error && error.code || "JS_EXCEPTION"),
        message: String(error && error.message || error)
      }}
    }});
  }}
}})()
"#
    )
}

fn decode_report(encoded: String, limit: usize) -> Result<JsRunReport, JsEngineError> {
    if encoded.len() > limit {
        return Ok(exception_report(
            "OUTPUT_LIMIT",
            format!("final value exceeds {limit} bytes"),
        ));
    }
    #[derive(Deserialize)]
    struct WireReport {
        ok: bool,
        value: Option<serde_json::Value>,
        error: Option<JsErrorReport>,
    }
    let wire: WireReport = serde_json::from_str(&encoded).map_err(|error| {
        JsEngineError::Initialization(format!("invalid worker report: {error}"))
    })?;
    Ok(JsRunReport {
        ok: wire.ok,
        value: wire.value,
        error: wire.error,
        stdout: String::new(),
        stdout_truncated: false,
        completed_calls: 0,
    })
}

fn exception_report(code: &str, message: String) -> JsRunReport {
    JsRunReport {
        ok: false,
        value: None,
        error: Some(JsErrorReport {
            code: code.to_string(),
            message,
        }),
        stdout: String::new(),
        stdout_truncated: false,
        completed_calls: 0,
    }
}

fn js_init(error: rquickjs::Error) -> JsEngineError {
    JsEngineError::Initialization(error.to_string())
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
