use coda_agent::{PendingApproval, ResumeDecision, ToolCallResolution};
use coda_core::llm::{Message as LlmMessage, ToolCall, ToolCallOutcome, ToolOutput};
use coda_server::{
    ask_user::AskUserParams,
    config::ToolApprovalConfig,
    parse_session_id_arg, print_logo,
    transport::{Transport, WebSocketClientTransport},
    wire::{AbortedTargetWire, ClientMessage, ServerMessage, WireEvent},
};
use rustyline::error::ReadlineError;
use std::io::{self, Write};
use uuid::Uuid;

/// The root agent is always named "coda" (see `agents::ROOT_AGENT_NAME`). The
/// snapshot does not carry the name, so the debug client hardcodes it for
/// rendering.
const ROOT_NAME: &str = "coda";

fn render_message(message: &LlmMessage) {
    match message {
        LlmMessage::User(msg) => println!("You: {}", msg.0),
        LlmMessage::Assistant(msg) => {
            if !msg.content.is_empty() {
                println!("Assistant: {}", msg.content);
            }
            for call in &msg.tool_calls {
                println!(
                    "[Tool: {}]: {}",
                    call.name,
                    call.arguments.as_deref().unwrap_or("{}")
                );
            }
        }
        LlmMessage::Tool(msg) => {
            let status = match msg.outcome {
                ToolCallOutcome::Auto => "auto",
                ToolCallOutcome::Approved => "approved",
                ToolCallOutcome::Resolved => "resolved",
                ToolCallOutcome::Rejected { .. } => "rejected",
                ToolCallOutcome::Aborted => "aborted",
            };
            match &msg.output {
                ToolOutput::Ok(output) => {
                    println!("[Tool Result: {}][{status}] {output}", msg.name)
                }
                ToolOutput::Err(output) => {
                    println!("[Tool Error: {}][{status}] {output}", msg.name)
                }
            }
        }
        _ => {}
    }
}

fn render_history(messages: &[LlmMessage]) {
    if messages.is_empty() {
        return;
    }
    println!("Resumed conversation:\n");
    for msg in messages {
        render_message(msg);
    }
    println!();
}

fn render_event(event: &WireEvent, root_name: &str) {
    match event {
        WireEvent::LlmContentChunk {
            content,
            agent_name,
            ..
        } => {
            if agent_name == root_name {
                print!("{content}");
            } else {
                print!("\x1b[36m{content}\x1b[0m");
            }
            io::stdout().flush().ok();
        }
        WireEvent::LlmEnd {
            message,
            agent_name,
            ..
        } => {
            if !message.content.is_empty() || message.usage.is_some() {
                println!();
            }
            if let Some(usage) = &message.usage {
                let total = usage.prompt_tokens + usage.completion_tokens;
                let label = if agent_name == root_name {
                    String::new()
                } else {
                    format!(": {agent_name}")
                };
                println!(
                    "[Token Usage{label}] prompt: {} | completion: {} | total: {total}",
                    usage.prompt_tokens, usage.completion_tokens
                );
            }
            if message.aborted {
                println!("[Generation interrupted]");
            }
        }
        WireEvent::ToolCallStart {
            call, agent_name, ..
        } => {
            if agent_name == root_name {
                println!(
                    "\n[Tool: {}]: {}",
                    call.name,
                    call.arguments.as_deref().unwrap_or("{}")
                );
            } else {
                println!(
                    "\n\x1b[36m[Sub-agent {agent_name}: {}]: {}\x1b[0m",
                    call.name,
                    call.arguments.as_deref().unwrap_or("{}")
                );
            }
        }
        WireEvent::ToolCallEnd { message, .. } => {
            let status = match message.outcome {
                ToolCallOutcome::Auto => "auto",
                ToolCallOutcome::Approved => "approved",
                ToolCallOutcome::Resolved => "resolved",
                ToolCallOutcome::Rejected { .. } => "rejected",
                ToolCallOutcome::Aborted => "aborted",
            };
            match &message.output {
                ToolOutput::Ok(output) => {
                    println!("[Tool Result: {}][{status}] {output}", message.name)
                }
                ToolOutput::Err(output) => {
                    println!("[Tool Error: {}][{status}] {output}", message.name)
                }
            }
        }
        WireEvent::Suspended { approval, .. } => {
            println!(
                "\n[{} tool call(s) from '{}' require approval]",
                approval.calls.len(),
                approval.agent_name
            );
        }
        WireEvent::Error { message, .. } => {
            println!("\n[Error] {message}");
        }
        WireEvent::Aborted { target, .. } => match target {
            AbortedTargetWire::Generation => {
                println!("\n\n[Aborted: generation interrupted]");
            }
            AbortedTargetWire::ToolCalls { call_ids } => {
                println!("\n[Aborted: {} tool call(s) interrupted]", call_ids.len());
            }
        },
        _ => {}
    }
}

