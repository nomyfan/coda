mod tool_adapter;

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::info;

use coda_core::tool::ToolObject;
use tool_adapter::McpToolAdapter;

type RoleClient = rmcp::RoleClient;

#[derive(Debug)]
pub enum McpError {
    InvalidConfig { server: String, reason: String },
    UnknownTransport { server: String, transport: String },
    Connect(String),
    Protocol(String),
    Tool(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::InvalidConfig { server, reason } => {
                write!(f, "MCP config error for server '{server}': {reason}")
            }
            McpError::UnknownTransport { server, transport } => {
                write!(
                    f,
                    "MCP unknown transport '{transport}' for server '{server}'"
                )
            }
            McpError::Connect(msg) => write!(f, "MCP connect error: {msg}"),
            McpError::Protocol(msg) => write!(f, "MCP protocol error: {msg}"),
            McpError::Tool(msg) => write!(f, "MCP tool error: {msg}"),
        }
    }
}

impl std::error::Error for McpError {}

#[derive(Debug, Clone)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<http::HeaderName, http::HeaderValue>,
    },
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfigRaw>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfigRaw {
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn resolve_headers(
    server: &str,
    raw: HashMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>, McpError> {
    raw.into_iter()
        .map(|(k, v)| {
            let name = http::HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                McpError::InvalidConfig {
                    server: server.to_string(),
                    reason: format!("invalid header name '{k}': {e}"),
                }
            })?;
            let value = http::HeaderValue::from_str(&v).map_err(|e| McpError::InvalidConfig {
                server: server.to_string(),
                reason: format!("invalid header value for '{k}': {e}"),
            })?;
            Ok((name, value))
        })
        .collect()
}

impl McpServerConfigRaw {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            r#type: None,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        }
    }

    pub fn resolve(self, name: String) -> Result<McpServerConfig, McpError> {
        let transport = match self.r#type.as_deref() {
            Some("stdio") => McpTransport::Stdio {
                command: self.command.ok_or_else(|| McpError::InvalidConfig {
                    server: name.clone(),
                    reason: "type is 'stdio' but 'command' is missing".into(),
                })?,
                args: self.args,
                env: self.env,
            },
            Some("http" | "streamable-http") => McpTransport::Http {
                url: self.url.ok_or_else(|| McpError::InvalidConfig {
                    server: name.clone(),
                    reason: "type is 'http' but 'url' is missing".into(),
                })?,
                headers: resolve_headers(&name, self.headers)?,
            },
            Some(other) => {
                return Err(McpError::UnknownTransport {
                    server: name,
                    transport: other.to_string(),
                });
            }
            None => match (self.command, self.url) {
                (Some(command), None) => McpTransport::Stdio {
                    command,
                    args: self.args,
                    env: self.env,
                },
                (None, Some(url)) => McpTransport::Http {
                    headers: resolve_headers(&name, self.headers)?,
                    url,
                },
                (Some(_), Some(_)) => {
                    return Err(McpError::InvalidConfig {
                        server: name,
                        reason: "both 'command' and 'url' are set, specify 'type' to disambiguate"
                            .into(),
                    });
                }
                (None, None) => {
                    return Err(McpError::InvalidConfig {
                        server: name,
                        reason: "either 'command' or 'url' must be provided".into(),
                    });
                }
            },
        };
        Ok(McpServerConfig { name, transport })
    }
}

pub(crate) struct McpClient {
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, McpError> {
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;

        let parts: Vec<String> = result
            .content
            .iter()
            .map(|c| match c.as_text() {
                Some(t) => t.text.clone(),
                None => serde_json::to_string(&c.raw).unwrap_or_default(),
            })
            .collect();
        let output = parts.join("\n");

        if result.is_error == Some(true) {
            Err(McpError::Tool(output))
        } else {
            Ok(output)
        }
    }
}

pub struct McpServer {
    client: Arc<McpClient>,
    prefix: String,
    tools: Vec<Tool>,
}

impl McpServer {
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        let service = match &config.transport {
            McpTransport::Stdio { command, args, env } => {
                let args = args.clone();
                let env = env.clone();
                let transport =
                    TokioChildProcess::new(Command::new(command).configure(move |cmd| {
                        cmd.args(&args);
                        for (k, v) in &env {
                            cmd.env(k, v);
                        }
                    }))
                    .map_err(|e| McpError::Connect(e.to_string()))?;

                ().serve(transport)
                    .await
                    .map_err(|e| McpError::Connect(e.to_string()))?
            }
            McpTransport::Http { url, headers } => {
                let http_config =
                    rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .custom_headers(headers.clone());
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::from_config(http_config);

                ().serve(transport)
                    .await
                    .map_err(|e| McpError::Connect(e.to_string()))?
            }
        };

        if let Some(info) = service.peer_info() {
            info!(
                server = config.name,
                name = ?info.server_info.name,
                version = ?info.server_info.version,
                "MCP server connected"
            );
        }

        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;

        info!(
            server = config.name,
            count = tools.len(),
            "discovered MCP tools"
        );

