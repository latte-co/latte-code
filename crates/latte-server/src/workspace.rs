//! Workspace management for the server.
use sha2::Digest;

use anyhow::{Context, Result};
use latte_headless::thread::ThreadRuntimeService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use crate::http::ServerEvent;

/// A workspace instance with its own runtime.
pub struct WorkspaceInstance {
    /// Stable workspace ID.
    pub id: String,
    /// Absolute path to the workspace root.
    pub path: PathBuf,
    /// The thread runtime service for this workspace.
    pub runtime: Arc<latte_headless::thread::ThreadRuntimeService>,
    /// Event sender for this workspace.
    pub event_tx: broadcast::Sender<ServerEvent>,
}

impl WorkspaceInstance {
    /// Create a new workspace instance.
    pub fn new(
        id: String,
        path: PathBuf,
        runtime: latte_headless::thread::ThreadRuntimeService,
        event_tx: broadcast::Sender<ServerEvent>,
    ) -> Self {
        Self {
            id,
            path,
            runtime: Arc::new(runtime),
            event_tx,
        }
    }
}

/// Manages multiple workspace instances.
pub struct WorkspaceManager {
    instances: Arc<RwLock<HashMap<PathBuf, Arc<WorkspaceInstance>>>>,
}

impl WorkspaceManager {
    /// Create a new workspace manager.
    pub fn new() -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a workspace instance for the given path.
    /// Uses canonical path as key and single-flight creation.
    pub async fn get_or_create(&self, path: impl AsRef<Path>) -> Result<Arc<WorkspaceInstance>> {
        let raw_path = path.as_ref().to_path_buf();

        // Canonicalize first
        let canonical = raw_path.canonicalize().context("invalid workspace path")?;

        // Try to get existing instance with read lock
        {
            let instances = self.instances.read().await;
            if let Some(instance) = instances.get(&canonical) {
                return Ok(instance.clone());
            }
        }

        // Acquire write lock for creation (single-flight)
        let mut instances = self.instances.write().await;

        // Double-check after acquiring write lock
        if let Some(instance) = instances.get(&canonical) {
            return Ok(instance.clone());
        }

        // Create new instance
        info!("creating workspace instance for {}", canonical.display());

        // Generate stable workspace ID from canonical path
        let id = format!(
            "ws_{}",
            sha2::Sha256::digest(canonical.to_string_lossy().as_bytes())
                .iter()
                .take(8)
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );

        // Create event channel for this workspace
        let (event_tx, _) = broadcast::channel(256);

        // Create the runtime for this workspace
        // TODO: pass proper config, provider, etc.
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(&canonical)
            .build()
            .context("failed to create engine")?;
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine,
            &canonical,
            Default::default(),
            Arc::new(|_| unimplemented!("provider factory")),
        );

        let instance = Arc::new(WorkspaceInstance::new(id, canonical.clone(), runtime, event_tx));

        // Store and return the winning instance
        instances.insert(canonical, instance.clone());

        Ok(instance)
    }

    /// Get a workspace by its ID.
    pub async fn get_by_id(&self, id: &str) -> Option<Arc<WorkspaceInstance>> {
        let instances = self.instances.read().await;
        instances.values().find(|i| i.id == id).cloned()
    }

    /// Emit an event to a workspace's event stream.
    pub async fn emit_event(&self, workspace_id: &str, event: ServerEvent) {
        if let Some(workspace) = self.get_by_id(workspace_id).await {
            let _ = workspace.event_tx.send(event);
        }
    }

    /// List all active workspace paths.
    pub async fn list_workspaces(&self) -> Vec<PathBuf> {
        let instances = self.instances.read().await;
        instances.keys().cloned().collect()
    }

    /// Remove a workspace instance.
    pub async fn remove(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        let mut instances = self.instances.write().await;
        instances.remove(path).is_some()
    }
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}
