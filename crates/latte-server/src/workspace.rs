//! Workspace management for the server.
use sha2::Digest;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tracing::info;

use crate::http::ServerEvent;

/// A fully constructed per-workspace runtime: a durable engine handle plus the
/// thread runtime service bound to that workspace's own provider registry.
///
/// The binary owns configuration and storage-path resolution, so it builds
/// these; the server only decides *when* to build one and caches the result.
pub struct BuiltWorkspace {
    pub engine: latte_engine::EngineHandle,
    pub runtime: latte_headless::thread::ThreadRuntimeService,
}

/// Builds a durable per-workspace runtime for one canonical workspace root.
/// Injected by the binary so each workspace resolves its own
/// `.latte/latte-code.jsonc` (models, endpoints, credentials) against the
/// shared global durable store.
pub type WorkspaceRuntimeBuilder =
    Arc<dyn Fn(&Path) -> Result<BuiltWorkspace, String> + Send + Sync>;

/// Resolves the canonical workspace root that durably owns a session, from the
/// global session catalog. Lets session reads survive a process restart
/// instead of relying only on the in-memory index.
pub type SessionLocator = Arc<dyn Fn(latte_core::ThreadId) -> Option<PathBuf> + Send + Sync>;

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
    /// Session ID -> workspace path index. A best-effort in-memory cache; the
    /// durable `session_locator` is the source of truth across restarts.
    session_index: Arc<RwLock<HashMap<latte_core::ThreadId, PathBuf>>>,
    /// Builds a durable per-workspace runtime on demand.
    builder: WorkspaceRuntimeBuilder,
    /// Resolves a session's owning workspace from the durable catalog.
    session_locator: SessionLocator,
}

impl WorkspaceManager {
    /// Create a new workspace manager from a durable runtime builder and a
    /// durable session locator.
    pub fn new(builder: WorkspaceRuntimeBuilder, session_locator: SessionLocator) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            session_index: Arc::new(RwLock::new(HashMap::new())),
            builder,
            session_locator,
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

        // Build the durable per-workspace runtime (own engine + own registry).
        let built = (self.builder)(&canonical)
            .map_err(|message| anyhow::anyhow!("failed to build workspace runtime: {message}"))?;

        let instance = Arc::new(WorkspaceInstance::new(
            id,
            canonical.clone(),
            built.runtime,
            event_tx,
            built.engine,
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

    /// Register a session in the index (best-effort in-memory cache).
    pub async fn register_session(
        &self,
        session_id: latte_core::ThreadId,
        workspace_path: PathBuf,
    ) {
        let mut index = self.session_index.write().await;
        index.insert(session_id, workspace_path);
    }

    /// Resolve the workspace path that owns a session. Prefers the in-memory
    /// cache, then falls back to the durable catalog locator so reads survive a
    /// process restart; a durable hit repopulates the cache.
    pub async fn get_session_workspace(
        &self,
        session_id: &latte_core::ThreadId,
    ) -> Option<PathBuf> {
        {
            let index = self.session_index.read().await;
            if let Some(path) = index.get(session_id) {
                return Some(path.clone());
            }
        }
        let path = (self.session_locator)(*session_id)?;
        self.session_index
            .write()
            .await
            .insert(*session_id, path.clone());
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager whose builder makes a durable per-workspace engine (its own
    /// SQLite under the workspace) with an error-stub provider factory, and
    /// whose locator never resolves (tests drive the in-memory index).
    fn manager() -> WorkspaceManager {
        let builder: WorkspaceRuntimeBuilder = Arc::new(|root: &Path| {
            let db = root.join(".latte/state.db");
            std::fs::create_dir_all(db.parent().unwrap()).map_err(|error| error.to_string())?;
            let engine = latte_engine::EngineBuilder::new()
                .workspace_root(root)
                .database_path(&db)
                .conversation_root(root.join(".latte/sessions"))
                .build()
                .map_err(|error| error.to_string())?;
            let factory: latte_headless::thread::ThreadProviderFactory =
                Arc::new(|_| Err("no provider configured in test".to_string()));
            let runtime = latte_headless::thread::ThreadRuntimeService::new(
                engine.clone(),
                root,
                Default::default(),
                factory,
            );
            Ok(BuiltWorkspace { engine, runtime })
        });
        let locator: SessionLocator = Arc::new(|_| None);
        WorkspaceManager::new(builder, locator)
    }

    #[tokio::test]
    async fn get_or_create_is_single_flight() {
        let manager = manager();
        let dir = tempfile::tempdir().unwrap();

        let first = manager.get_or_create(dir.path()).await.unwrap();
        let second = manager.get_or_create(dir.path()).await.unwrap();
        // Single-flight: the same canonical path returns the same instance.
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.id, second.id);

        // get_by_id resolves the created instance and misses on an unknown id.
        assert!(manager.get_by_id(&first.id).await.is_some());
        assert!(manager.get_by_id("ws_absent").await.is_none());
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
        assert!(manager.get_by_id(&ids[0]).await.is_some());
    }

    #[tokio::test]
    async fn session_index_registers_and_resolves() {
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
    }

    #[tokio::test]
    async fn session_workspace_falls_back_to_durable_locator() {
        // A manager whose durable locator always resolves to a fixed path, and
        // whose in-memory index is empty: resolution must fall through to the
        // locator (simulating a read after a restart) and then cache the hit.
        let resolved = std::path::PathBuf::from("/durable/workspace/root");
        let locator_path = resolved.clone();
        let builder: WorkspaceRuntimeBuilder =
            Arc::new(|_| Err("builder unused in this test".to_string()));
        let locator: SessionLocator = Arc::new(move |_| Some(locator_path.clone()));
        let manager = WorkspaceManager::new(builder, locator);

        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
        // Not in the in-memory index, but the durable locator resolves it.
        assert_eq!(
            manager.get_session_workspace(&thread_id).await,
            Some(resolved.clone())
        );
        // The durable hit is cached back into the index.
        assert_eq!(
            manager.get_session_workspace(&thread_id).await,
            Some(resolved)
        );
    }

    #[tokio::test]
    async fn session_workspace_none_when_locator_misses() {
        let manager = manager(); // locator always returns None
        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
        assert!(manager.get_session_workspace(&thread_id).await.is_none());
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

        // The default manager's provider factory is a configured-error stub;
        // starting a turn exercises it and yields a retryable child failure
        // rather than a panic.
        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
        let binding = latte_core::ThreadProviderBindingV2 {
            version: 1,
            provider_name: "test".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "test".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:TEST".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        let snapshot = workspace
            .runtime
            .start(thread_id, "hello".into(), binding)
            .await
            .expect("start persists a retryable failure without panicking");
        assert_eq!(snapshot.thread_id, thread_id);
    }
}