fn prompt_ask_user(
    rl: &mut rustyline::DefaultEditor,
    question: &str,
    options: &[String],
) -> Result<String, Box<dyn std::error::Error>> {
    println!("\n[User Input Required]\n");
    println!("{}\n", question);
    for (i, opt) in options.iter().enumerate() {
        println!("  {}. {}", i + 1, opt);
    }
    println!("  0. Other (type your own response)\n");

    loop {
        let input = rl.readline("> ")?;
        let input = input.trim().to_string();

        if input == "0" {
            let custom = rl.readline("Your response: ")?;
            return Ok(custom.trim().to_string());
        }
        if let Ok(idx) = input.parse::<usize>()
            && idx >= 1
            && idx <= options.len()
        {
            return Ok(options[idx - 1].clone());
        }
        println!("  Please enter a number between 0 and {}.", options.len());
    }
}

enum ApprovalResult {
    Approve(String),
    Reject(String, Option<String>),
    AlwaysAllow,
}

fn prompt_approval(
    rl: &mut rustyline::DefaultEditor,
    call: &ToolCall,
) -> Result<ApprovalResult, Box<dyn std::error::Error>> {
    println!(
        "  {}: {}",
        call.name,
        call.arguments.as_deref().unwrap_or("{}")
    );

    let prompt = if call.name == "shell" {
        "     Approve? [y/n/a(lways)]: "
    } else {
        "     Approve? [y/n]: "
    };

    loop {
        let input = rl.readline(prompt)?;

        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                return Ok(ApprovalResult::Approve(call.id.clone()));
            }
            "n" | "no" => {
                let reason = rl.readline("     Reason (optional, Enter to skip): ")?;
                let reason = reason.trim().to_string();
                return Ok(ApprovalResult::Reject(
                    call.id.clone(),
                    if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    },
                ));
            }
            "a" | "always" if call.name == "shell" => {
                return Ok(ApprovalResult::AlwaysAllow);
            }
            _ => println!(
                "     Please enter 'y' or 'n'{}",
                if call.name == "shell" { " or 'a'" } else { "" }
            ),
        }
    }
}

fn extract_shell_command(call: &ToolCall) -> String {
    let args = call.arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v["command"].as_str().map(String::from))
        .unwrap_or_default()
}

async fn send_client(
    transport: &WebSocketClientTransport,
    msg: &ClientMessage,
) -> Result<(), Box<dyn std::error::Error>> {
    if transport.send(msg).await {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "connection closed while sending client message",
        )
        .into())
    }
}

async fn add_allow_pattern(
    transport: &WebSocketClientTransport,
    workspace_id: &str,
    pattern: String,
    root_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    send_client(
        transport,
        &ClientMessage::AddAllowPattern {
            workspace_id: workspace_id.to_string(),
            pattern: pattern.clone(),
        },
    )
    .await?;

    loop {
        match transport.recv().await {
            Some(ServerMessage::AllowPatternResult {
                pattern: returned,
                error,
                ..
            }) if returned == pattern => {
                return match error {
                    Some(error) => Err(std::io::Error::other(error).into()),
                    None => Ok(()),
                };
            }
            Some(ServerMessage::AllowPatternResult {
                error: Some(error), ..
            }) => {
                println!("\n[Error] {error}");
            }
            Some(ServerMessage::AllowPatternResult { .. }) => {}
            Some(ServerMessage::Event { event, .. }) => render_event(&event, root_name),
            Some(ServerMessage::Snapshot { .. }) => {}
            Some(ServerMessage::WorkspaceCatalog { .. }) => {}
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "connection closed while waiting for allow pattern result",
                )
                .into());
            }
        }
    }
}

