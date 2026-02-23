//! Types for the agent communication graph.

use serde::{Deserialize, Serialize};

/// A directed edge in the agent communication graph.
///
/// Represents a policy-governed communication channel between two nodes (agents or humans).
/// For hierarchical links, `from` is the superior and `to` is the subordinate.
/// For peer links, the ordering is arbitrary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLink {
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub direction: LinkDirection,
    pub kind: LinkKind,
}

impl AgentLink {
    /// Parse config link definitions into agent links.
    pub fn from_config(defs: &[crate::config::LinkDef]) -> anyhow::Result<Vec<Self>> {
        defs.iter()
            .map(|def| {
                let direction: LinkDirection = def
                    .direction
                    .parse()
                    .map_err(|e: String| anyhow::anyhow!("{e} (link {} → {})", def.from, def.to))?;
                let kind: LinkKind = def
                    .kind
                    .parse()
                    .map_err(|e: String| anyhow::anyhow!("{e} (link {} → {})", def.from, def.to))?;
                Ok(AgentLink {
                    from_agent_id: def.from.clone(),
                    to_agent_id: def.to.clone(),
                    direction,
                    kind,
                })
            })
            .collect()
    }

    /// Stable identifier for the link channel conversation ID.
    /// Deterministic from agent IDs so the same link always maps to the same channel.
    pub fn channel_id(&self) -> String {
        // Sort agent IDs to ensure the same pair always produces the same channel
        let (a, b) = if self.from_agent_id <= self.to_agent_id {
            (&self.from_agent_id, &self.to_agent_id)
        } else {
            (&self.to_agent_id, &self.from_agent_id)
        };
        format!("link:{a}:{b}")
    }
}

/// Direction policy for an agent link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    /// from_agent can message to_agent, but not vice versa.
    OneWay,
    /// Both agents can message each other through this link.
    TwoWay,
}

impl LinkDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkDirection::OneWay => "one_way",
            LinkDirection::TwoWay => "two_way",
        }
    }
}

impl std::fmt::Display for LinkDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for LinkDirection {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "one_way" => Ok(LinkDirection::OneWay),
            "two_way" => Ok(LinkDirection::TwoWay),
            other => Err(format!(
                "invalid link direction: '{other}', expected 'one_way' or 'two_way'"
            )),
        }
    }
}

/// The kind of link between two nodes.
///
/// `Hierarchical` means `from` is above `to` in the org — `from` manages `to`.
/// `Peer` means both nodes are at the same level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// from is above to in the hierarchy. from manages to.
    Hierarchical,
    /// Both nodes are at the same level.
    Peer,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkKind::Hierarchical => "hierarchical",
            LinkKind::Peer => "peer",
        }
    }
}

impl std::fmt::Display for LinkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for LinkKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "hierarchical" => Ok(LinkKind::Hierarchical),
            "peer" => Ok(LinkKind::Peer),
            // Backward compat: map old relationship values
            "superior" => Ok(LinkKind::Hierarchical),
            "subordinate" => Ok(LinkKind::Hierarchical),
            other => Err(format!(
                "invalid link kind: '{other}', expected 'hierarchical' or 'peer'"
            )),
        }
    }
}
