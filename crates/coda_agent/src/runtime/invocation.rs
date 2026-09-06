use super::*;
use crate::{SubAgentMode, agent::Receiver};
use coda_core::{llm::MessageOrigin, task::TaskId};

pub(super) struct SubagentInvocation {
    pub tool_name: String,
    pub origin: MessageOrigin,
    pub turn_id: TurnId,
    pub task: String,
    pub run_in_background: bool,
}

pub(super) enum InvocationReceipt {
    Foreground { envelope_id: String },
    Background(TaskId),
}

impl ProcessRuntime {
    /// Submit one call after the driver's whole-batch stateful preflight.
    /// Instance selection is independent of whether its execution is backgrounded.
    pub(super) async fn invoke(
        &self,
        caller: &ProcessId,
        call: SubagentInvocation,
    ) -> Result<InvocationReceipt, String> {
        let execution = self
            .execution(caller)
            .ok_or("caller has no active execution")?;
        let program = execution
            .agent_path
            .last()
            .and_then(|name| self.programs.get(name))
            .ok_or("caller program is unavailable")?;
        let target = program
            .subagents
            .get(&call.tool_name)
            .ok_or("subagent is not available to caller")?;
        let derivation_key = match target.mode {
            SubAgentMode::Stateless => call.origin.derivation_key(),
            SubAgentMode::Stateful => target.name.clone(),
        };
        let pid = ProcessId::from_uuid5(caller, &derivation_key);
        let envelope = Envelope::with_id(|id| Envelope {
            id,
            from: Sender::Agent {
                name: program.name.clone(),
                pid: caller.clone(),
            },
            to: Receiver {
                name: target.name.clone(),
                pid,
            },
            reply_to: None,
            body: EnvelopeBody::ToolCall {
                call_id: call.origin.call_id.clone(),
                parent_message_id: call.origin.message_id,
                derivation_key,
                turn_id: call.turn_id,
                task: call.task,
            },
        });
        if call.run_in_background {
            // Background admission must finish even if the caller is interrupted.
            let runtime = self.clone();
            let caller = caller.clone();
            tokio::spawn(async move {
                runtime
                    .dispatch_background(envelope, call.origin, caller)
                    .await
            })
            .await
            .map_err(|error| format!("Background dispatch stopped: {error}"))?
            .map(InvocationReceipt::Background)
        } else {
            let envelope_id = envelope.id.clone();
            self.send_message(envelope)
                .await
                .map_err(|error| error.to_string())?;
            Ok(InvocationReceipt::Foreground { envelope_id })
        }
    }
}
