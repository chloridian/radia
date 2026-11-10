use std::{collections::BTreeSet, sync::Arc};

use crate::{
    Category, CategoryKey, FolderType, InventoryError, InventoryNodeId, InventoryOwner, InventoryRevision, Item, ItemKey,
    MAX_FOLDER_FETCH_CHILDREN, MAX_INVENTORY_CATEGORIES, MAX_INVENTORY_DESCRIPTION_BYTES, MAX_INVENTORY_ITEMS, MAX_INVENTORY_NAME_BYTES,
};
use imbl::{HashMap, OrdSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryRoots {
    personal: CategoryKey,
    library: Option<CategoryKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryLimits {
    pub max_categories: usize,
    pub max_items: usize,
    pub max_folder_fetch_children: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_categories: MAX_INVENTORY_CATEGORIES,
            max_items: MAX_INVENTORY_ITEMS,
            max_folder_fetch_children: MAX_FOLDER_FETCH_CHILDREN,
        }
    }
}

impl InventoryRoots {
    pub const fn personal(self) -> CategoryKey {
        self.personal
    }

    pub const fn library(self) -> Option<CategoryKey> {
        self.library
    }

    pub fn contains(self, key: CategoryKey) -> bool {
        self.personal == key || self.library == Some(key)
    }
}

#[derive(Clone)]
pub(crate) struct InventoryState {
    pub(crate) revision: InventoryRevision,
    pub(crate) roots: InventoryRoots,
    pub(crate) categories: HashMap<CategoryKey, Arc<Category>>,
    pub(crate) folder_generations: HashMap<CategoryKey, InventoryRevision>,
    pub(crate) owner_refresh_generations: HashMap<InventoryOwner, u64>,
    pub(crate) folder_refresh_generations: HashMap<CategoryKey, u64>,
    pub(crate) items: HashMap<ItemKey, Arc<Item>>,
    pub(crate) children: HashMap<CategoryKey, OrdSet<InventoryNodeId>>,
    pub(crate) incomplete_categories: HashMap<InventoryOwner, usize>,
    pub(crate) categories_by_type: HashMap<(InventoryOwner, FolderType), OrdSet<CategoryKey>>,
    pub(crate) preferred_category_by_type: HashMap<(InventoryOwner, FolderType), CategoryKey>,
}

#[derive(Clone)]
pub struct Inventory {
    pub(crate) state: Arc<InventoryState>,
    pub(crate) limits: InventoryLimits,
}

impl Inventory {
    pub fn new(personal_root: Category, library_root: Option<Category>, limits: InventoryLimits) -> Result<Self, InventoryError> {
        validate_category(&personal_root)?;
        if personal_root.parent.is_some() {
            return Err(InventoryError::InvalidRoot);
        }
        if let Some(root) = &library_root {
            validate_category(root)?;
            if root.parent.is_some() {
                return Err(InventoryError::InvalidRoot);
            }
            if root.key == personal_root.key {
                return Err(InventoryError::DuplicateRoots);
            }
            if root.key.owner() == personal_root.key.owner() {
                return Err(InventoryError::SharedRootOwner);
            }
        }
        if limits.max_categories == 0 || (library_root.is_some() && limits.max_categories < 2) {
            return Err(InventoryError::TooManyCategories);
        }
        let roots = InventoryRoots { personal: personal_root.key, library: library_root.as_ref().map(|root| root.key) };
        let mut state = InventoryState {
            revision: InventoryRevision(0),
            roots,
            categories: HashMap::new(),
            folder_generations: HashMap::new(),
            owner_refresh_generations: HashMap::new(),
            folder_refresh_generations: HashMap::new(),
            items: HashMap::new(),
            children: HashMap::new(),
            incomplete_categories: HashMap::new(),
            categories_by_type: HashMap::new(),
            preferred_category_by_type: HashMap::new(),
        };
        insert_category_record(&mut state, personal_root)?;
        if let Some(root) = library_root {
            insert_category_record(&mut state, root)?;
        }
        Ok(Self { state: Arc::new(state), limits })
    }

    pub fn from_categories(
        personal_root: Category,
        library_root: Option<Category>,
        categories: Vec<Category>,
        limits: InventoryLimits,
    ) -> Result<Self, InventoryError> {
        let mut inventory = Self::new(personal_root, library_root, limits)?;
        inventory.install_categories(categories)?;
        Ok(inventory)
    }

    pub fn revision(&self) -> InventoryRevision {
        self.state.revision
    }

    pub fn invalidate_owner(&mut self, owner: InventoryOwner) -> Result<(), InventoryError> {
        self.invalidate_owners(std::iter::once(owner))
    }

