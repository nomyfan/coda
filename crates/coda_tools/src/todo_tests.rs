use super::*;

use coda_core::tool::{ThreadState, ToolCallContext};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A thread that records what it is told, so a test can drive one tool's writes
/// into another tool's reads the way the runtime does.
#[derive(Default)]
struct FakeThread(Mutex<HashMap<String, serde_json::Value>>);

impl ThreadState for FakeThread {
    fn get(&self, kind: &str) -> Option<serde_json::Value> {
        self.0.lock().unwrap().get(kind).cloned()
    }
    fn set(&self, kind: &str, value: serde_json::Value) {
        self.0.lock().unwrap().insert(kind.to_string(), value);
    }
}

fn ctx(thread: &Arc<FakeThread>) -> ToolCallContext {
    ToolCallContext {
        state: thread.clone(),
        ..Default::default()
    }
}

fn params(titles: &[(&str, bool)]) -> WriteTodosParams {
    let todos: Vec<_> = titles
        .iter()
        .map(|(title, done)| serde_json::json!({ "title": title, "done": done }))
        .collect();
    serde_json::from_value(serde_json::json!({ "todos": todos })).unwrap()
}

async fn write(thread: &Arc<FakeThread>, titles: &[(&str, bool)]) -> String {
    WriteTodosTool::new()
        .execute(params(titles), ctx(thread))
        .await
        .unwrap()
}

async fn read(thread: &Arc<FakeThread>) -> String {
    ReadTodosTool::new()
        .execute(ReadTodosParams {}, ctx(thread))
        .await
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_thread_that_never_wrote_todos_reads_empty() {
    assert_eq!(read(&Arc::default()).await, "No todos.");
}

#[tokio::test]
async fn a_write_is_read_back_through_the_thread() {
    let thread = Arc::<FakeThread>::default();
    assert_eq!(
        write(&thread, &[("parse", true), ("test", false)]).await,
        "Todos updated. 2 items."
    );
    assert_eq!(read(&thread).await, "1. [x] parse\n2. [ ] test\n");
}

#[tokio::test]
async fn a_later_write_replaces_the_list_wholesale() {
    // Every write records a complete value, which is what lets the runtime
    // collapse a range of them by keeping the last — see `ThreadState`.
    let thread = Arc::<FakeThread>::default();
    write(&thread, &[("parse", false), ("test", false)]).await;
    write(&thread, &[("ship", true)]).await;

    assert_eq!(read(&thread).await, "1. [x] ship\n");
}

#[tokio::test]
async fn an_empty_write_clears_the_list() {
    let thread = Arc::<FakeThread>::default();
    write(&thread, &[("parse", false)]).await;
    write(&thread, &[]).await;

    assert_eq!(
        read(&thread).await,
        "No todos.",
        "clearing the list is a write like any other, not an absence of one"
    );
}

#[tokio::test]
async fn two_threads_of_one_agent_do_not_see_each_other() {
    // The reason neither tool keeps a store: one `Agent` serves many threads —
    // every stateless sub-agent call is a new one — and a store on the tool
    // leaked between them. State reached through the call's context cannot.
    let theirs = Arc::<FakeThread>::default();
    let ours = Arc::<FakeThread>::default();
    write(&theirs, &[("theirs", false)]).await;

    assert_eq!(read(&theirs).await, "1. [ ] theirs\n");
    assert_eq!(read(&ours).await, "No todos.");
}

#[tokio::test]
async fn the_tools_hold_nothing_between_calls() {
    // One tool instance is shared by every call an agent makes, so anything it
    // kept would outlive the thread it was kept for.
    let write_tool = WriteTodosTool::new();
    let read_tool = ReadTodosTool::new();
    let first = Arc::<FakeThread>::default();

    write_tool
        .execute(params(&[("only here", false)]), ctx(&first))
        .await
        .unwrap();

    let second = Arc::<FakeThread>::default();
    let leaked = read_tool
        .execute(ReadTodosParams {}, ctx(&second))
        .await
        .unwrap();

    assert_eq!(leaked.to_string(), "No todos.");
}

#[tokio::test]
async fn an_unreadable_stored_value_reads_as_empty() {
    // The kind is opaque to everything that stores it, so nothing on the way in
    // or out validates the shape. A thread whose value cannot be read back still
    // loads; it does not fail the turn.
    let thread = Arc::<FakeThread>::default();
    thread.set("todos", serde_json::json!({ "not": "a list" }));

    assert_eq!(read(&thread).await, "No todos.");
}
