use coda_core::llm::{ChatCompletionRequest, LLMProvider, LLMProviderConfig, LLMStreamEvent};
use coda_openai::{OpenAICompatible, ProviderKind};
use futures::StreamExt;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, timeout},
};

#[tokio::test]
async fn sdk_stream_preserves_model_reports_without_requiring_valid_metadata_on_every_chunk() {
    for kind in [
        ProviderKind::Generic,
        ProviderKind::Deepseek,
        ProviderKind::OpenRouter,
    ] {
        for (chunks, expected_models) in [
            (
                vec![
                    json!({"model": "first", "choices": [{"delta": {"role": "assistant"}}]}),
                    json!({"choices": [{"delta": {"content": "answer"}}]}),
                    json!({"choices": []}),
                ],
                vec!["first"],
            ),
            (
                vec![
                    json!({"model": {"invalid": true}, "choices": [{"delta": {"content": "answer"}}]}),
                    json!({"model": "last", "choices": []}),
                ],
                vec!["last"],
            ),
            (
                vec![
                    json!({"model": "first", "choices": [{"delta": {"content": "answer"}}]}),
                    json!({"model": "first", "choices": []}),
                    json!({"model": "last", "choices": []}),
                    json!({"model": null, "choices": []}),
                ],
                vec!["first", "last"],
            ),
            (
                vec![json!({"choices": [{"delta": {"content": "answer"}}]})],
                vec![],
            ),
        ] {
            timeout(Duration::from_secs(5), async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let header_end = loop {
                        let mut buffer = [0; 4096];
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "client closed before sending its request");
                        request.extend_from_slice(&buffer[..read]);
                        if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                            break index + 4;
                        }
                    };
                    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                    assert!(headers.starts_with("POST /v1/chat/completions "));
                    let content_length: usize = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap();
                    while request.len() < header_end + content_length {
                        let mut buffer = [0; 4096];
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "client closed before sending its body");
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let body: serde_json::Value = serde_json::from_slice(&request[header_end..]).unwrap();
                    assert_eq!(body["model"], "requested-alias");
                    assert_eq!(body["stream"], true);

                    let sse = chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>()
                        + "data: [DONE]\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len(),
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });

                let provider = OpenAICompatible::new(LLMProviderConfig {
                    api_key: "test-key".into(), base_url, include_usage: false,
                }, kind, "configured-provider");
                let mut stream = std::pin::pin!(provider.stream(ChatCompletionRequest {
                    model: "requested-alias".into(), ..Default::default()
                }));
                let mut models = Vec::new();
                let mut content = String::new();
                let mut completed = false;
                while let Some(event) = stream.next().await {
                    assert!(!completed, "Completed must be the last event");
                    match event.unwrap() {
                        LLMStreamEvent::ModelReported(model) => models.push(model),
                        LLMStreamEvent::ContentChunk(chunk) => content.push_str(&chunk),
                        LLMStreamEvent::ReasoningChunk(_) => panic!("unexpected reasoning"),
                        LLMStreamEvent::Completed(message) => {
                            assert_eq!(message.content, "answer");
                            assert!(message.generation.is_none());
                            completed = true;
                        }
                    }
                }
                assert!(completed);
                assert_eq!(content, "answer");
                assert_eq!(models, expected_models);
                server.await.unwrap();
            }).await.expect("local SDK stream timed out");
        }
    }
}
