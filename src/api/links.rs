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
    groups: Vec<TopologyGroup>,
}

#[derive(Debug, Serialize)]
struct TopologyAgent {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

#[derive(Debug, Serialize)]
struct TopologyLink {
    from: String,
    to: String,
    direction: String,
    relationship: String,
}

#[derive(Debug, Serialize)]
struct TopologyGroup {
    name: String,
    agent_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

/// Get the full agent topology for graph rendering.
pub async fn topology(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let agent_configs = state.agent_configs.load();
    let agents: Vec<TopologyAgent> = agent_configs
        .iter()
        .map(|config| TopologyAgent {
            id: config.id.clone(),
            name: config.display_name.clone().unwrap_or_else(|| config.id.clone()),
            display_name: config.display_name.clone(),
            role: config.role.clone(),
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

    let all_groups = state.agent_groups.load();
    let groups: Vec<TopologyGroup> = all_groups
        .iter()
        .map(|group| TopologyGroup {
            name: group.name.clone(),
            agent_ids: group.agent_ids.clone(),
            color: group.color.clone(),
        })
        .collect();

    Json(TopologyResponse {
        agents,
        links,
        groups,
    })
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

// -- Group CRUD --

#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default)]
    pub agent_ids: Vec<String>,
    pub color: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroupRequest {
    pub name: Option<String>,
    pub agent_ids: Option<Vec<String>>,
    pub color: Option<String>,
}

/// List all groups.
pub async fn list_groups(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let groups = state.agent_groups.load();
    Json(serde_json::json!({ "groups": &**groups }))
}

/// Create a visual agent group.
pub async fn create_group(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateGroupRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if request.name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let existing = state.agent_groups.load();
    if existing.iter().any(|g| g.name == request.name) {
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

    if doc.get("groups").is_none() {
        doc["groups"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let groups_array = doc["groups"]
        .as_array_of_tables_mut()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut group_table = toml_edit::Table::new();
    group_table["name"] = toml_edit::value(&request.name);
    let mut ids = toml_edit::Array::new();
    for id in &request.agent_ids {
        ids.push(id.as_str());
    }
    group_table["agent_ids"] = toml_edit::value(ids);
    if let Some(color) = &request.color {
        group_table["color"] = toml_edit::value(color.as_str());
    }
    groups_array.push(group_table);

    tokio::fs::write(&config_path, doc.to_string())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to write config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let new_group = crate::config::GroupDef {
        name: request.name.clone(),
        agent_ids: request.agent_ids.clone(),
        color: request.color.clone(),
    };
    let mut groups = (**existing).clone();
    groups.push(new_group.clone());
    state.set_agent_groups(groups);

    tracing::info!(name = %request.name, "agent group created via API");

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "group": new_group }))))
}

/// Update a group by name.
pub async fn update_group(
    State(state): State<Arc<ApiState>>,
    Path(group_name): Path<String>,
    Json(request): Json<UpdateGroupRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = state.agent_groups.load();
    let index = existing
        .iter()
        .position(|g| g.name == group_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut updated = existing[index].clone();
    let new_name = request.name.as_deref().unwrap_or(&group_name);

    // If renaming, check for conflict
    if new_name != group_name && existing.iter().any(|g| g.name == new_name) {
        return Err(StatusCode::CONFLICT);
    }

    if let Some(name) = &request.name {
        updated.name = name.clone();
    }
    if let Some(agent_ids) = &request.agent_ids {
        updated.agent_ids = agent_ids.clone();
    }
    if let Some(color) = &request.color {
        updated.color = Some(color.clone());
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

    if let Some(groups_array) = doc.get_mut("groups").and_then(|g| g.as_array_of_tables_mut()) {
        for table in groups_array.iter_mut() {
            let table_name = table.get("name").and_then(|v| v.as_str());
            if table_name == Some(&group_name) {
                if request.name.is_some() {
                    table["name"] = toml_edit::value(updated.name.as_str());
                }
                if let Some(agent_ids) = &request.agent_ids {
                    let mut arr = toml_edit::Array::new();
                    for id in agent_ids {
                        arr.push(id.as_str());
                    }
                    table["agent_ids"] = toml_edit::value(arr);
                }
                if let Some(color) = &request.color {
                    table["color"] = toml_edit::value(color.as_str());
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

    let mut groups = (**existing).clone();
    groups[index] = updated.clone();
    state.set_agent_groups(groups);

    tracing::info!(name = %group_name, "agent group updated via API");

    Ok(Json(serde_json::json!({ "group": updated })))
}

/// Delete a group by name.
pub async fn delete_group(
    State(state): State<Arc<ApiState>>,
    Path(group_name): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = state.agent_groups.load();
    if !existing.iter().any(|g| g.name == group_name) {
        return Err(StatusCode::NOT_FOUND);
    }

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

    if let Some(groups_array) = doc.get_mut("groups").and_then(|g| g.as_array_of_tables_mut()) {
        let mut remove_index = None;
        for (idx, table) in groups_array.iter().enumerate() {
            let table_name = table.get("name").and_then(|v| v.as_str());
            if table_name == Some(&group_name) {
                remove_index = Some(idx);
                break;
            }
        }
        if let Some(idx) = remove_index {
            groups_array.remove(idx);
        }
    }

    tokio::fs::write(&config_path, doc.to_string())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to write config.toml");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let groups: Vec<_> = existing
        .iter()
        .filter(|g| g.name != group_name)
        .cloned()
        .collect();
    state.set_agent_groups(groups);

    tracing::info!(name = %group_name, "agent group deleted via API");

    Ok(StatusCode::NO_CONTENT)
}
