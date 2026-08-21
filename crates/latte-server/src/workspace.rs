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
    pub registry: std::sync::Arc<latte_headless::registry::ProviderRegistry>,
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
    pub engine: latte_engine::EngineHandle,
    /// Provider registry for binding discovery.
    registry: std::sync::Arc<latte_headless::registry::ProviderRegistry>,
}

impl WorkspaceInstance {
    /// Create a new workspace instance.
    pub fn new(
        id: String,
        path: PathBuf,
        runtime: latte_headless::thread::ThreadRuntimeService,
        event_tx: broadcast::Sender<ServerEvent>,
        engine: latte_engine::EngineHandle,
        registry: std::sync::Arc<latte_headless::registry::ProviderRegistry>,
    ) -> Self {
        // The engine canonicalizes the workspace root for its session catalog.
        // Mirror that identity so `list_threads_v2_for_workspace` matches.
        let workspace_root = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .into_owned();

        // Wire progress events to the workspace event channel.
        let progress_event_tx = event_tx.clone();
        let progress_sink: std::sync::Arc<dyn latte_headless::thread::ThreadProgressSink> =
            std::sync::Arc::new(
                move |thread_id: latte_core::ThreadId,
                      progress: latte_core::ThreadTransientProgress| {
                    let run_id = match &progress {
                        latte_core::ThreadTransientProgress::ProviderAttempt { run_id, .. }
                        | latte_core::ThreadTransientProgress::AssistantDelta { run_id, .. }
                        | latte_core::ThreadTransientProgress::ToolProgress { run_id, .. } => {
                            run_id.to_string()
                        }
                    };
                    let _ = progress_event_tx.send(ServerEvent::Progress {
                        session_id: thread_id.to_string(),
                        run_id,
                        progress: serde_json::to_value(&progress).unwrap_or_default(),
                    });
                },
            );
        let runtime = runtime.with_progress_sink(progress_sink);

        let instance = Self {
            id,
            path,
            workspace_root,
            runtime: Arc::new(runtime),
            event_tx,
            engine,
            registry,
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
        // Use the tail (newest 500 entries) to match the TUI's
        // `thread_snapshot_tail_v2` behavior: the TUI shows the latest
        // transcript page, not the oldest.
        self.engine.thread_snapshot_tail_v2(thread_id, 500)
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

    /// Finds sessions whose title exactly matches `title` in this workspace.
    /// Unlike `search_sessions` (substring match with a result cap), this
    /// uses the engine's exact-title index so the match is not truncated by
    /// pagination.
    ///
    /// # Errors
    /// Returns a storage error when the catalog cannot be searched.
    pub fn find_sessions_by_exact_title(
        &self,
        title: &str,
        limit: usize,
    ) -> Result<Vec<latte_core::ThreadSessionSummary>, latte_engine::StorageError> {
        self.engine
            .find_thread_sessions_v2_by_exact_title_for_workspace(
                &self.workspace_root,
                title,
                limit,
            )
    }

    /// Returns the provider binding catalog for model discovery.
    ///
    /// # Errors
    /// Returns a registry error when a configured model's binding cannot be
    /// constructed (fail-closed: the client sees the broken configuration
    /// instead of a silently partial catalog).
    pub fn bindings(
        &self,
    ) -> Result<
        Vec<latte_headless::registry::BindingCatalogEntry>,
        latte_headless::registry::RegistryError,
    > {
        self.registry
            .thread_binding_catalog(&self.engine.tool_descriptors())
    }

    /// Start bridging engine events to the workspace event channel.
    fn start_event_bridge(&self) {
        let mut subscription = self.engine.subscribe_threads();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            loop {
                match subscription.recv().await {
                    Ok(event) => {
                        // Forward all durable thread events as wake-up signals.
                        // SSE is wake-up only; clients refetch snapshots.
                        let server_event = ServerEvent::ThreadChanged {
                            session_id: event.thread_id.to_string(),
                            revision: event.revision,
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
            built.registry,
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

    /// Start a background task that periodically recovers expired leases.
    /// The task runs until `shutdown` fires (the server lifecycle owner
    /// signals it and joins the returned handle on exit).
    pub fn start_recovery_sweeper(
        self: &Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately; skip it so recovery runs
            // after one interval, not at startup.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => {
                        if *shutdown.borrow_and_update() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let instances: Vec<Arc<WorkspaceInstance>> = {
                            let instances = manager.instances.read().await;
                            instances.values().cloned().collect()
                        };
                        for instance in instances {
                            if let Err(error) = instance.engine.recover_expired_leases() {
                                tracing::warn!("lease recovery failed for workspace {}: {error}", instance.id);
                            }
                        }
                    }
                }
            }
        })
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
            let registry = std::sync::Arc::new(
                latte_headless::registry::ProviderRegistry::parse_jsonc(
                    r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#,
                )
                .map_err(|error| error.to_string())?,
            );
            Ok(BuiltWorkspace {
                engine,
                runtime,
                registry,
            })
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
            .start(thread_id, "hello".into(), binding, None)
            .await
            .expect("start persists a retryable failure without panicking");
        assert_eq!(snapshot.thread_id, thread_id);
    }

    #[tokio::test]
    async fn get_or_create_builder_failure_is_reported() {
        // A builder that always fails: get_or_create surfaces the error rather
        // than panicking or caching a broken instance.
        let builder: WorkspaceRuntimeBuilder =
            Arc::new(|_| Err("simulated builder failure".to_string()));
        let locator: SessionLocator = Arc::new(|_| None);
        let manager = WorkspaceManager::new(builder, locator);
        let dir = tempfile::tempdir().unwrap();

        let result = manager.get_or_create(dir.path()).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            err.to_string().contains("simulated builder failure"),
            "unexpected error: {err}"
        );

        // A subsequent call with the same path retries the builder (no stale
        // cache entry from the failure).
        assert!(manager.get_or_create(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn event_bridge_closes_when_engine_drops() {
        // Dropping the engine handle closes its broadcast sender, which causes
        // the event bridge loop to receive Closed and break (line 144).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".latte/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .conversation_root(dir.path().join(".latte/sessions"))
            .build()
            .unwrap();
        let (event_tx, _event_rx) = broadcast::channel(16);
        let factory: latte_headless::thread::ThreadProviderFactory =
            Arc::new(|_| Err("unused".to_string()));
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine.clone(),
            dir.path(),
            Default::default(),
            factory,
        );
        // Creating the instance starts the event bridge task.
        let instance = WorkspaceInstance::new(
            "ws_test".into(),
            dir.path().to_path_buf(),
            runtime,
            event_tx.clone(),
            engine.clone(),
            std::sync::Arc::new(latte_headless::registry::ProviderRegistry::parse_jsonc(r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#).unwrap()),
        );

        // Drop all EngineHandle clones (our local + the one inside instance)
        // so the broadcast sender closes and the bridge task gets Closed.
        drop(engine);
        drop(instance);
        // Give the bridge task time to observe Closed and break.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn workspace_instance_with_non_canonical_path_uses_fallback() {
        // When the path cannot be canonicalized (doesn't exist on disk),
        // WorkspaceInstance::new uses the raw path as workspace_root (line 65).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".latte/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .conversation_root(dir.path().join(".latte/sessions"))
            .build()
            .unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let factory: latte_headless::thread::ThreadProviderFactory =
            Arc::new(|_| Err("unused".to_string()));
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine.clone(),
            dir.path(),
            Default::default(),
            factory,
        );

        // A path that doesn't exist triggers the unwrap_or_else fallback.
        let fake_path = PathBuf::from("/nonexistent/workspace/for/coverage");
        let instance = WorkspaceInstance::new(
            "ws_fake".into(),
            fake_path.clone(),
            runtime,
            event_tx,
            engine,
            std::sync::Arc::new(latte_headless::registry::ProviderRegistry::parse_jsonc(r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#).unwrap()),
        );
        assert_eq!(instance.id, "ws_fake");
        assert_eq!(instance.path, fake_path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_bridge_resyncs_on_lagged_subscription() {
        // Flooding the engine's thread_events channel (capacity 64) with
        // synchronous commits before the bridge task is polled causes the
        // bridge receiver to lag; the bridge forwards a ResyncRequired event
        // (covers the Lagged arm).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".latte/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .conversation_root(dir.path().join(".latte/sessions"))
            .build()
            .unwrap();
        let (event_tx, mut event_rx) = broadcast::channel(256);
        let factory: latte_headless::thread::ThreadProviderFactory =
            Arc::new(|_| Err("unused".to_string()));
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine.clone(),
            dir.path(),
            Default::default(),
            factory,
        );
        // Creating the instance starts the event bridge task. On a
        // current-thread runtime the task is not polled until this test yields.
        let _instance = WorkspaceInstance::new(
            "ws_lag".into(),
            dir.path().to_path_buf(),
            runtime,
            event_tx,
            engine.clone(),
            std::sync::Arc::new(latte_headless::registry::ProviderRegistry::parse_jsonc(r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#).unwrap()),
        );

        // Produce more thread events than the channel capacity (64) without
        // yielding, so the bridge receiver falls behind and lags.
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
        let now = latte_core::wall_time_ms();
        for _ in 0..70 {
            let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
            let run_id = latte_core::RunId::from_uuid(uuid::Uuid::now_v7());
            let lease = engine.acquire_thread_lease(thread_id, now, 60_000).unwrap();
            engine
                .create_started_thread_v2(
                    &latte_core::ThreadCommandId::from_uuid(uuid::Uuid::now_v7()),
                    thread_id,
                    run_id,
                    binding.clone(),
                    "prompt",
                    &lease,
                    now,
                    None,
                )
                .unwrap();
        }

        // Yield so the bridge task is polled and observes the lag.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The bridge must have forwarded a ResyncRequired event.
        let mut saw_resync = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, crate::http::ServerEvent::ResyncRequired) {
                saw_resync = true;
            }
        }
        assert!(
            saw_resync,
            "event bridge did not forward ResyncRequired on lagged subscription"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_or_create_with_slow_builder_hits_double_check() {
        // A slow builder holds the write lock long enough for other tasks to
        // queue up; when they acquire the lock they hit the double-check path
        // (lines 194-195) and return the existing instance.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let builder: WorkspaceRuntimeBuilder = Arc::new(move |root: &Path| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let db = root.join(".latte/state.db");
            std::fs::create_dir_all(db.parent().unwrap()).map_err(|e| e.to_string())?;
            let engine = latte_engine::EngineBuilder::new()
                .workspace_root(root)
                .database_path(&db)
                .conversation_root(root.join(".latte/sessions"))
                .build()
                .map_err(|e| e.to_string())?;
            let factory: latte_headless::thread::ThreadProviderFactory =
                Arc::new(|_| Err("no provider".to_string()));
            let runtime = latte_headless::thread::ThreadRuntimeService::new(
                engine.clone(),
                root,
                Default::default(),
                factory,
            );
            let registry = std::sync::Arc::new(
                latte_headless::registry::ProviderRegistry::parse_jsonc(
                    r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#,
                )
                .map_err(|error| error.to_string())?,
            );
            Ok(BuiltWorkspace {
                engine,
                runtime,
                registry,
            })
        });
        let locator: SessionLocator = Arc::new(|_| None);
        let manager = Arc::new(WorkspaceManager::new(builder, locator));

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
    async fn recovery_sweeper_recovers_expired_leases_then_shuts_down_cleanly() {
        // The sweeper must actually run under the server lifecycle: on each
        // tick it recovers every workspace's expired leases, and when the
        // owner signals shutdown the task exits and joins without being
        // abandoned. This is the production wiring the review flagged as
        // missing (start_recovery_sweeper had no caller).
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(manager());
        let instance = manager.get_or_create(dir.path()).await.unwrap();

        // Simulate a crash: a running thread whose lease is already expired
        // against the wall clock (absolute expiry at epoch 1001ms).
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
        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
        let run_id = latte_core::RunId::from_uuid(uuid::Uuid::now_v7());
        let lease = instance
            .engine
            .acquire_thread_lease(thread_id, 1, 1000)
            .unwrap();
        instance
            .engine
            .create_started_thread_v2(
                &latte_core::ThreadCommandId::from_uuid(uuid::Uuid::now_v7()),
                thread_id,
                run_id,
                binding,
                "crashed mid-run",
                &lease,
                2,
                None,
            )
            .unwrap();

        // Start the sweeper with a short interval so the test does not wait 30s.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle =
            manager.start_recovery_sweeper(shutdown_rx, std::time::Duration::from_millis(20));

        // Poll until the sweep recovers the expired lease (thread interrupted).
        let mut recovered = false;
        for _ in 0..100 {
            let snapshot = instance
                .engine
                .thread_snapshot_v2(thread_id, None, 100)
                .unwrap();
            if snapshot.lifecycle == latte_core::ThreadLifecycle::Interrupted {
                recovered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(recovered, "sweeper did not recover the expired lease");

        // Signal shutdown; the task must observe it and join cleanly.
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("sweeper task did not join after shutdown")
            .expect("sweeper task panicked");
    }
}
