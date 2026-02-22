//! Types for the agent communication graph.

use serde::{Deserialize, Serialize};

/// A directed edge in the agent communication graph.
///
/// Represents a policy-governed communication channel between two agents.
/// When agent A has a link to agent B, agent A can send messages to agent B.
/// The link carries direction and relationship flags that define the nature
/// of the communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLink {
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub direction: LinkDirection,
    pub relationship: LinkRelationship,
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
                let relationship: LinkRelationship = def
                    .relationship
                    .parse()
                    .map_err(|e: String| anyhow::anyhow!("{e} (link {} → {})", def.from, def.to))?;
                Ok(AgentLink {
                    from_agent_id: def.from.clone(),
                    to_agent_id: def.to.clone(),
                    direction,
                    relationship,
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

/// Relationship semantics for an agent link.
///
/// Affects the receiving agent's system prompt context. A superior can delegate tasks,
/// a subordinate reports status and escalates. Peers communicate collaboratively.
/// The relationship doesn't restrict message delivery — it frames context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkRelationship {
    /// Equal peers — neither agent has authority over the other.
    Peer,
    /// from_agent is superior to to_agent. Can delegate tasks,
    /// request status, and override decisions.
    Superior,
    /// from_agent is subordinate to to_agent. Reports status,
    /// escalates issues, requests approval.
    Subordinate,
}

impl LinkRelationship {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkRelationship::Peer => "peer",
            LinkRelationship::Superior => "superior",
            LinkRelationship::Subordinate => "subordinate",
        }
    }

    /// Get the inverse relationship from the other agent's perspective.
    pub fn inverse(&self) -> Self {
        match self {
            LinkRelationship::Peer => LinkRelationship::Peer,
            LinkRelationship::Superior => LinkRelationship::Subordinate,
            LinkRelationship::Subordinate => LinkRelationship::Superior,
        }
    }
}

impl std::fmt::Display for LinkRelationship {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for LinkRelationship {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "peer" => Ok(LinkRelationship::Peer),
            "superior" => Ok(LinkRelationship::Superior),
            "subordinate" => Ok(LinkRelationship::Subordinate),
            other => Err(format!(
                "invalid link relationship: '{other}', expected 'peer', 'superior', or 'subordinate'"
            )),
        }
    }
}