    pub fn invalidate_all_owners(&mut self) -> Result<(), InventoryError> {
        let roots = self.state.roots;
        self.invalidate_owners(std::iter::once(roots.personal().owner()).chain(roots.library().map(|root| root.owner())))
    }

    fn invalidate_owners(&mut self, owners: impl IntoIterator<Item = InventoryOwner>) -> Result<(), InventoryError> {
        let mut next = self.state.as_ref().clone();
        let mut invalidated = false;
        for owner in owners {
            let refresh_generation = next.owner_refresh_generations.get(&owner).copied().unwrap_or_default();
            let refresh_generation = refresh_generation.checked_add(1).ok_or(InventoryError::RevisionExhausted)?;
            let category_count = next
                .categories_by_type
                .iter()
                .filter(|((category_owner, _), _)| *category_owner == owner)
                .try_fold(0usize, |count, (_, categories)| count.checked_add(categories.len()))
                .ok_or(InventoryError::InconsistentIndex)?;
            next.owner_refresh_generations.insert(owner, refresh_generation);
            if category_count == 0 {
                next.incomplete_categories.remove(&owner);
            } else {
                next.incomplete_categories.insert(owner, category_count);
            }
            invalidated = true;
        }
        if !invalidated {
            return Ok(());
        }
        let revision = self.state.revision.0.checked_add(1).ok_or(InventoryError::RevisionExhausted)?;
        next.revision = InventoryRevision(revision);
        self.state = Arc::new(next);
        Ok(())
    }

    pub fn roots(&self) -> InventoryRoots {
        self.state.roots
    }

    pub fn snapshot(&self) -> crate::InventorySnapshot {
        crate::InventorySnapshot { state: Arc::clone(&self.state), limits: self.limits }
    }

    pub(crate) fn limits(&self) -> InventoryLimits {
        self.limits
    }

    pub(crate) fn publish(
        &mut self,
        next: InventoryState,
        changes: Vec<crate::InventoryChange>,
    ) -> Result<crate::ChangeSet, InventoryError> {
        self.publish_with_refetch(next, changes, Vec::new())
    }

    pub(crate) fn publish_with_refetch(
        &mut self,
        next: InventoryState,
        changes: Vec<crate::InventoryChange>,
        refetch_categories: Vec<CategoryKey>,
    ) -> Result<crate::ChangeSet, InventoryError> {
        self.publish_with_refetch_cancellable(next, changes, refetch_categories, None)
    }

    pub(crate) fn publish_cancellable(
        &mut self,
        next: InventoryState,
        changes: Vec<crate::InventoryChange>,
        cancellation: &crate::InventoryPreparationCancellation,
    ) -> Result<crate::ChangeSet, InventoryError> {
        self.publish_with_refetch_cancellable(next, changes, Vec::new(), Some(cancellation))
    }

    fn publish_with_refetch_cancellable(
        &mut self,
        mut next: InventoryState,
        changes: Vec<crate::InventoryChange>,
        refetch_categories: Vec<CategoryKey>,
        cancellation: Option<&crate::InventoryPreparationCancellation>,
    ) -> Result<crate::ChangeSet, InventoryError> {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        if changes.is_empty() {
            return Ok(crate::ChangeSet::new(self.state.revision, changes, refetch_categories));
        }
        let revision = self.state.revision.0.checked_add(1).ok_or(InventoryError::RevisionExhausted)?;
        let revision = InventoryRevision(revision);
        next.revision = revision;
        let mut touched = BTreeSet::new();
        for (index, change) in changes.iter().enumerate() {
            if index.is_multiple_of(128)
                && let Some(cancellation) = cancellation
            {
                cancellation.check()?;
            }
            match change {
                crate::InventoryChange::CategoryUpserted(category) => {
                    touched.insert(category.key);
                    if let Some(parent) = self.state.categories.get(&category.key).and_then(|old| old.parent) {
                        touched.insert(parent);
                    }
                    if let Some(parent) = category.parent {
                        touched.insert(parent);
                    }
                }
                crate::InventoryChange::ItemUpserted(item) => {
                    if let Some(old) = self.state.items.get(&item.key) {
                        touched.insert(old.parent);
                    }
                    touched.insert(item.parent);
                }
                crate::InventoryChange::NodeMoved { from, to, .. } => {
                    touched.insert(*from);
                    touched.insert(*to);
                }
                crate::InventoryChange::CategoryRemoved(key) => {
                    if let Some(parent) = self.state.categories.get(key).and_then(|category| category.parent) {
                        touched.insert(parent);
                    }
                    next.folder_generations.remove(key);
                }
                crate::InventoryChange::ItemRemoved(key) => {
                    if let Some(item) = self.state.items.get(key) {
                        touched.insert(item.parent);
                    }
                }
                crate::InventoryChange::FolderFetchApplied { folder, .. } | crate::InventoryChange::VersionSet { folder, .. } => {
                    touched.insert(*folder);
                }
            }
        }
        for folder in touched {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            if next.categories.contains_key(&folder) {
                next.folder_generations.insert(folder, revision);
            } else {
                next.folder_generations.remove(&folder);
            }
        }
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let changes = crate::ChangeSet::new(revision, changes, refetch_categories);
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        self.state = Arc::new(next);
        Ok(changes)
    }

