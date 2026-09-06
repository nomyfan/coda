use super::*;
use crate::agent::Receiver;
use coda_core::llm::ToolOutput;
use futures::poll;

#[tokio::test]
async fn backpressured_reply_is_archived_when_exit_precedes_delivery() {
    for receiver_closes in [false, true] {
        let runtime = ProcessRuntime::new(MemoryStorage::default(), "root".into());
        let reply = Envelope::with_id(|id| Envelope {
            id,
            from: Sender::Agent {
                name: "worker".into(),
                thread_id: ProcessId::new(),
            },
            to: Receiver {
                name: "coda".into(),
                thread_id: "root".to_string().into(),
            },
            reply_to: Some("accepted-call".into()),
            body: EnvelopeBody::Reply {
                call_id: "call".into(),
                output: ToolOutput::Ok("done".into()),
                aborted: false,
            },
        });
        // Keep the recipient paused with a full inbox to exercise delivery independently of LLM timing.
        let (sender, inbox) = mpsc::channel(1);
        let mut inbox = Some(inbox);
        sender.try_send(reply.clone()).unwrap();
        let (control, _commands) = mpsc::channel(1);
        let abort = runtime
            .process_tasks
            .lock()
            .unwrap()
            .spawn(async { "recipient".into() });
        runtime.processes.lock().await.insert(
            "root".into(),
            ProcessHandle {
                control_sender: control,
                message_sender: sender,
                abort,
                finished: tokio::sync::watch::channel(true).1,
            },
        );
        let mut pending = std::pin::pin!(runtime.deliver(reply.clone()));
        assert!(poll!(&mut pending).is_pending());
        timeout(Duration::from_secs(1), runtime.request_exit())
            .await
            .expect("exit must not wait for inbox capacity");
        if receiver_closes {
            drop(inbox.take());
        } else {
            // A permit may become available after exit; it must not bypass the archive either.
            inbox.as_mut().unwrap().recv().await.unwrap();
        }
        timeout(Duration::from_secs(1), &mut pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime.snapshot.lock().await.drained_envelopes["root"].len(),
            1
        );
        assert_eq!(
            runtime.snapshot.lock().await.drained_envelopes["root"][0].id,
            reply.id
        );
        if let Some(inbox) = &mut inbox {
            assert!(
                inbox.try_recv().is_err(),
                "an archived reply must not also enter the inbox"
            );
        }
        assert!(runtime.wait_for_exit(Some(Duration::from_secs(1))).await);
    }
}
