//! [`Services`]: the configuration and the storage backend shared by the storages and crawlers
//! of a process.
//!
//! Crawlee for JS resolves these through an ambient, `AsyncLocalStorage`-scoped service locator.
//! Here they are explicit: crawlers take a `Services` value (cheap to clone) or fall back to the
//! process-wide default.

use std::sync::{Arc, OnceLock};

use crate::configuration::Configuration;
use crate::errors::StorageResult;
use crate::storage::backend::{StorageBackend, StorageIdentifier};
use crate::storage::dataset::Dataset;
use crate::storage::key_value_store::KeyValueStore;
use crate::storage::memory::MemoryStorageBackend;
use crate::storage::request_queue::RequestQueue;

#[derive(Clone)]
pub struct Services {
    pub configuration: Arc<Configuration>,
    pub storage: Arc<dyn StorageBackend>,
    /// Set once the run-scoped storages were purged, so that they are purged once per backend
    /// even when several crawlers run one after another.
    purged: Arc<tokio::sync::OnceCell<()>>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

static GLOBAL: OnceLock<Services> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
#[error("the global services were already initialized; set them before first use")]
pub struct ServiceConflictError;

impl Services {
    /// Services with the default [`Configuration`] and the given storage.
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Services::with_configuration(Configuration::default(), storage)
    }

    pub fn with_configuration(configuration: Configuration, storage: Arc<dyn StorageBackend>) -> Self {
        Services { configuration: Arc::new(configuration), storage, purged: Arc::default() }
    }

    /// Services with the storage `configuration` asks for: files under
    /// [`storage_dir`](Configuration::storage_dir) when
    /// [`persist_storage`](Configuration::persist_storage) is set (and the `fs-storage` feature
    /// is enabled), process memory otherwise.
    pub fn from_configuration(configuration: Configuration) -> Self {
        #[cfg(feature = "fs-storage")]
        if configuration.persist_storage {
            let storage =
                Arc::new(crate::storage::file_system::FileSystemStorageBackend::new(configuration.storage_dir.clone()));
            return Services::with_configuration(configuration, storage);
        }
        Services::with_configuration(configuration, Arc::new(MemoryStorageBackend::new()))
    }

    /// Services backed by a fresh in-memory storage, with the default configuration.
    pub fn in_memory() -> Self {
        Services::new(Arc::new(MemoryStorageBackend::new()))
    }

    /// The process-wide services. Unless [`set_global`](Self::set_global) was called first, they
    /// are created on first use from [`Configuration::from_env`]: with the defaults, storages are
    /// files under `./storage`, as in Crawlee for JS.
    pub fn global() -> &'static Services {
        GLOBAL.get_or_init(|| Services::from_configuration(Configuration::from_env()))
    }

    /// Sets the process-wide services. Fails once they have been read or set.
    pub fn set_global(services: Services) -> Result<(), ServiceConflictError> {
        GLOBAL.set(services).map_err(|_| ServiceConflictError)
    }

    /// Empties the run-scoped storages (the default and aliased ones) if
    /// [`purge_on_start`](Configuration::purge_on_start) is set. Only the first call purges;
    /// crawlers call it before they open their storages.
    pub async fn purge_on_start(&self) -> StorageResult<()> {
        if !self.configuration.purge_on_start {
            return Ok(());
        }
        self.purged.get_or_try_init(|| self.storage.purge()).await?;
        Ok(())
    }

    pub async fn open_dataset(&self, id: &StorageIdentifier) -> StorageResult<Dataset> {
        Dataset::open(self.storage.as_ref(), id).await
    }

    pub async fn open_key_value_store(&self, id: &StorageIdentifier) -> StorageResult<KeyValueStore> {
        KeyValueStore::open(self.storage.as_ref(), id).await
    }

    pub async fn open_request_queue(&self, id: &StorageIdentifier) -> StorageResult<RequestQueue> {
        RequestQueue::open(self.storage.as_ref(), id).await
    }
}