    fn install_categories(&mut self, categories: impl IntoIterator<Item = Category>) -> Result<(), InventoryError> {
        let mut next = self.state.as_ref().clone();
        for category in categories {
            validate_category(&category)?;
            if next.categories.contains_key(&category.key) {
                return Err(InventoryError::DuplicateCategory);
            }
            if next.categories.len() >= self.limits.max_categories {
                return Err(InventoryError::TooManyCategories);
            }
            let Some(parent) = category.parent else {
                return Err(InventoryError::InvalidRoot);
            };
            if parent.owner() != category.key.owner() || !next.categories.contains_key(&parent) {
                return Err(InventoryError::MissingParent);
            }
            insert_child(&mut next, parent, InventoryNodeId::Category(category.key.id()))?;
            replace_category_record(&mut next, category)?;
        }
        self.state = Arc::new(next);
        Ok(())
    }
}

pub(crate) fn insert_category_record(state: &mut InventoryState, category: Category) -> Result<(), InventoryError> {
    if state.categories.contains_key(&category.key) {
        return Err(InventoryError::InconsistentIndex);
    }
    state.children.insert(category.key, OrdSet::new());
    replace_category_record(state, category)
}

pub(crate) fn replace_category_record(state: &mut InventoryState, category: Category) -> Result<(), InventoryError> {
    let key = category.key;
    if !state.children.contains_key(&key) {
        state.children.insert(key, OrdSet::new());
    }
    state.children.get(&key).ok_or(InventoryError::InconsistentIndex)?;
    let previous = state.categories.get(&key).cloned();
    let was_complete = category_is_complete(state, key);
    if !state.folder_refresh_generations.contains_key(&key) {
        let refresh_generation = state.owner_refresh_generations.get(&key.owner()).copied().unwrap_or_default();
        state.folder_refresh_generations.insert(key, refresh_generation);
    }
    if let Some(previous) = previous.as_ref()
        && previous.preferred_folder_type != category.preferred_folder_type
    {
        remove_category_type(state, key, previous.preferred_folder_type)?;
    }
    if previous.as_ref().is_none_or(|previous| previous.preferred_folder_type != category.preferred_folder_type) {
        insert_category_type(state, key, category.preferred_folder_type)?;
    }
    state.categories.insert(key, Arc::new(category));
    if !state.folder_generations.contains_key(&key) {
        state.folder_generations.insert(key, state.revision);
    }
    update_incomplete_count(state, key.owner(), previous.as_ref().is_some_and(|_| !was_complete), !category_is_complete(state, key))?;
    Ok(())
}

pub(crate) fn remove_category_record(state: &mut InventoryState, key: CategoryKey) -> Result<(), InventoryError> {
    let category = state.categories.get(&key).cloned().ok_or(InventoryError::InconsistentIndex)?;
    let children = state.children.get(&key).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if !children.is_empty() {
        return Err(InventoryError::InconsistentIndex);
    }
    let was_complete = category_is_complete(state, key);
    update_incomplete_count(state, key.owner(), !was_complete, false)?;
    remove_category_type(state, key, category.preferred_folder_type)?;
    state.categories.remove(&key);
    state.children.remove(&key);
    state.folder_refresh_generations.remove(&key);
    Ok(())
}

pub(crate) fn insert_child(state: &mut InventoryState, parent: CategoryKey, child: InventoryNodeId) -> Result<(), InventoryError> {
    state.categories.get(&parent).ok_or(InventoryError::InconsistentIndex)?;
    let mut children = state.children.get(&parent).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if children.contains(&child) {
        return Err(InventoryError::InconsistentIndex);
    }
    let was_complete = category_is_complete(state, parent);
    children.insert(child);
    state.children.insert(parent, children);
    update_incomplete_count(state, parent.owner(), !was_complete, !category_is_complete(state, parent))?;
    Ok(())
}