        Ok(McpServer {
            client: Arc::new(McpClient { service }),
            prefix: config.name.clone(),
            tools,
        })
    }

    /// Create fresh `ToolObject` instances for all tools on this server.
    pub fn tool_objects(&self) -> Vec<Box<dyn ToolObject>> {
        self.tools
            .iter()
            .map(|tool| {
                Box::new(McpToolAdapter::new(
                    self.client.clone(),
                    &self.prefix,
                    tool.clone(),
                )) as Box<dyn ToolObject>
            })
            .collect()
    }

    pub async fn shutdown(self) -> Result<(), McpError> {
        let mut client = self.client;
        for _ in 0..10 {
            match Arc::try_unwrap(client) {
                Ok(c) => {
                    return c
                        .service
                        .cancel()
                        .await
                        .map_err(|e| McpError::Protocol(e.to_string()))
                        .map(|_| ());
                }
                Err(arc) => {
                    client = arc;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        Err(McpError::Protocol(
            "cannot shutdown: tool adapters still hold references after retries".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_explicit_stdio() {
        let raw = McpServerConfigRaw {
            r#type: Some("stdio".into()),
            command: Some("npx".into()),
            args: vec!["-y".into(), "some-server".into()],
            env: HashMap::from([("KEY".into(), "VAL".into())]),
            ..McpServerConfigRaw::empty()
        };
        let config = raw.resolve("s1".into()).unwrap();
        assert_eq!(config.name, "s1");
        match config.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "some-server"]);
                assert_eq!(env.get("KEY").unwrap(), "VAL");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn resolve_explicit_http() {
        let raw = McpServerConfigRaw {
            r#type: Some("http".into()),
            url: Some("http://localhost:8080/mcp".into()),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            ..McpServerConfigRaw::empty()
        };
        let config = raw.resolve("s2".into()).unwrap();
        assert_eq!(config.name, "s2");
        match config.transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "http://localhost:8080/mcp");
                assert_eq!(
                    headers.get(&http::header::AUTHORIZATION).unwrap(),
                    "Bearer tok"
                );
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn resolve_infer_stdio_from_command() {
        let raw = McpServerConfigRaw {
            command: Some("node".into()),
            args: vec!["server.js".into()],
            ..McpServerConfigRaw::empty()
        };
        let config = raw.resolve("s3".into()).unwrap();
        assert!(matches!(config.transport, McpTransport::Stdio { .. }));
    }

    #[test]
    fn resolve_infer_http_from_url() {
        let raw = McpServerConfigRaw {
            url: Some("http://example.com/mcp".into()),
            headers: HashMap::from([("X-Custom".into(), "v".into())]),
            ..McpServerConfigRaw::empty()
        };
        let config = raw.resolve("s4".into()).unwrap();
        match config.transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "http://example.com/mcp");
                assert_eq!(
                    headers
                        .get(&http::HeaderName::from_static("x-custom"))
                        .unwrap(),
                    "v"
                );
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn resolve_stdio_missing_command() {
        let raw = McpServerConfigRaw {
            r#type: Some("stdio".into()),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "bad"));
    }

    #[test]
    fn resolve_http_missing_url() {
        let raw = McpServerConfigRaw {
            r#type: Some("http".into()),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "bad"));
    }

    #[test]
    fn resolve_unknown_transport() {
        let raw = McpServerConfigRaw {
            r#type: Some("grpc".into()),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(
            matches!(err, McpError::UnknownTransport { server, transport } if server == "bad" && transport == "grpc")
        );
    }

    #[test]
    fn resolve_ambiguous_command_and_url() {
        let raw = McpServerConfigRaw {
            command: Some("node".into()),
            url: Some("http://localhost/mcp".into()),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "bad"));
    }

    #[test]
    fn resolve_neither_command_nor_url() {
        let raw = McpServerConfigRaw::empty();
        let err = raw.resolve("empty".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "empty"));
    }

    #[test]
    fn resolve_invalid_header_name() {
        let raw = McpServerConfigRaw {
            url: Some("http://example.com/mcp".into()),
            headers: HashMap::from([("bad header\0".into(), "v".into())]),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "bad"));
    }

    #[test]
    fn resolve_invalid_header_value() {
        let raw = McpServerConfigRaw {
            r#type: Some("http".into()),
            url: Some("http://example.com/mcp".into()),
            headers: HashMap::from([("X-Ok".into(), "bad\nvalue".into())]),
            ..McpServerConfigRaw::empty()
        };
        let err = raw.resolve("bad".into()).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { server, .. } if server == "bad"));
    }

    #[test]
    fn resolve_explicit_type_overrides_inference() {
        let raw = McpServerConfigRaw {
            r#type: Some("stdio".into()),
            command: Some("node".into()),
            url: Some("http://ignored".into()),
            ..McpServerConfigRaw::empty()
        };
        let config = raw.resolve("s5".into()).unwrap();
        assert!(matches!(config.transport, McpTransport::Stdio { .. }));
    }
}
