//! Workspace management for the server.
use sha2::Digest;

use anyhow::{Context, Result};
use latte_headless::thread::ThreadRuntimeService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use crate::http::ServerEvent;

/// Provider factory type.
pub type ProviderFactory = Arc<
    dyn Fn(&latte_core::ThreadProviderBindingV2) -> Result<latte_headless::registry::ResolvedProvider, String>
        + Send
        + Sync,
>;

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
    /// Engine handle for event subscription.
    engine: latte_engine::EngineHandle,
}

impl WorkspaceInstance {
    /// Create a new workspace instance.
    pub fn new(
        id: String,
        path: PathBuf,
        runtime: latte_headless::thread::ThreadRuntimeService,
        event_tx: broadcast::Sender<ServerEvent>,
        engine: latte_engine::EngineHandle,
    ) -> Self {
        let instance = Self {
            id,
            path,
            runtime: Arc::new(runtime),
            event_tx,
            engine,
        };

        // Start event bridge
        instance.start_event_bridge();

        instance
    }

    /// Start bridging engine events to the workspace event channel.
    fn start_event_bridge(&self) {
        let engine = self.engine.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let mut subscription = engine.subscribe_threads();
            loop {
                match subscription.recv().await {
                    Ok(event) => {
                        // Forward engine events to workspace event channel
                        let server_event = match event.event {
                            latte_core::ThreadEvent::LifecycleChanged { .. } => {
                                ServerEvent::ThreadChanged {
                                    session_id: event.thread_id.to_string(),
                                    revision: event.revision,
                                }
                            }
                            _ => continue,
                        };
                        let _ = event_tx.send(server_event);
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

/// Manages multiple workspace instances.
pub struct WorkspaceManager {
    instances: Arc<RwLock<HashMap<PathBuf, Arc<WorkspaceInstance>>>>,
    /// Session ID -> workspace path index.
    session_index: Arc<RwLock<HashMap<latte_core::ThreadId, PathBuf>>>,
    /// Provider factory.
    provider_factory: ProviderFactory,
}

impl WorkspaceManager {
    /// Create a new workspace manager.
    pub fn new(provider_factory: ProviderFactory) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            session_index: Arc::new(RwLock::new(HashMap::new())),
            provider_factory,
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
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(&canonical)
            .build()
            .context("failed to create engine")?;
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine.clone(),
            &canonical,
            Default::default(),
            self.provider_factory.clone(),
        );

        let instance = Arc::new(WorkspaceInstance::new(
            id,
            canonical.clone(),
            runtime,
            event_tx,
            engine,
        ));

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

    /// Register a session in the index.
    pub async fn register_session(&self, session_id: latte_core::ThreadId, workspace_path: PathBuf) {
        let mut index = self.session_index.write().await;
        index.insert(session_id, workspace_path);
    }

    /// Get the workspace path for a session.
    pub async fn get_session_workspace(&self, session_id: &latte_core::ThreadId) -> Option<PathBuf> {
        let index = self.session_index.read().await;
        index.get(session_id).cloned()
    }

    /// Remove a session from the index.
    pub async fn unregister_session(&self, session_id: &latte_core::ThreadId) {
        let mut index = self.session_index.write().await;
        index.remove(session_id);
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
        // Default provider factory that returns an error
        let factory: ProviderFactory = Arc::new(|_| {
            Err("no provider factory configured".to_string())
        });
        Self::new(factory)
    }
}
