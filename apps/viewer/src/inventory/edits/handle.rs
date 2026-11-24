use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use inventory::{CategoryKey, FolderType, ItemKey};
use tokio::sync::{mpsc, mpsc::error::TrySendError, oneshot};

use super::types::{EditOperation, EditRequest, InventoryEditOutcome, InventoryEditRejection};
use crate::inspection::BoundedQueueStats;

#[derive(Clone)]
pub struct InventoryEditHandle {
    requests: mpsc::Sender<EditRequest>,
    full_rejections: Arc<AtomicU64>,
}

impl InventoryEditHandle {
    pub(super) fn new(requests: mpsc::Sender<EditRequest>) -> Self {
        Self { requests, full_rejections: Arc::new(AtomicU64::new(0)) }
    }

    pub fn queue_stats(&self) -> BoundedQueueStats {
        let capacity = self.requests.max_capacity();
        BoundedQueueStats {
            depth: capacity.saturating_sub(self.requests.capacity()),
            capacity,
            full_count: self.full_rejections.load(Ordering::Relaxed),
        }
    }

    pub async fn create_folder(&self, parent: CategoryKey, name: impl Into<String>, preferred_type: FolderType) -> InventoryEditOutcome {
        self.submit(EditOperation::CreateFolder { parent, name: name.into(), preferred_type }).await
    }

    pub async fn rename_category(&self, category: CategoryKey, name: impl Into<String>) -> InventoryEditOutcome {
        self.submit(EditOperation::RenameCategory { category, name: name.into() }).await
    }

    pub async fn rename_item(&self, item: ItemKey, name: impl Into<String>) -> InventoryEditOutcome {
        self.submit(EditOperation::RenameItem { item, name: name.into() }).await
    }

    pub async fn move_item(&self, item: ItemKey, destination: CategoryKey) -> InventoryEditOutcome {
        self.submit(EditOperation::MoveItem { item, destination }).await
    }

    pub async fn move_folder(&self, folder: CategoryKey, destination: CategoryKey) -> InventoryEditOutcome {
        self.submit(EditOperation::MoveFolder { folder, destination }).await
    }

    pub async fn delete_item(&self, item: ItemKey) -> InventoryEditOutcome {
        self.submit(EditOperation::DeleteItem(item)).await
    }

    pub async fn delete_folder(&self, folder: CategoryKey) -> InventoryEditOutcome {
        self.submit(EditOperation::DeleteFolder(folder)).await
    }

    pub async fn empty_trash(&self) -> InventoryEditOutcome {
        self.submit(EditOperation::EmptyTrash).await
    }

    async fn submit(&self, operation: EditOperation) -> InventoryEditOutcome {
        let (response, result) = oneshot::channel();
        match self.requests.try_send(EditRequest { operation, response }) {
            Ok(()) => result.await.unwrap_or(InventoryEditOutcome::Rejected(InventoryEditRejection::SessionEnded)),
            Err(TrySendError::Full(_)) => {
                self.full_rejections.fetch_add(1, Ordering::Relaxed);
                InventoryEditOutcome::Rejected(InventoryEditRejection::QueueFull)
            }
            Err(TrySendError::Closed(_)) => InventoryEditOutcome::Rejected(InventoryEditRejection::SessionEnded),
        }
    }
}
