use std::sync::Arc;

use utils::{FolderId, ItemId};
use uuid::Uuid;

use crate::{
    AssetType, Category, CategoryKey, FolderType, InventoryNodeId, InventoryNodeKey, InventoryOwner, InventoryRevision, Item, ItemKey,
    LinkResolution, state,
    state::{InventoryLimits, InventoryState},
};

#[derive(Clone)]
pub struct InventorySnapshot {
    pub(crate) state: Arc<InventoryState>,
    pub(crate) limits: InventoryLimits,
}

impl InventorySnapshot {
    pub fn revision(&self) -> InventoryRevision {
        self.state.revision
    }

    pub fn folder_generation(&self, key: CategoryKey) -> Option<InventoryRevision> {
        self.state.folder_generations.get(&key).copied()
    }

    pub fn owner_refresh_generation(&self, owner: InventoryOwner) -> u64 {
        self.state.owner_refresh_generations.get(&owner).copied().unwrap_or_default()
    }

    pub fn roots(&self) -> crate::InventoryRoots {
        self.state.roots
    }

    pub fn category(&self, key: CategoryKey) -> Option<&Category> {
        self.state.categories.get(&key).map(Arc::as_ref)
    }

    pub fn item(&self, key: ItemKey) -> Option<&Item> {
        self.state.items.get(&key).map(Arc::as_ref)
    }

    pub fn categories(&self) -> impl Iterator<Item = &Category> {
        self.state.categories.values().map(Arc::as_ref)
    }

    pub fn category_count(&self) -> usize {
        self.state.categories.len()
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.state.items.values().map(Arc::as_ref)
    }

    pub fn items_of_asset_type(&self, asset_type: AssetType) -> impl Iterator<Item = &Item> {
        self.items().filter(move |item| item.asset_type == asset_type)
    }

    pub fn categories_for(&self, owner: InventoryOwner) -> impl Iterator<Item = &Category> {
        self.categories().filter(move |category| category.key.owner() == owner)
    }

    pub fn items_for(&self, owner: InventoryOwner) -> impl Iterator<Item = &Item> {
        self.items().filter(move |item| item.key.owner() == owner)
    }

    pub fn children(&self, parent: CategoryKey) -> impl Iterator<Item = InventoryNodeKey> + '_ {
        self.state
            .children
            .get(&parent)
            .into_iter()
            .flat_map(move |children| children.iter().copied().map(move |id| InventoryNodeKey::new(parent.owner(), id)))
    }

    pub fn child_count(&self, parent: CategoryKey) -> usize {
        self.state.children.get(&parent).map_or(0, |children| children.len())
    }

    pub fn incomplete_category_count(&self, owner: InventoryOwner) -> usize {
        self.state.incomplete_categories.get(&owner).copied().unwrap_or_default()
    }

    pub fn incomplete_category_count_all(&self) -> usize {
        self.state.incomplete_categories.values().copied().sum()
    }

    pub fn is_complete(&self, key: CategoryKey) -> bool {
        state::category_is_complete(&self.state, key)
    }

    pub fn category_with_type(&self, owner: InventoryOwner, folder_type: FolderType) -> Option<&Category> {
        let key = (owner, folder_type);
        let category = self.state.preferred_category_by_type.get(&key)?;
        self.category(*category)
    }

    pub fn link_resolution(&self, key: ItemKey) -> Option<LinkResolution> {
        let item = self.item(key)?;
        let target = match item.asset_type {
            AssetType::Link => InventoryNodeId::Item(ItemId::new(item.asset_id.uuid())),
            AssetType::LinkFolder => InventoryNodeId::Category(FolderId::new(item.asset_id.uuid())),
            _ => return Some(LinkResolution::NotLink),
        };
        let node_key = InventoryNodeKey::new(key.owner(), target);
        let loaded = match target {
            InventoryNodeId::Category(id) => self.state.categories.contains_key(&CategoryKey::new(key.owner(), id)),
            InventoryNodeId::Item(id) => self.state.items.contains_key(&ItemKey::new(key.owner(), id)),
        };
        if loaded {
            return Some(LinkResolution::Resolved(node_key));
        }
        let target_id = item.asset_id.uuid();
        Some(if self.owner_is_complete(key.owner()) { LinkResolution::Broken(target_id) } else { LinkResolution::NotLoaded(target_id) })
    }

    pub fn owner_is_complete(&self, owner: InventoryOwner) -> bool {
        self.incomplete_category_count(owner) == 0
    }

    pub fn contains_category_id(&self, owner: InventoryOwner, id: Uuid) -> bool {
        self.category(CategoryKey::new(owner, FolderId::new(id))).is_some()
    }
}
