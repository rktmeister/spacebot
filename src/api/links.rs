//! API handlers for agent links and topology.

use crate::api::state::ApiState;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde::Serialize;
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
