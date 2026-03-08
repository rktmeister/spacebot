//! SSH daemon management for Spacebot instances.
//!
//! Provides API endpoints for pushing authorized keys and toggling sshd.
//! sshd listens on port 2222 to avoid conflicts with other services that
//! may occupy port 22 in containerized environments.
//!
//! Host keys and authorized_keys are persisted under `$SPACEBOT_DIR/ssh/`
//! so they survive container restarts.

use super::state::ApiState;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

/// Port sshd listens on. Uses 2222 to avoid conflicts with other services
/// that may occupy port 22 in containerized environments.
const SSHD_PORT: u16 = 2222;

/// Host key algorithms to generate if missing.
const HOST_KEY_TYPES: &[&str] = &["rsa", "ecdsa", "ed25519"];

#[derive(Deserialize)]
pub(super) struct AuthorizedKeyRequest {
    public_key: String,
}

#[derive(Serialize)]
pub(super) struct AuthorizedKeyResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
pub(super) struct SshStatusResponse {
    enabled: bool,
    port: u16,
    has_authorized_key: bool,
}

/// PUT /api/ssh/authorized-key — write the public key for SSH access.
pub(super) async fn set_authorized_key(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<AuthorizedKeyRequest>,
) -> Result<Json<AuthorizedKeyResponse>, StatusCode> {
    let key = request.public_key.trim();
    if key.is_empty() {
        return Ok(Json(AuthorizedKeyResponse {
            success: false,
            message: "public_key is empty".into(),
        }));
    }

    let ssh_dir = ssh_data_dir(&state).await;
    if let Err(error) = tokio::fs::create_dir_all(&ssh_dir).await {
        tracing::error!(%error, "failed to create SSH data directory");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let authorized_keys_path = ssh_dir.join("authorized_keys");
    if let Err(error) = tokio::fs::write(&authorized_keys_path, format!("{key}\n")).await {
        tracing::error!(%error, "failed to write authorized_keys");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Also write to the system location so sshd can find it.
    if let Err(error) = install_authorized_keys(&authorized_keys_path).await {
        tracing::error!(%error, "failed to install authorized_keys to /root/.ssh");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    tracing::info!("SSH authorized key updated");
    Ok(Json(AuthorizedKeyResponse {
        success: true,
        message: "authorized key updated".into(),
    }))
}

/// GET /api/ssh/status — check if SSH is available.
pub(super) async fn ssh_status(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<SshStatusResponse>, StatusCode> {
    let ssh_dir = ssh_data_dir(&state).await;
    let has_key = ssh_dir.join("authorized_keys").exists();
    let enabled = is_sshd_running().await;

    Ok(Json(SshStatusResponse {
        enabled,
        port: SSHD_PORT,
        has_authorized_key: has_key,
    }))
}

/// Enable SSH: generate host keys if needed, install authorized_keys, start sshd.
pub async fn enable(state: &ApiState) -> Result<(), String> {
    let ssh_dir = ssh_data_dir(state).await;
    tokio::fs::create_dir_all(&ssh_dir)
        .await
        .map_err(|e| format!("failed to create SSH dir: {e}"))?;

    // Generate host keys into the persistent directory if missing.
    generate_host_keys(&ssh_dir)
        .await
        .map_err(|e| format!("failed to generate host keys: {e}"))?;

    // Install host keys to /etc/ssh/ so sshd can find them.
    install_host_keys(&ssh_dir)
        .await
        .map_err(|e| format!("failed to install host keys: {e}"))?;

    // Install authorized_keys to /root/.ssh/ if we have one.
    let ak_path = ssh_dir.join("authorized_keys");
    if ak_path.exists() {
        install_authorized_keys(&ak_path)
            .await
            .map_err(|e| format!("failed to install authorized_keys: {e}"))?;
    }

    // Create /run/sshd (required by sshd).
    tokio::fs::create_dir_all("/run/sshd")
        .await
        .map_err(|e| format!("failed to create /run/sshd: {e}"))?;

    // Start sshd if not already running.
    if is_sshd_running().await {
        tracing::info!("sshd already running, skipping start");
        return Ok(());
    }

    let output = Command::new("/usr/sbin/sshd")
        .args([
            "-e",
            "-o",
            "PermitRootLogin=prohibit-password",
            "-o",
            "PasswordAuthentication=no",
            "-p",
            &SSHD_PORT.to_string(),
        ])
        .output()
        .await
        .map_err(|e| format!("failed to start sshd: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sshd exited with {}: {stderr}", output.status));
    }

    tracing::info!(port = SSHD_PORT, "sshd started");
    Ok(())
}

/// Disable SSH: kill sshd.
pub async fn disable() -> Result<(), String> {
    let output = Command::new("pkill")
        .args(["-f", &format!("/usr/sbin/sshd.*-p {SSHD_PORT}")])
        .output()
        .await
        .map_err(|e| format!("failed to kill sshd: {e}"))?;

    // pkill returns 1 if no processes matched — that's fine.
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pkill failed: {stderr}"));
    }

    tracing::info!("sshd stopped");
    Ok(())
}

// -- Internal helpers --

/// Persistent SSH directory on the data volume.
async fn ssh_data_dir(state: &ApiState) -> PathBuf {
    let instance_dir = state.instance_dir.load();
    instance_dir.join("ssh")
}

/// Check if sshd is running by looking for its PID file or process.
async fn is_sshd_running() -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("/usr/sbin/sshd.*-p {SSHD_PORT}")])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate SSH host keys into the persistent directory if they don't exist.
async fn generate_host_keys(ssh_dir: &Path) -> Result<(), std::io::Error> {
    for key_type in HOST_KEY_TYPES {
        let key_path = ssh_dir.join(format!("ssh_host_{key_type}_key"));
        if key_path.exists() {
            continue;
        }

        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                key_type,
                "-f",
                &key_path.to_string_lossy(),
                "-N",
                "", // no passphrase
                "-q",
            ])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(key_type, %stderr, "ssh-keygen failed");
        }
    }
    Ok(())
}

/// Copy host keys from the persistent directory to /etc/ssh/.
async fn install_host_keys(ssh_dir: &Path) -> Result<(), std::io::Error> {
    for key_type in HOST_KEY_TYPES {
        let src = ssh_dir.join(format!("ssh_host_{key_type}_key"));
        let src_pub = ssh_dir.join(format!("ssh_host_{key_type}_key.pub"));
        let dst = PathBuf::from(format!("/etc/ssh/ssh_host_{key_type}_key"));
        let dst_pub = PathBuf::from(format!("/etc/ssh/ssh_host_{key_type}_key.pub"));

        if src.exists() {
            tokio::fs::copy(&src, &dst).await?;
            // Host keys must be 0600 or sshd refuses them.
            set_permissions(&dst, 0o600).await?;
        }
        if src_pub.exists() {
            tokio::fs::copy(&src_pub, &dst_pub).await?;
            set_permissions(&dst_pub, 0o644).await?;
        }
    }
    Ok(())
}

/// Copy authorized_keys to /root/.ssh/authorized_keys.
async fn install_authorized_keys(src: &Path) -> Result<(), std::io::Error> {
    let dot_ssh = PathBuf::from("/root/.ssh");
    tokio::fs::create_dir_all(&dot_ssh).await?;
    set_permissions(&dot_ssh, 0o700).await?;

    let dst = dot_ssh.join("authorized_keys");
    tokio::fs::copy(src, &dst).await?;
    set_permissions(&dst, 0o600).await?;
    Ok(())
}

/// Set Unix file permissions.
async fn set_permissions(path: &Path, mode: u32) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    tokio::fs::set_permissions(path, perms).await
}
