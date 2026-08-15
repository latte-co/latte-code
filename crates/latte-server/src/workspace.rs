//! Workspace management for the server.
use sha2::Digest;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tracing::info;

use crate::http::ServerEvent;

/// Provider factory type.
pub type ProviderFactory = Arc<
    dyn Fn(
            &latte_core::ThreadProviderBindingV2,
        ) -> Result<latte_headless::registry::ResolvedProvider, String>
        + Send
        + Sync,
>;

/// A workspace instance with its own runtime.
pub struct WorkspaceInstance {
    /// Stable workspace ID.
    pub id: String,
    /// Absolute path to the workspace root.
    pub path: PathBuf,
    /// Canonical workspace identity string used by the engine's catalog, kept
    /// separate from `path` so session queries match the stored identity even
    /// when path spellings differ (e.g. `/var` vs `/private/var`).
    workspace_root: String,
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
        // The engine canonicalizes the workspace root for its session catalog.
        // Mirror that identity so `list_threads_v2_for_workspace` matches.
        let workspace_root = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .into_owned();
        let instance = Self {
            id,
            path,
            workspace_root,
            runtime: Arc::new(runtime),
            event_tx,
            engine,
        };

        // Start event bridge
        instance.start_event_bridge();

        instance
    }

    /// Loads the durable snapshot for one session owned by this workspace.
    ///
    /// # Errors
    /// Returns a storage error when the thread does not exist or cannot be read.
    pub fn snapshot(
        &self,
        thread_id: latte_core::ThreadId,
    ) -> Result<latte_core::ThreadSnapshot, latte_engine::StorageError> {
        self.engine.thread_snapshot_v2(thread_id, None, 500)
    }

    /// Lists the durable sessions bound to this workspace, newest transcript
    /// tail included, for the read API and the SSE refetch signal.
    ///
    /// # Errors
    /// Returns a storage error when the session catalog cannot be read.
    pub fn list_sessions(
        &self,
    ) -> Result<Vec<latte_core::ThreadSnapshot>, latte_engine::StorageError> {
        self.engine
            .list_threads_v2_for_workspace(&self.workspace_root)
    }

    /// Searches this workspace's local session catalog by title/id.
    ///
    /// # Errors
    /// Returns a storage error when the catalog cannot be searched.
    pub fn search_sessions(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<latte_core::ThreadSessionSummary>, latte_engine::StorageError> {
        self.engine.search_thread_sessions_v2(query, limit)
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
                    Err(latte_engine::SubscriptionError::Lagged(_)) => {
                        // On lag, send resync required and continue
                        let _ = event_tx.send(ServerEvent::ResyncRequired);
                    }
                    Err(latte_engine::SubscriptionError::Closed) => break,
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
    pub async fn register_session(
        &self,
        session_id: latte_core::ThreadId,
        workspace_path: PathBuf,
    ) {
        let mut index = self.session_index.write().await;
        index.insert(session_id, workspace_path);
    }

    /// Get the workspace path for a session.
    pub async fn get_session_workspace(
        &self,
        session_id: &latte_core::ThreadId,
    ) -> Option<PathBuf> {
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
        let factory: ProviderFactory =
            Arc::new(|_| Err("no provider factory configured".to_string()));
        Self::new(factory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> WorkspaceManager {
        WorkspaceManager::default()
    }

    #[tokio::test]
    async fn get_or_create_is_single_flight_and_lists_and_removes() {
        let manager = manager();
        let dir = tempfile::tempdir().unwrap();

        let first = manager.get_or_create(dir.path()).await.unwrap();
        let second = manager.get_or_create(dir.path()).await.unwrap();
        // Single-flight: the same canonical path returns the same instance.
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.id, second.id);

        let workspaces = manager.list_workspaces().await;
        assert_eq!(workspaces.len(), 1);

        // get_by_id resolves the created instance and misses on an unknown id.
        assert!(manager.get_by_id(&first.id).await.is_some());
        assert!(manager.get_by_id("ws_absent").await.is_none());

        assert!(manager.remove(&workspaces[0]).await);
        assert!(!manager.remove(&workspaces[0]).await);
        assert!(manager.list_workspaces().await.is_empty());
    }

    #[tokio::test]
    async fn get_or_create_rejects_missing_path() {
        let manager = manager();
        assert!(
            manager
                .get_or_create("/nonexistent/path/for/tests")
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_or_create_resolves_to_one_instance() {
        // Concurrent creation of the same workspace must exercise the
        // double-check-after-write-lock path and yield a single instance.
        let manager = Arc::new(manager());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                manager.get_or_create(&path).await.unwrap().id.clone()
            }));
        }
        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.unwrap());
        }
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(manager.list_workspaces().await.len(), 1);
    }

    #[tokio::test]
    async fn session_index_registers_and_unregisters() {
        let manager = manager();
        let dir = tempfile::tempdir().unwrap();
        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());

        assert!(manager.get_session_workspace(&thread_id).await.is_none());
        manager
            .register_session(thread_id, dir.path().to_path_buf())
            .await;
        assert_eq!(
            manager.get_session_workspace(&thread_id).await,
            Some(dir.path().to_path_buf())
        );
        manager.unregister_session(&thread_id).await;
        assert!(manager.get_session_workspace(&thread_id).await.is_none());
    }

    #[tokio::test]
    async fn emit_event_reaches_a_workspace_subscriber() {
        let manager = manager();
        let dir = tempfile::tempdir().unwrap();
        let workspace = manager.get_or_create(dir.path()).await.unwrap();
        let mut rx = workspace.event_tx.subscribe();

        manager
            .emit_event(&workspace.id, ServerEvent::ResyncRequired)
            .await;
        // Emitting to an unknown workspace id is a no-op, not an error.
        manager
            .emit_event("ws_absent", ServerEvent::ResyncRequired)
            .await;

        let event = rx.try_recv().expect("subscriber must observe the event");
        assert!(matches!(event, ServerEvent::ResyncRequired));
    }

    #[tokio::test]
    async fn snapshot_and_list_and_search_read_durable_sessions() {
        let manager = manager();
        let dir = tempfile::tempdir().unwrap();
        let workspace = manager.get_or_create(dir.path()).await.unwrap();

        // No sessions yet: reads are empty / not-found rather than errors.
        assert!(workspace.list_sessions().unwrap().is_empty());
        assert!(
            workspace
                .search_sessions("anything", 50)
                .unwrap()
                .is_empty()
        );
        let missing = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
        assert!(workspace.snapshot(missing).is_err());
    }
}
