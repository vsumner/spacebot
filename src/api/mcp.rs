//! API handlers for MCP server management.
//!
//! CRUD endpoints for `[[defaults.mcp]]` in config.toml, plus per-agent
//! connection status.

use super::config::{reload_all_runtime_configs, write_validated_config};
use super::state::ApiState;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// ── Request / Response types ────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct CreateMcpServerRequest {
    pub name: String,
    pub transport: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Serialize)]
pub(super) struct McpServerInfo {
    pub name: String,
    pub transport: String,
    pub enabled: bool,
    pub state: String,
}

#[derive(Serialize)]
pub(super) struct McpAgentStatus {
    pub agent_id: String,
    pub servers: Vec<McpServerInfo>,
}

#[derive(Serialize)]
pub(super) struct MutationResponse {
    pub success: bool,
    pub message: String,
}

// ── Handlers ────────────────────────────────────────────────────────────

/// GET /api/mcp/servers — list all configured MCP servers from config.toml.
pub(super) async fn list_mcp_servers(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Vec<McpServerInfo>>, StatusCode> {
    let config_path = state.config_path.read().await.clone();
    if !config_path.exists() {
        return Ok(Json(Vec::new()));
    }

    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let server_defs: Vec<(String, String, bool)> = {
        let doc: toml_edit::DocumentMut = content
            .parse()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let mut defs = Vec::new();
        if let Some(arr) = doc
            .get("defaults")
            .and_then(|defaults| defaults.get("mcp"))
            .and_then(|mcp| mcp.as_array_of_tables())
        {
            for table in arr.iter() {
                let name = table
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let transport = table
                    .get("transport")
                    .and_then(|v| v.as_str())
                    .unwrap_or("stdio")
                    .to_string();
                let enabled = table
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                defs.push((name, transport, enabled));
            }
        }
        defs
    };

    let mut servers = Vec::with_capacity(server_defs.len());
    for (name, transport, enabled) in server_defs {
        let state_str = get_server_state(&state, &name).await;
        servers.push(McpServerInfo {
            name,
            transport,
            enabled,
            state: state_str,
        });
    }

    Ok(Json(servers))
}

/// POST /api/mcp/servers — add a new MCP server definition to config.toml.
pub(super) async fn create_mcp_server(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateMcpServerRequest>,
) -> Result<Json<MutationResponse>, StatusCode> {
    if request.name.trim().is_empty() {
        return Ok(Json(MutationResponse {
            success: false,
            message: "Server name cannot be empty".into(),
        }));
    }

    let config_path = state.config_path.read().await.clone();
    let content = if config_path.exists() {
        tokio::fs::read_to_string(&config_path)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Check for duplicates
    if let Some(arr) = doc
        .get("defaults")
        .and_then(|defaults| defaults.get("mcp"))
        .and_then(|mcp| mcp.as_array_of_tables())
    {
        for table in arr.iter() {
            if table.get("name").and_then(|v| v.as_str()) == Some(&request.name) {
                return Ok(Json(MutationResponse {
                    success: false,
                    message: format!("MCP server '{}' already exists", request.name),
                }));
            }
        }
    }

    // Build the new table
    let mut new_table = toml_edit::Table::new();
    new_table.set_implicit(true);
    new_table["name"] = toml_edit::value(&request.name);
    new_table["transport"] = toml_edit::value(&request.transport);

    if !request.enabled {
        new_table["enabled"] = toml_edit::value(false);
    }
    if let Some(cmd) = &request.command {
        new_table["command"] = toml_edit::value(cmd);
    }
    if !request.args.is_empty() {
        let mut arr = toml_edit::Array::new();
        for arg in &request.args {
            arr.push(arg.as_str());
        }
        new_table["args"] = toml_edit::value(arr);
    }
    if let Some(url) = &request.url {
        new_table["url"] = toml_edit::value(url);
    }
    if !request.env.is_empty() {
        let mut env_table = toml_edit::InlineTable::new();
        for (k, v) in &request.env {
            env_table.insert(k, v.as_str().into());
        }
        new_table["env"] = toml_edit::value(env_table);
    }
    if !request.headers.is_empty() {
        let mut headers_table = toml_edit::InlineTable::new();
        for (k, v) in &request.headers {
            headers_table.insert(k, v.as_str().into());
        }
        new_table["headers"] = toml_edit::value(headers_table);
    }

    // Append to [[defaults.mcp]] array
    if doc.get("defaults").is_none() {
        doc["defaults"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if doc["defaults"].get("mcp").is_none() {
        doc["defaults"]["mcp"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    if let Some(arr) = doc
        .get_mut("defaults")
        .and_then(|defaults| defaults.get_mut("mcp"))
        .and_then(|v| v.as_array_of_tables_mut())
    {
        arr.push(new_table);
    }

    let new_config = write_validated_config(&state, &config_path, doc.to_string()).await?;
    reload_all_runtime_configs(&state, &new_config).await;

    Ok(Json(MutationResponse {
        success: true,
        message: format!("MCP server '{}' added", request.name),
    }))
}

/// PUT /api/mcp/servers — update an existing MCP server definition.
pub(super) async fn update_mcp_server(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateMcpServerRequest>,
) -> Result<Json<MutationResponse>, StatusCode> {
    let config_path = state.config_path.read().await.clone();
    if !config_path.exists() {
        return Ok(Json(MutationResponse {
            success: false,
            message: "No config file found".into(),
        }));
    }

    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(arr) = doc
        .get_mut("defaults")
        .and_then(|defaults| defaults.get_mut("mcp"))
        .and_then(|v| v.as_array_of_tables_mut())
    else {
        return Ok(Json(MutationResponse {
            success: false,
            message: format!("MCP server '{}' not found", request.name),
        }));
    };

    let mut found = false;
    for table in arr.iter_mut() {
        if table.get("name").and_then(|v| v.as_str()) == Some(&request.name) {
            table["transport"] = toml_edit::value(&request.transport);
            if request.enabled {
                table.remove("enabled");
            } else {
                table["enabled"] = toml_edit::value(false);
            }
            if let Some(cmd) = &request.command {
                table["command"] = toml_edit::value(cmd);
            } else {
                table.remove("command");
            }
            if !request.args.is_empty() {
                let mut args_arr = toml_edit::Array::new();
                for arg in &request.args {
                    args_arr.push(arg.as_str());
                }
                table["args"] = toml_edit::value(args_arr);
            } else {
                table.remove("args");
            }
            if let Some(url) = &request.url {
                table["url"] = toml_edit::value(url);
            } else {
                table.remove("url");
            }
            found = true;
            break;
        }
    }

    if !found {
        return Ok(Json(MutationResponse {
            success: false,
            message: format!("MCP server '{}' not found", request.name),
        }));
    }

    let new_config = write_validated_config(&state, &config_path, doc.to_string()).await?;
    reload_all_runtime_configs(&state, &new_config).await;

    Ok(Json(MutationResponse {
        success: true,
        message: format!("MCP server '{}' updated", request.name),
    }))
}

/// DELETE /api/mcp/servers/{name} — remove a server definition from config.toml.
pub(super) async fn delete_mcp_server(
    State(state): State<Arc<ApiState>>,
    Path(server_name): Path<String>,
) -> Result<Json<MutationResponse>, StatusCode> {
    let config_path = state.config_path.read().await.clone();
    if !config_path.exists() {
        return Ok(Json(MutationResponse {
            success: false,
            message: "No config file found".into(),
        }));
    }

    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(arr) = doc
        .get_mut("defaults")
        .and_then(|defaults| defaults.get_mut("mcp"))
        .and_then(|v| v.as_array_of_tables_mut())
    else {
        return Ok(Json(MutationResponse {
            success: false,
            message: format!("MCP server '{}' not found", server_name),
        }));
    };

    let original_len = arr.len();
    let mut idx = 0;
    while idx < arr.len() {
        let matches = arr
            .get(idx)
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            == Some(&server_name);
        if matches {
            arr.remove(idx);
        } else {
            idx += 1;
        }
    }

    if arr.len() == original_len {
        return Ok(Json(MutationResponse {
            success: false,
            message: format!("MCP server '{}' not found", server_name),
        }));
    }

    let new_config = write_validated_config(&state, &config_path, doc.to_string()).await?;
    reload_all_runtime_configs(&state, &new_config).await;

    Ok(Json(MutationResponse {
        success: true,
        message: format!("MCP server '{}' removed", server_name),
    }))
}

/// GET /api/mcp/status — per-agent MCP connection status.
pub(super) async fn mcp_status(State(state): State<Arc<ApiState>>) -> Json<Vec<McpAgentStatus>> {
    let managers = state.mcp_managers.load();
    let mut result = Vec::with_capacity(managers.len());

    for (agent_id, manager) in managers.iter() {
        let statuses = manager.statuses().await;
        let servers = statuses
            .into_iter()
            .map(|s| McpServerInfo {
                name: s.name,
                transport: s.transport,
                enabled: s.enabled,
                state: match &s.state {
                    crate::mcp::McpConnectionState::Connected => "connected".into(),
                    crate::mcp::McpConnectionState::Connecting => "connecting".into(),
                    crate::mcp::McpConnectionState::Disconnected => "disconnected".into(),
                    crate::mcp::McpConnectionState::Failed(err) => format!("failed: {err}"),
                },
            })
            .collect();

        result.push(McpAgentStatus {
            agent_id: agent_id.clone(),
            servers,
        });
    }

    Json(result)
}

/// POST /api/mcp/servers/{name}/reconnect — force-reconnect a specific server.
pub(super) async fn reconnect_mcp_server(
    State(state): State<Arc<ApiState>>,
    Path(server_name): Path<String>,
) -> Result<Json<MutationResponse>, StatusCode> {
    let managers = state.mcp_managers.load();

    for manager in managers.values() {
        match manager.reconnect(&server_name).await {
            Ok(()) => {
                return Ok(Json(MutationResponse {
                    success: true,
                    message: format!("Server '{}' reconnected", server_name),
                }));
            }
            Err(_) => continue,
        }
    }

    Ok(Json(MutationResponse {
        success: false,
        message: format!("Server '{}' not found in any agent", server_name),
    }))
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Look up live connection state for a server name across all agents.
async fn get_server_state(state: &ApiState, server_name: &str) -> String {
    let managers = state.mcp_managers.load();
    for manager in managers.values() {
        for status in manager.statuses().await {
            if status.name == server_name {
                return match &status.state {
                    crate::mcp::McpConnectionState::Connected => "connected".into(),
                    crate::mcp::McpConnectionState::Connecting => "connecting".into(),
                    crate::mcp::McpConnectionState::Disconnected => "disconnected".into(),
                    crate::mcp::McpConnectionState::Failed(err) => format!("failed: {err}"),
                };
            }
        }
    }
    "not_connected".into()
}

#[cfg(test)]
mod tests {
    use super::{CreateMcpServerRequest, create_mcp_server, delete_mcp_server};
    use crate::api::state::ApiState;
    use crate::config::{Config, RuntimeConfig};
    use axum::Json;
    use axum::extract::{Path, State};
    use std::collections::HashMap;
    use std::sync::Arc;

    const VALID_BASE_CONFIG: &str = r#"
[llm]
anthropic_key = "test-anthropic-key"

[defaults.routing]
channel = "anthropic/claude-sonnet-4"
branch = "anthropic/claude-sonnet-4"
worker = "anthropic/claude-sonnet-4"
compactor = "anthropic/claude-sonnet-4"
cortex = "anthropic/claude-sonnet-4"

[[agents]]
id = "main"
default = true
"#;

    fn new_test_state() -> Arc<ApiState> {
        let (provider_setup_tx, _) = tokio::sync::mpsc::channel(8);
        let (agent_tx, _) = tokio::sync::mpsc::channel(8);
        let (agent_remove_tx, _) = tokio::sync::mpsc::channel(8);
        Arc::new(ApiState::new_with_provider_sender(
            provider_setup_tx,
            agent_tx,
            agent_remove_tx,
        ))
    }

    async fn configure_runtime_for_main(
        state: &Arc<ApiState>,
        config_path: &std::path::Path,
    ) -> Arc<RuntimeConfig> {
        let config = Config::load_from_path(config_path).expect("config should load for test");
        let resolved_agent = config
            .resolve_agents()
            .into_iter()
            .find(|agent| agent.id == "main")
            .expect("main agent should exist in resolved config");
        let runtime_config = Arc::new(RuntimeConfig::new(
            &config.instance_dir,
            &resolved_agent,
            &config.defaults,
            crate::prompts::PromptEngine::new("en").expect("prompt engine should build"),
            crate::identity::Identity::default(),
            crate::skills::SkillSet::default(),
        ));

        let mut runtime_configs = HashMap::new();
        runtime_configs.insert("main".to_string(), runtime_config.clone());
        state.set_runtime_configs(runtime_configs);

        let mut mcp_managers = HashMap::new();
        mcp_managers.insert(
            "main".to_string(),
            Arc::new(crate::mcp::McpManager::new(resolved_agent.mcp.clone())),
        );
        state.set_mcp_managers(mcp_managers);

        runtime_config
    }

    #[tokio::test]
    async fn create_and_delete_mcp_server_persist_and_reload_runtime_config() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let config_path = temp_dir.path().join("config.toml");
        tokio::fs::write(&config_path, VALID_BASE_CONFIG)
            .await
            .expect("config.toml should be written");

        let state = new_test_state();
        state.set_config_path(config_path.clone()).await;
        let runtime_config = configure_runtime_for_main(&state, &config_path).await;

        let create_response = create_mcp_server(
            State(state.clone()),
            Json(CreateMcpServerRequest {
                name: "test-server".to_string(),
                transport: "stdio".to_string(),
                enabled: true,
                command: Some("echo".to_string()),
                args: vec!["hello".to_string()],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
            }),
        )
        .await
        .expect("create_mcp_server should succeed")
        .0;

        assert!(create_response.success);

        let persisted_after_create =
            Config::load_from_path(&config_path).expect("created config should parse");
        assert_eq!(persisted_after_create.defaults.mcp.len(), 1);
        assert_eq!(persisted_after_create.defaults.mcp[0].name, "test-server");

        let runtime_mcp_after_create = runtime_config.mcp.load();
        assert_eq!(runtime_mcp_after_create.len(), 1);
        assert_eq!(runtime_mcp_after_create[0].name, "test-server");

        let delete_response = delete_mcp_server(State(state), Path("test-server".to_string()))
            .await
            .expect("delete_mcp_server should succeed")
            .0;
        assert!(delete_response.success);

        let persisted_after_delete =
            Config::load_from_path(&config_path).expect("deleted config should parse");
        assert!(persisted_after_delete.defaults.mcp.is_empty());

        let runtime_mcp_after_delete = runtime_config.mcp.load();
        assert!(runtime_mcp_after_delete.is_empty());
    }
}