pub(crate) fn remove_child(state: &mut InventoryState, parent: CategoryKey, child: InventoryNodeId) -> Result<(), InventoryError> {
    state.categories.get(&parent).ok_or(InventoryError::InconsistentIndex)?;
    let mut children = state.children.get(&parent).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if !children.contains(&child) {
        return Err(InventoryError::InconsistentIndex);
    }
    let was_complete = category_is_complete(state, parent);
    children.remove(&child);
    state.children.insert(parent, children);
    update_incomplete_count(state, parent.owner(), !was_complete, !category_is_complete(state, parent))?;
    Ok(())
}

pub(crate) fn category_is_complete(state: &InventoryState, key: CategoryKey) -> bool {
    let Some(category) = state.categories.get(&key) else { return false };
    state.folder_refresh_generations.get(&key).copied().unwrap_or_default()
        == state.owner_refresh_generations.get(&key.owner()).copied().unwrap_or_default()
        && category.server_version.is_known()
        && category.server_descendent_count.and_then(|count| usize::try_from(count).ok())
            == state.children.get(&key).map(|children| children.len())
}

pub(crate) fn update_incomplete_count(
    state: &mut InventoryState,
    owner: InventoryOwner,
    was_incomplete: bool,
    is_incomplete: bool,
) -> Result<(), InventoryError> {
    if was_incomplete == is_incomplete {
        return Ok(());
    }
    let count = state.incomplete_categories.get(&owner).copied().unwrap_or_default();
    if is_incomplete {
        let updated = count.checked_add(1).ok_or(InventoryError::InconsistentIndex)?;
        state.incomplete_categories.insert(owner, updated);
    } else {
        let updated = count.checked_sub(1).ok_or(InventoryError::InconsistentIndex)?;
        if updated == 0 {
            state.incomplete_categories.remove(&owner);
        } else {
            state.incomplete_categories.insert(owner, updated);
        }
    }
    Ok(())
}

fn insert_category_type(state: &mut InventoryState, key: CategoryKey, folder_type: FolderType) -> Result<(), InventoryError> {
    let type_key = (key.owner(), folder_type);
    let mut categories = state.categories_by_type.get(&type_key).cloned().unwrap_or_default();
    if categories.contains(&key) {
        return Err(InventoryError::InconsistentIndex);
    }
    categories.insert(key);
    state.categories_by_type.insert(type_key, categories);
    if state.preferred_category_by_type.get(&type_key).is_none_or(|preferred| key < *preferred) {
        state.preferred_category_by_type.insert(type_key, key);
    }
    Ok(())
}

fn remove_category_type(state: &mut InventoryState, key: CategoryKey, folder_type: FolderType) -> Result<(), InventoryError> {
    let type_key = (key.owner(), folder_type);
    let mut categories = state.categories_by_type.get(&type_key).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if !categories.contains(&key) {
        return Err(InventoryError::InconsistentIndex);
    }
    categories.remove(&key);
    if categories.is_empty() {
        state.categories_by_type.remove(&type_key);
        state.preferred_category_by_type.remove(&type_key);
    } else {
        if state.preferred_category_by_type.get(&type_key) == Some(&key) {
            let preferred = categories.iter().next().copied().ok_or(InventoryError::InconsistentIndex)?;
            state.preferred_category_by_type.insert(type_key, preferred);
        }
        state.categories_by_type.insert(type_key, categories);
    }
    Ok(())
}

pub(crate) fn validate_category(category: &Category) -> Result<(), InventoryError> {
    if category.key.owner().id().is_nil() {
        return Err(InventoryError::InvalidOwner);
    }
    if category.key.id().uuid().is_nil() {
        return Err(InventoryError::InvalidCategoryId);
    }
    if category.name.len() > MAX_INVENTORY_NAME_BYTES {
        return Err(InventoryError::NameTooLong);
    }
    if category.parent.is_some_and(|parent| parent.owner() != category.key.owner()) {
        return Err(InventoryError::InvalidParent);
    }
    Ok(())
}

pub(crate) fn validate_item(item: &Item) -> Result<(), InventoryError> {
    if item.key.owner().id().is_nil() {
        return Err(InventoryError::InvalidOwner);
    }
    if item.key.id().uuid().is_nil() {
        return Err(InventoryError::InvalidItemId);
    }
    if item.parent.owner() != item.key.owner() {
        return Err(InventoryError::InvalidParent);
    }
    if item.name.len() > MAX_INVENTORY_NAME_BYTES {
        return Err(InventoryError::NameTooLong);
    }
    if item.description.len() > MAX_INVENTORY_DESCRIPTION_BYTES {
        return Err(InventoryError::DescriptionTooLong);
    }
    Ok(())
}