/// Prompt the user to resolve every call in `approval`, then send a single
/// `Resume` covering them. `AlwaysAllow` additionally pushes the derived allow
/// pattern to the server.
async fn resolve_and_resume(
    rl: &mut rustyline::DefaultEditor,
    transport: &WebSocketClientTransport,
    workspace_id: &str,
    session_id: &str,
    approval: &PendingApproval,
    root_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut resolutions = vec![];
    for call in &approval.calls {
        if call.name == "ask_user" {
            let output = match serde_json::from_str::<AskUserParams>(
                call.arguments.as_deref().unwrap_or("{}"),
            ) {
                Ok(params) => {
                    ToolOutput::Ok(prompt_ask_user(rl, &params.question, &params.options)?)
                }
                Err(err) => ToolOutput::Err(format!("Invalid ask_user arguments: {err}")),
            };
            resolutions.push((call.id.clone(), ToolCallResolution::Resolved(output)));
            continue;
        }
        loop {
            match prompt_approval(rl, call)? {
                ApprovalResult::Approve(id) => {
                    resolutions.push((id, ToolCallResolution::Execute));
                    break;
                }
                ApprovalResult::Reject(id, reason) => {
                    resolutions.push((id, ToolCallResolution::Rejected { reason }));
                    break;
                }
                ApprovalResult::AlwaysAllow => {
                    let command = extract_shell_command(call);
                    let suggested = ToolApprovalConfig::derive_pattern(&command);
                    match rl.readline(&format!(
                        "     Allow pattern [{suggested}] (Ctrl+C to cancel): "
                    )) {
                        Ok(input) => {
                            let pattern = if input.trim().is_empty() {
                                suggested
                            } else {
                                input.trim().to_string()
                            };
                            match add_allow_pattern(
                                transport,
                                workspace_id,
                                pattern.clone(),
                                root_name,
                            )
                            .await
                            {
                                Ok(()) => {
                                    println!("     Added allow pattern: {pattern}");
                                    resolutions
                                        .push((call.id.clone(), ToolCallResolution::Execute));
                                    break;
                                }
                                Err(err) => {
                                    println!("     Failed to add allow pattern: {err}");
                                    continue;
                                }
                            }
                        }
                        Err(ReadlineError::Interrupted) => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
    }

    send_client(
        transport,
        &ClientMessage::Resume {
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            agent_name: approval.agent_name.clone(),
            thread_id: approval.thread_id.clone(),
            decision: ResumeDecision { resolutions },
        },
    )
    .await?;
    Ok(())
}

/// Outcome of consuming the event stream up to a settling point.
enum TurnState {
    /// The root agent finished; return to the prompt.
    Done,
    /// The connection closed.
    Closed,
}

/// Render events until the root agent's turn settles, resolving suspensions and
/// forwarding Ctrl+C as an `Abort` along the way.
async fn run_turn(
    rl: &mut rustyline::DefaultEditor,
    transport: &WebSocketClientTransport,
    workspace_id: &str,
    session_id: &str,
    root_name: &str,
) -> Result<TurnState, Box<dyn std::error::Error>> {
    loop {
        let msg = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                send_client(
                    transport,
                    &ClientMessage::Abort {
                        workspace_id: workspace_id.to_string(),
                        session_id: session_id.to_string(),
                    },
                ).await?;
                continue;
            }
            msg = transport.recv() => msg,
        };

        let event = match msg {
            Some(ServerMessage::Event { event, .. }) => event,
            Some(ServerMessage::Snapshot { .. }) => continue, // only sent once, at connect
            Some(ServerMessage::AllowPatternResult {
                error: Some(error), ..
            }) => {
                println!("\n[Error] {error}");
                continue;
            }
            Some(ServerMessage::AllowPatternResult { .. }) => continue,
            Some(ServerMessage::WorkspaceCatalog { .. }) => continue,
            None => return Ok(TurnState::Closed),
        };
        render_event(&event, root_name);

        match &event {
            WireEvent::Suspended { approval, .. } => {
                resolve_and_resume(rl, transport, workspace_id, session_id, approval, root_name)
                    .await?;
            }
            WireEvent::LlmEnd {
                message,
                agent_name,
                ..
            } if agent_name == root_name && message.tool_calls.is_empty() => {
                return Ok(TurnState::Done);
            }
            WireEvent::Aborted { agent_name, .. } if agent_name == root_name => {
                return Ok(TurnState::Done);
            }
            WireEvent::Error { agent_name, .. } if agent_name == root_name => {
                return Ok(TurnState::Done);
            }
            _ => {}
        }
    }
}

fn print_usage() {
    println!("Usage: coda-client [--resume <uuid>]");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server_url =
        std::env::var("CODA_SERVER_URL").unwrap_or_else(|_| "ws://127.0.0.1:3000".to_string());
    let workspace_id = std::env::var("CODA_WORKSPACE_ID").unwrap_or_else(|_| "coda".to_string());

    let session_id = match parse_session_id_arg(std::env::args().skip(1)) {
        Ok(Some(id)) => id,
        Ok(None) => Uuid::new_v4().to_string(),
        Err(err) if err.is_empty() => {
            print_usage();
            return Ok(());
        }
        Err(err) => {
            eprintln!("Error: {err}");
            print_usage();
            return Ok(());
        }
    };

    print_logo("An AI Agent (client)");
    println!("Type 'quit', 'exit', or 'q' to stop\n");
    println!("Session id: {session_id}\n");
    println!("Workspace: {workspace_id}\n");
    println!("Server: {server_url}\n");

    let url = format!("{server_url}/ws");
    let transport = WebSocketClientTransport::connect(&url).await?;
    send_client(
        &transport,
        &ClientMessage::OpenSession {
            workspace_id: workspace_id.clone(),
            session_id: session_id.clone(),
        },
    )
    .await?;

    let mut rl = rustyline::DefaultEditor::new()?;

    // The server's first message is always the snapshot: prior history plus any
    // approvals left pending from a previous suspension.
    match transport.recv().await {
        Some(ServerMessage::WorkspaceCatalog { .. }) => match transport.recv().await {
            Some(ServerMessage::Snapshot {
                messages,
                pending_approvals,
                ..
            }) => {
                render_history(&messages);
                if !pending_approvals.is_empty() {
                    println!(
                        "[Resuming session: {} pending approval(s) to resolve]\n",
                        pending_approvals.len()
                    );
                    for approval in &pending_approvals {
                        resolve_and_resume(
                            &mut rl,
                            &transport,
                            &workspace_id,
                            &session_id,
                            approval,
                            ROOT_NAME,
                        )
                        .await?;
                    }
                    if let TurnState::Closed =
                        run_turn(&mut rl, &transport, &workspace_id, &session_id, ROOT_NAME).await?
                    {
                        println!("\nConnection closed by server.");
                        return Ok(());
                    }
                    println!();
                }
            }
            Some(ServerMessage::Event { event, .. }) => render_event(&event, ROOT_NAME),
            Some(ServerMessage::AllowPatternResult {
                error: Some(error), ..
            }) => {
                eprintln!("Unexpected allow pattern result before snapshot: {error}");
            }
            Some(ServerMessage::AllowPatternResult { .. }) => {
                eprintln!("Unexpected allow pattern result before snapshot.");
            }
            Some(ServerMessage::WorkspaceCatalog { .. }) => {}
            None => {
                eprintln!("Connection closed before snapshot.");
                return Ok(());
            }
        },
        Some(ServerMessage::Snapshot {
            messages,
            pending_approvals,
            ..
        }) => {
            render_history(&messages);
            if !pending_approvals.is_empty() {
                println!(
                    "[Resuming session: {} pending approval(s) to resolve]\n",
                    pending_approvals.len()
                );
                for approval in &pending_approvals {
                    resolve_and_resume(
                        &mut rl,
                        &transport,
                        &workspace_id,
                        &session_id,
                        approval,
                        ROOT_NAME,
                    )
                    .await?;
                }
                // The session is now resuming; render until it settles.
                if let TurnState::Closed =
                    run_turn(&mut rl, &transport, &workspace_id, &session_id, ROOT_NAME).await?
                {
                    println!("\nConnection closed by server.");
                    return Ok(());
                }
                println!();
            }
        }
        Some(ServerMessage::Event { event, .. }) => render_event(&event, ROOT_NAME),
        Some(ServerMessage::AllowPatternResult {
            error: Some(error), ..
        }) => {
            eprintln!("Unexpected allow pattern result before snapshot: {error}");
        }
        Some(ServerMessage::AllowPatternResult { .. }) => {
            eprintln!("Unexpected allow pattern result before snapshot.");
        }
        None => {
            eprintln!("Connection closed before snapshot.");
            return Ok(());
        }
    }

    loop {
        let input = match rl.readline("You: ") {
            Ok(line) => line,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => {
                println!("Goodbye!");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input.eq_ignore_ascii_case("quit")
            || input.eq_ignore_ascii_case("exit")
            || input.eq_ignore_ascii_case("q")
        {
            println!("Goodbye!");
            break;
        }

        send_client(
            &transport,
            &ClientMessage::Task {
                workspace_id: workspace_id.clone(),
                session_id: session_id.clone(),
                task: input.to_string(),
            },
        )
        .await?;

        print!("Assistant: ");
        io::stdout().flush()?;

        match run_turn(&mut rl, &transport, &workspace_id, &session_id, ROOT_NAME).await? {
            TurnState::Done => println!(),
            TurnState::Closed => {
                println!("\nConnection closed by server.");
                break;
            }
        }
    }

    println!("Session id: {session_id}");
    println!("You can resume this session with: coda-client --resume {session_id}");

    Ok(())
}
