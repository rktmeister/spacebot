//! Install skill tool — lets cortex install skills from skills.sh into the agent workspace.
//!
//! After finding a skill via `skills_search`, cortex can install it directly
//! using this tool. Skills are installed to the agent's workspace skills directory
//! and become immediately available to workers.

use crate::config::RuntimeConfig;
use crate::skills::SkillSet;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Tool for installing skills from the skills.sh registry.
#[derive(Debug, Clone)]
pub struct InstallSkillTool {
    runtime_config: Arc<RuntimeConfig>,
}

impl InstallSkillTool {
    pub fn new(runtime_config: Arc<RuntimeConfig>) -> Self {
        Self { runtime_config }
    }
}

/// Error type for install_skill tool.
#[derive(Debug, thiserror::Error)]
#[error("install_skill failed: {0}")]
pub struct InstallSkillError(String);

/// Arguments for install_skill tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallSkillArgs {
    /// GitHub source to install from, in `owner/repo` or `owner/repo/skill-name` format.
    /// Use the `source` field from `skills_search` results.
    pub source: String,
}

/// Output from install_skill tool.
#[derive(Debug, Serialize)]
pub struct InstallSkillOutput {
    pub success: bool,
    pub message: String,
    /// Names of the skills that were installed.
    pub installed: Vec<String>,
}

impl Tool for InstallSkillTool {
    const NAME: &'static str = "install_skill";

    type Error = InstallSkillError;
    type Args = InstallSkillArgs;
    type Output = InstallSkillOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/install_skill").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "GitHub source in owner/repo or owner/repo/skill-name format. Use the `source` field from skills_search results."
                    }
                },
                "required": ["source"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let source = args.source.trim();
        if source.is_empty() {
            return Ok(InstallSkillOutput {
                success: false,
                message: "source is required".to_string(),
                installed: Vec::new(),
            });
        }

        let target_dir = self.runtime_config.workspace_dir.join("skills");

        let installed = crate::skills::install_from_github(source, &target_dir)
            .await
            .map_err(|error| InstallSkillError(error.to_string()))?;

        if installed.is_empty() {
            return Ok(InstallSkillOutput {
                success: false,
                message: format!(
                    "No skills found in '{source}'. Check that the repo contains SKILL.md files."
                ),
                installed: Vec::new(),
            });
        }

        // Reload skills into RuntimeConfig so they're immediately available.
        let instance_skills_dir = self.runtime_config.instance_dir.join("skills");
        let skills = SkillSet::load(&instance_skills_dir, &target_dir).await;
        self.runtime_config.reload_skills(skills);

        let names = installed.join(", ");
        Ok(InstallSkillOutput {
            success: true,
            message: format!("Installed {} skill(s): {names}", installed.len()),
            installed,
        })
    }
}
