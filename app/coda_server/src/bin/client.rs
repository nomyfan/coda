use coda_agent::{ResumeDecision, ToolCallResolution};
use coda_core::llm::{Message, ToolCall, ToolCallOutcome, ToolOutput};
use coda_server::{
    ask_user::AskUserParams,
    config::ToolApprovalConfig,
    parse_session_id_arg, print_logo,
    wire::{
        AbortedTargetWire, AddAllowPatternRequest, ChatRequest, ChatResponse, ChatStatus,
        HistoryResponse, WireEvent,
    },
};
use rustyline::error::ReadlineError;
use std::collections::HashMap;
use std::io::{self, Write};
use uuid::Uuid;

fn render_message(message: &Message) {
    match message {
        Message::User(msg) => println!("You: {}", msg.0),
        Message::Assistant(msg) => {
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
        Message::Tool(msg) => {
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

fn render_history(messages: &[Message]) {
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

async fn resolve_pending_calls(
    rl: &mut rustyline::DefaultEditor,
    pending_calls: &[ToolCall],
    client: &reqwest::Client,
    server_url: &str,
) -> Result<Vec<(String, ToolCallResolution)>, Box<dyn std::error::Error>> {
    let mut resolutions = vec![];
    for pending_call in pending_calls {
        if pending_call.name == "ask_user" {
            let output = match serde_json::from_str::<AskUserParams>(
                pending_call.arguments.as_deref().unwrap_or("{}"),
            ) {
                Ok(params) => {
                    ToolOutput::Ok(prompt_ask_user(rl, &params.question, &params.options)?)
                }
                Err(err) => ToolOutput::Err(format!("Invalid ask_user arguments: {err}")),
            };
            resolutions.push((
                pending_call.id.clone(),
                ToolCallResolution::Resolved(output),
            ));
            continue;
        }
        loop {
            match prompt_approval(rl, pending_call)? {
                ApprovalResult::Approve(id) => {
                    resolutions.push((id, ToolCallResolution::Execute));
                    break;
                }
                ApprovalResult::Reject(id, reason) => {
                    resolutions.push((id, ToolCallResolution::Rejected { reason }));
                    break;
                }
                ApprovalResult::AlwaysAllow => {
                    let command = extract_shell_command(pending_call);
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
                            match client
                                .post(format!("{server_url}/permissions/allow"))
                                .json(&AddAllowPatternRequest {
                                    pattern: pattern.clone(),
                                })
                                .send()
                                .await
                            {
                                Ok(resp) if resp.status().is_success() => {
                                    println!("     Added allow pattern: {pattern}");
                                }
                                Ok(resp) => {
                                    let body = resp.text().await.unwrap_or_default();
                                    eprintln!("     Failed to save pattern: {body}");
                                }
                                Err(e) => {
                                    eprintln!("     Failed to save pattern: {e}");
                                }
                            }
                            resolutions
                                .push((pending_call.id.clone(), ToolCallResolution::Execute));
                            break;
                        }
                        Err(ReadlineError::Interrupted) => continue,
                        Err(e) => return Err(Box::new(e)),
                    }
                }
            }
        }
    }
    Ok(resolutions)
}

fn print_usage() {
    println!("Usage: coda-client [--resume <uuid>]");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server_url =
        std::env::var("CODA_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());

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
    println!("Server: {server_url}\n");

    let mut rl = rustyline::DefaultEditor::new()?;
    let client = reqwest::Client::new();

    let mut pending_decisions: HashMap<String, ResumeDecision> = HashMap::new();

    if let Ok(resp) = client
        .get(format!("{server_url}/history/{session_id}"))
        .send()
        .await
        && let Ok(history) = resp.json::<HistoryResponse>().await
    {
        render_history(&history.messages);
        if !history.pending_approvals.is_empty() {
            println!(
                "\n[Resuming session — {} pending approval(s) to resolve]\n",
                history.pending_approvals.len()
            );
            for p in &history.pending_approvals {
                println!(
                    "[Approval needed: {} call(s) from '{}']",
                    p.calls.len(),
                    p.agent_name
                );
                let resolutions =
                    resolve_pending_calls(&mut rl, &p.calls, &client, &server_url).await?;
                pending_decisions.insert(p.thread_id.clone(), ResumeDecision { resolutions });
            }
            println!();
        }
    }

    loop {
        let (task, resume_decisions) = if pending_decisions.is_empty() {
            let input = match rl.readline("You: ") {
                Ok(line) => line,
                Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => {
                    println!("Goodbye!");
                    break;
                }
                Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error>),
            };
            let input = input.trim().to_string();

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

            (Some(input), HashMap::new())
        } else {
            (None, std::mem::take(&mut pending_decisions))
        };

        let req = ChatRequest {
            session_id: session_id.clone(),
            task,
            resume_decisions,
        };

        let resp = client
            .post(format!("{server_url}/chat"))
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            eprintln!(
                "Server error: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
            pending_decisions.clear();
            continue;
        }

        let chat_resp: ChatResponse = resp.json().await?;

        for event in &chat_resp.events {
            render_event(event, "coda");
        }

        match chat_resp.status {
            ChatStatus::Done => {
                pending_decisions.clear();
                println!();
            }
            ChatStatus::PendingApproval => {
                println!();
                for p in &chat_resp.pending_approvals {
                    println!(
                        "[Approval needed: {} call(s) from '{}']",
                        p.calls.len(),
                        p.agent_name
                    );
                    let resolutions =
                        resolve_pending_calls(&mut rl, &p.calls, &client, &server_url).await?;
                    pending_decisions.insert(p.thread_id.clone(), ResumeDecision { resolutions });
                }
                println!();
            }
            ChatStatus::Error(msg) => {
                eprintln!("Error: {msg}");
                pending_decisions.clear();
                println!();
            }
        }
    }

    println!("Session id: {session_id}");
    println!("You can resume this session with: coda-client --resume {session_id}");

    Ok(())
}
