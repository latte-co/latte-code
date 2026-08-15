//! Workspace management for the server.

use anyhow::{Context, Result};
use latte_headless::thread::ThreadRuntimeService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// A workspace instance with its own runtime.
pub struct WorkspaceInstance {
    /// Absolute path to the workspace root.
    pub path: PathBuf,
    /// The thread runtime service for this workspace.
    pub runtime: Arc<latte_headless::thread::ThreadRuntimeService>,
}

impl WorkspaceInstance {
    /// Create a new workspace instance.
    pub fn new(path: PathBuf, runtime: latte_headless::thread::ThreadRuntimeService) -> Self {
        Self {
            path,
            runtime: Arc::new(runtime),
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
    pub async fn get_or_create(&self, path: impl AsRef<Path>) -> Result<Arc<WorkspaceInstance>> {
        let path = path.as_ref().to_path_buf();

        // Try to get existing instance
        {
            let instances = self.instances.read().await;
            if let Some(instance) = instances.get(&path) {
                return Ok(instance.clone());
            }
        }

        // Create new instance
        info!("creating workspace instance for {}", path.display());

        // Validate the path
        let canonical = path.canonicalize().context("invalid workspace path")?;

        // Create the runtime for this workspace
        // TODO: pass proper config, provider, etc.
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(&canonical)
            .build()
            .context("failed to create engine")?;
        let runtime = ThreadRuntimeService::new(
            engine,
            &canonical,
            Default::default(),
            Arc::new(|_| unimplemented!("provider factory")),
        );

        let instance = Arc::new(WorkspaceInstance::new(canonical, runtime));

        // Store it
        let mut instances = self.instances.write().await;
        instances.insert(path.clone(), instance.clone());

        Ok(instance)
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
