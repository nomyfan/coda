use std::path::Path;

use coda_core::tool::ToolObject;
use coda_mcp::{McpConfig, McpError, McpServer};
use tracing::{info, warn};

pub struct McpServers {
    servers: Vec<McpServer>,
}

impl McpServers {
    pub fn empty() -> Self {
        McpServers { servers: vec![] }
    }

    pub fn tool_objects(&self) -> Vec<Box<dyn ToolObject>> {
        self.servers.iter().flat_map(|s| s.tool_objects()).collect()
    }

    pub async fn shutdown(self) {
        for server in self.servers {
            if let Err(e) = server.shutdown().await {
                warn!("MCP server shutdown error: {e}");
            }
        }
    }
}

pub async fn load_mcp_servers(workspace_dir: &Path) -> Result<McpServers, McpError> {
    let config_path = workspace_dir.join(".coda").join("mcp.json");
    if !config_path.exists() {
        return Ok(McpServers::empty());
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| McpError::Connect(format!("failed to read mcp.json: {e}")))?;
    let config: McpConfig = serde_json::from_str(&content)
        .map_err(|e| McpError::Connect(format!("invalid mcp.json: {e}")))?;

    let mut servers = Vec::new();
    for server_config in &config.servers {
        info!(server = server_config.name, "connecting to MCP server");
        match McpServer::connect(server_config).await {
            Ok(server) => {
                servers.push(server);
            }
            Err(e) => {
                warn!(
                    server = server_config.name,
                    error = %e,
                    "failed to connect to MCP server, skipping"
                );
            }
        }
    }

    Ok(McpServers { servers })
}
