//! [`Services`]: the storage backend (and later the event manager and configuration) shared by
//! the storages and crawlers of a process.
//!
//! Crawlee for JS resolves these through an ambient, `AsyncLocalStorage`-scoped service locator.
//! Here they are explicit: crawlers take a `Services` value (cheap to clone) or fall back to the
//! process-wide default.

use std::sync::{Arc, OnceLock};

use crate::errors::StorageResult;
use crate::storage::backend::{StorageBackend, StorageIdentifier};
use crate::storage::dataset::Dataset;
use crate::storage::key_value_store::KeyValueStore;
use crate::storage::memory::MemoryStorageBackend;
use crate::storage::request_queue::RequestQueue;

#[derive(Clone)]
pub struct Services {
    pub storage: Arc<dyn StorageBackend>,
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
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Services { storage }
    }

    /// Services backed by a fresh in-memory storage.
    pub fn in_memory() -> Self {
        Services::new(Arc::new(MemoryStorageBackend::new()))
    }

    /// The process-wide services, initialized with in-memory storage on first use.
    pub fn global() -> &'static Services {
        GLOBAL.get_or_init(Services::in_memory)
    }

    /// Sets the process-wide services. Fails once they have been read or set.
    pub fn set_global(services: Services) -> Result<(), ServiceConflictError> {
        GLOBAL.set(services).map_err(|_| ServiceConflictError)
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
