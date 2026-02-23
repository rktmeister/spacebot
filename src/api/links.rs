//! API handlers for agent links and topology.

use crate::api::state::ApiState;
use crate::links::{AgentLink, LinkDirection, LinkRelationship};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// List all links in the instance.
pub async fn list_links(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let links = state.agent_links.load();
    Json(serde_json::json!({ "links": &**links }))
}

/// Get links for a specific agent.
pub async fn agent_links(
    State(state): State<Arc<ApiState>>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    let all_links = state.agent_links.load();
    let links: Vec<_> = crate::links::links_for_agent(&all_links, &agent_id);
    Json(serde_json::json!({ "links": links }))
}

/// Topology response for graph rendering.
#[derive(Debug, Serialize)]
struct TopologyResponse {
    agents: Vec<TopologyAgent>,
    links: Vec<TopologyLink>,
}

#[derive(Debug, Serialize)]
struct TopologyAgent {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct TopologyLink {
    from: String,
    to: String,
    direction: String,
    relationship: String,
}

/// Get the full agent topology for graph rendering.
pub async fn topology(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let agent_configs = state.agent_configs.load();
    let agents: Vec<TopologyAgent> = agent_configs
        .iter()
        .map(|config| TopologyAgent {
            id: config.id.clone(),
            name: config.id.clone(),
        })
        .collect();

    let all_links = state.agent_links.load();
    let links: Vec<TopologyLink> = all_links
        .iter()
        .map(|link| TopologyLink {
            from: link.from_agent_id.clone(),
            to: link.to_agent_id.clone(),
            direction: link.direction.as_str().to_string(),
            relationship: link.relationship.as_str().to_string(),
        })
        .collect();

    Json(TopologyResponse { agents, links })
}

// -- Write endpoints --

#[derive(Debug, Deserialize)]
pub struct CreateLinkRequest {
    pub from: String,
    pub to: String,
    #[serde(default = "default_direction")]
    pub direction: String,
    #[serde(default = "default_relationship")]
    pub relationship: String,
}

fn default_direction() -> String {
    "two_way".into()
}

fn default_relationship() -> String {
    "peer".into()
}

#[derive(Debug, Deserialize)]
pub struct UpdateLinkRequest {
    pub direction: Option<String>,
    pub relationship: Option<String>,
}

/// Create a new link between two agents. Persists to config.toml and updates in-memory state.
pub async fn create_link(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateLinkRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate direction and relationship parse correctly
    let direction: LinkDirection = request
        .direction
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let relationship: LinkRelationship = request
        .relationship
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Validate agents exist
    let agent_configs = state.agent_configs.load();
    let from_exists = agent_configs.iter().any(|a| a.id == request.from);
    let to_exists = agent_configs.iter().any(|a| a.id == request.to);
    if !from_exists || !to_exists {
        return Err(StatusCode::NOT_FOUND);
    }

    if request.from == request.to {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Check for duplicate
    let existing = state.agent_links.load();
    let duplicate = existing.iter().any(|link| {
        link.from_agent_id == request.from && link.to_agent_id == request.to
    });
    if duplicate {
        return Err(StatusCode::CONFLICT);
    }

    // Write to config.toml
    let config_path = state.config_path.read().await.clone();
    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(|error| {
        tracing::warn!(%error, "failed to parse config.toml");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Get or create the [[links]] array
    if doc.get("links").is_none() {
        doc["links"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let links_array = doc["links"]
        .as_array_of_tables_mut()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut link_table = toml_edit::Table::new();
    link_table["from"] = toml_edit::value(&request.from);
    link_table["to"] = toml_edit::value(&request.to);
    link_table["direction"] = toml_edit::value(direction.as_str());
    link_table["relationship"] = toml_edit::value(relationship.as_str());
    links_array.push(link_table);

    tokio::fs::write(&config_path, doc.to_string())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to write config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Update in-memory state
    let new_link = AgentLink {
        from_agent_id: request.from.clone(),
        to_agent_id: request.to.clone(),
        direction,
        relationship,
    };
    let mut links = (**existing).clone();
    links.push(new_link.clone());
    state.set_agent_links(links);

    tracing::info!(
        from = %request.from,
        to = %request.to,
        direction = %direction,
        relationship = %relationship,
        "agent link created via API"
    );

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "link": new_link,
        })),
    ))
}

/// Update a link's properties. Identifies the link by from/to agent IDs in the path.
pub async fn update_link(
    State(state): State<Arc<ApiState>>,
    Path((from, to)): Path<(String, String)>,
    Json(request): Json<UpdateLinkRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = state.agent_links.load();
    let link_index = existing
        .iter()
        .position(|link| link.from_agent_id == from && link.to_agent_id == to)
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut updated = existing[link_index].clone();
    if let Some(dir) = &request.direction {
        updated.direction = dir.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    }
    if let Some(rel) = &request.relationship {
        updated.relationship = rel.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    }

    // Write to config.toml
    let config_path = state.config_path.read().await.clone();
    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(|error| {
        tracing::warn!(%error, "failed to parse config.toml");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Find and update the matching [[links]] entry
    if let Some(links_array) = doc.get_mut("links").and_then(|l| l.as_array_of_tables_mut()) {
        for table in links_array.iter_mut() {
            let table_from = table.get("from").and_then(|v| v.as_str());
            let table_to = table.get("to").and_then(|v| v.as_str());
            if table_from == Some(&from) && table_to == Some(&to) {
                if request.direction.is_some() {
                    table["direction"] = toml_edit::value(updated.direction.as_str());
                }
                if request.relationship.is_some() {
                    table["relationship"] = toml_edit::value(updated.relationship.as_str());
                }
                break;
            }
        }
    }

    tokio::fs::write(&config_path, doc.to_string())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to write config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Update in-memory state
    let mut links = (**existing).clone();
    links[link_index] = updated.clone();
    state.set_agent_links(links);

    tracing::info!(from = %from, to = %to, "agent link updated via API");

    Ok(Json(serde_json::json!({ "link": updated })))
}

/// Delete a link between two agents.
pub async fn delete_link(
    State(state): State<Arc<ApiState>>,
    Path((from, to)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = state.agent_links.load();
    let had_link = existing
        .iter()
        .any(|link| link.from_agent_id == from && link.to_agent_id == to);
    if !had_link {
        return Err(StatusCode::NOT_FOUND);
    }

    // Write to config.toml
    let config_path = state.config_path.read().await.clone();
    let content = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(|error| {
        tracing::warn!(%error, "failed to parse config.toml");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Remove the matching [[links]] entry
    if let Some(links_array) = doc.get_mut("links").and_then(|l| l.as_array_of_tables_mut()) {
        let mut remove_index = None;
        for (idx, table) in links_array.iter().enumerate() {
            let table_from = table.get("from").and_then(|v| v.as_str());
            let table_to = table.get("to").and_then(|v| v.as_str());
            if table_from == Some(&from) && table_to == Some(&to) {
                remove_index = Some(idx);
                break;
            }
        }
        if let Some(idx) = remove_index {
            links_array.remove(idx);
        }
    }

    tokio::fs::write(&config_path, doc.to_string())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to write config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Update in-memory state
    let links: Vec<_> = existing
        .iter()
        .filter(|link| !(link.from_agent_id == from && link.to_agent_id == to))
        .cloned()
        .collect();
    state.set_agent_links(links);

    tracing::info!(from = %from, to = %to, "agent link deleted via API");

    Ok(StatusCode::NO_CONTENT)
}
