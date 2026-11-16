use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use utils::FolderId;

use crate::state::{
    InventoryState, category_is_complete, insert_child, remove_category_record, remove_child, replace_category_record, validate_category,
    validate_item,
};
use crate::{
    Category, CategoryKey, ChangeSet, Inventory, InventoryChange, InventoryError, InventoryLimits, InventoryNodeId, InventoryNodeKey,
    InventoryOwner, InventoryRevision, InventorySnapshot, Item, ItemKey, ServerVersion,
};

pub const MAX_FOLDER_FETCH_CHILDREN: usize = 100_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryChild {
    Category(Category),
    Item(Item),
}

pub(crate) enum CachedFolderApply {
    Skipped,
    Applied { conflict: bool },
}

pub struct PreparedFolderFetch {
    folder: CategoryKey,
    base_revision: InventoryRevision,
    folder_generation: InventoryRevision,
    state: Arc<InventoryState>,
    changes: ChangeSet,
}

impl std::fmt::Debug for PreparedFolderFetch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedFolderFetch")
            .field("folder", &self.folder)
            .field("base_revision", &self.base_revision)
            .field("folder_generation", &self.folder_generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default)]
pub struct InventoryPreparationCancellation(Arc<AtomicBool>);

impl InventoryPreparationCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> Result<(), InventoryError> {
        if self.0.load(Ordering::Acquire) { Err(InventoryError::Cancelled) } else { Ok(()) }
    }
}

#[derive(Debug)]
pub enum FolderFetchCommit {
    Applied(ChangeSet),
    StaleBase(PreparedFolderFetch),
}

impl InventorySnapshot {
    pub fn prepare_folder_fetch(
        &self,
        folder: CategoryKey,
        version: ServerVersion,
        descendent_count: Option<u32>,
        children: Vec<InventoryChild>,
    ) -> Result<PreparedFolderFetch, InventoryError> {
        self.prepare_folder_fetch_cancellable(folder, version, descendent_count, children, &InventoryPreparationCancellation::default())
    }

    pub fn prepare_folder_fetch_cancellable(
        &self,
        folder: CategoryKey,
        version: ServerVersion,
        descendent_count: Option<u32>,
        children: Vec<InventoryChild>,
        cancellation: &InventoryPreparationCancellation,
    ) -> Result<PreparedFolderFetch, InventoryError> {
        let folder_generation = self.state.folder_generations.get(&folder).copied().ok_or(InventoryError::MissingCategory)?;
        let mut candidate = Inventory { state: Arc::clone(&self.state), limits: self.limits };
        let changes = candidate.apply_folder_fetch_state(folder, version, descendent_count, children, cancellation)?;
        Ok(PreparedFolderFetch { folder, base_revision: self.state.revision, folder_generation, state: candidate.state, changes })
    }
}

impl InventoryChild {
    fn node_key(&self) -> InventoryNodeKey {
        match self {
            Self::Category(category) => InventoryNodeKey::new(category.key.owner(), InventoryNodeId::Category(category.key.id())),
            Self::Item(item) => InventoryNodeKey::new(item.key.owner(), InventoryNodeId::Item(item.key.id())),
        }
    }

    fn parent(&self) -> CategoryKey {
        match self {
            Self::Category(category) => category.parent.unwrap_or(category.key),
            Self::Item(item) => item.parent,
        }
    }
}

impl Inventory {
    pub fn commit_folder_fetch(&mut self, prepared: PreparedFolderFetch) -> Result<FolderFetchCommit, InventoryError> {
        if self.state.revision != prepared.base_revision
            || self.state.folder_generations.get(&prepared.folder).copied() != Some(prepared.folder_generation)
        {
            return Ok(FolderFetchCommit::StaleBase(prepared));
        }
        self.state = prepared.state;
        Ok(FolderFetchCommit::Applied(prepared.changes))
    }

    pub(crate) fn apply_cached_folder(
        &mut self,
        metadata: &Category,
        children: &[InventoryChild],
        cached_complete: bool,
        expected_child_count: usize,
        final_chunk: bool,
    ) -> Result<CachedFolderApply, InventoryError> {
        let folder = metadata.key;
        let Some(current_folder) = self.state.categories.get(&folder) else { return Ok(CachedFolderApply::Skipped) };
        if category_is_complete(&self.state, folder)
            || (current_folder.server_version.is_known() && current_folder.server_version != metadata.server_version)
        {
            return Ok(CachedFolderApply::Skipped);
        }
        if children.len() > self.limits.max_folder_fetch_children {
            return Err(InventoryError::TooManyFolderChildren);
        }
        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        let mut conflict = false;
        for child in children {
            if child.node_key().owner() != folder.owner() || child.parent() != folder {
                return Err(InventoryError::InvalidParent);
            }
            match child {
                InventoryChild::Category(category) => {
                    validate_category(category)?;
                    if self.state.roots.contains(category.key) {
                        return Err(InventoryError::FixedRoot);
                    }
                    match self.state.categories.get(&category.key) {
                        Some(current) if current.parent == Some(folder) => continue,
                        Some(_) => {
                            conflict = true;
                            continue;
                        }
                        None => {}
                    }
                    upsert_category_state(&mut next, category.clone(), &mut changes)?;
                }
                InventoryChild::Item(item) => {
                    validate_item(item)?;
                    match self.state.items.get(&item.key) {
                        Some(current) if current.parent == folder => continue,
                        Some(_) => {
                            conflict = true;
                            continue;
                        }
                        None => {}
                    }
                    upsert_item_state(&mut next, item.clone(), &mut changes)?;
                }
            }
        }
        if next.categories.len() > self.limits.max_categories {
            return Err(InventoryError::TooManyCategories);
        }
        if next.items.len() > self.limits.max_items {
            return Err(InventoryError::TooManyItems);
        }
        let child_count = next.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
        let descendent_count = if final_chunk && cached_complete && !conflict && child_count == expected_child_count {
            u32::try_from(expected_child_count).ok()
        } else {
            None
        };
        let current = next.categories.get(&folder).ok_or(InventoryError::InconsistentIndex)?;
        if current.server_descendent_count != descendent_count {
            let mut updated = current.as_ref().clone();
            updated.server_descendent_count = descendent_count;
            replace_category_record(&mut next, updated)?;
            let updated = next.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
            changes.push(InventoryChange::CategoryUpserted(updated));
        }
        self.publish(next, changes)?;
        Ok(CachedFolderApply::Applied { conflict })
    }

    pub fn upsert_category(&mut self, category: Category) -> Result<ChangeSet, InventoryError> {
        validate_category(&category)?;
        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        upsert_category_state(&mut next, category, &mut changes)?;
        if next.categories.len() > self.limits.max_categories {
            return Err(InventoryError::TooManyCategories);
        }
        self.publish(next, changes)
    }

    pub fn upsert_item(&mut self, item: Item) -> Result<ChangeSet, InventoryError> {
        validate_item(&item)?;
        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        upsert_item_state(&mut next, item, &mut changes)?;
        if next.items.len() > self.limits.max_items {
            return Err(InventoryError::TooManyItems);
        }
        self.publish(next, changes)
    }

    pub fn move_node(&mut self, key: InventoryNodeKey, parent: CategoryKey) -> Result<ChangeSet, InventoryError> {
        if key.owner() != parent.owner() {
            return Err(InventoryError::InvalidParent);
        }
        if !self.state.categories.contains_key(&parent) {
            return Err(InventoryError::MissingParent);
        }
        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        match key.id() {
            InventoryNodeId::Category(id) => {
                let category_key = CategoryKey::new(key.owner(), id);
                if self.state.roots.contains(category_key) {
                    return Err(InventoryError::FixedRoot);
                }
                let mut category =
                    next.categories.get(&category_key).map(|category| category.as_ref().clone()).ok_or(InventoryError::MissingCategory)?;
                category.parent = Some(parent);
                upsert_category_state(&mut next, category, &mut changes)?;
            }
            InventoryNodeId::Item(id) => {
                let item_key = ItemKey::new(key.owner(), id);
                let mut item = next.items.get(&item_key).map(|item| item.as_ref().clone()).ok_or(InventoryError::MissingItem)?;
                item.parent = parent;
                upsert_item_state(&mut next, item, &mut changes)?;
            }
        }
        self.publish(next, changes)
    }

    pub fn remove_subtree(&mut self, owner: InventoryOwner, folder_id: FolderId) -> Result<ChangeSet, InventoryError> {
        let root = CategoryKey::new(owner, folder_id);
        if self.state.roots.contains(root) {
            return Err(InventoryError::FixedRoot);
        }
        if !self.state.categories.contains_key(&root) {
            return self.publish(self.state.as_ref().clone(), Vec::new());
        }
        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        remove_category_subtree(&mut next, root, &mut changes)?;
        self.publish(next, changes)
    }

    fn apply_folder_fetch_state(
        &mut self,
        folder: CategoryKey,
        version: ServerVersion,
        descendent_count: Option<u32>,
        children: Vec<InventoryChild>,
        cancellation: &InventoryPreparationCancellation,
    ) -> Result<ChangeSet, InventoryError> {
        cancellation.check()?;
        if children.len() > self.limits.max_folder_fetch_children {
            return Err(InventoryError::TooManyFolderChildren);
        }
        if !self.state.categories.contains_key(&folder) {
            return Err(InventoryError::MissingCategory);
        }
        let current_version = self.state.categories.get(&folder).ok_or(InventoryError::InconsistentIndex)?.server_version;
        if current_version.is_known() && version.is_known() && version.raw() < current_version.raw() {
            return self.publish(self.state.as_ref().clone(), Vec::new());
        }
        let mut incoming = HashSet::with_capacity(children.len());
        let mut new_categories = 0usize;
        let mut new_items = 0usize;
        for (index, child) in children.iter().enumerate() {
            if index.is_multiple_of(128) {
                cancellation.check()?;
            }
            if child.node_key().owner() != folder.owner() || child.parent() != folder {
                return Err(InventoryError::InvalidParent);
            }
            if !incoming.insert(child.node_key()) {
                return Err(InventoryError::DuplicateFetchChild);
            }
            match child {
                InventoryChild::Category(category) => {
                    validate_category(category)?;
                    if self.state.roots.contains(category.key) {
                        return Err(InventoryError::FixedRoot);
                    }
                }
                InventoryChild::Item(item) => validate_item(item)?,
            }
            match child {
                InventoryChild::Category(category) if !self.state.categories.contains_key(&category.key) => {
                    new_categories = new_categories.checked_add(1).ok_or(InventoryError::TooManyCategories)?;
                }
                InventoryChild::Item(item) if !self.state.items.contains_key(&item.key) => {
                    new_items = new_items.checked_add(1).ok_or(InventoryError::TooManyItems)?;
                }
                _ => {}
            }
        }

        let old_children = self.state.children.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
        let mut removal = SubtreeRemoval::default();
        for (index, stale) in old_children.iter().copied().enumerate() {
            if index.is_multiple_of(128) {
                cancellation.check()?;
            }
            let key = InventoryNodeKey::new(folder.owner(), stale);
            if incoming.contains(&key) {
                continue;
            }
            match stale {
                InventoryNodeId::Category(id) => {
                    removal.collect_category(&self.state, CategoryKey::new(folder.owner(), id), &incoming, Some(cancellation))?;
                }
                InventoryNodeId::Item(id) => {
                    removal.collect_item(&self.state, ItemKey::new(folder.owner(), id), folder, &incoming, Some(cancellation))?;
                }
            }
        }
        check_final_capacity(&self.state, &removal, new_categories, new_items, self.limits)?;

        let mut next = self.state.as_ref().clone();
        let mut changes = Vec::new();
        let preserved = std::mem::take(&mut removal.preserved);
        if !preserved.is_empty() {
            for (index, child) in children.iter().enumerate() {
                if index.is_multiple_of(128) {
                    cancellation.check()?;
                }
                if !preserved.contains(&child.node_key()) {
                    continue;
                }
                match child {
                    InventoryChild::Category(category) => upsert_category_state(&mut next, category.clone(), &mut changes)?,
                    InventoryChild::Item(item) => upsert_item_state(&mut next, item.clone(), &mut changes)?,
                }
            }
        }
        removal.apply(&mut next, &mut changes, Some(cancellation))?;
        for (index, child) in children.into_iter().enumerate() {
            if index.is_multiple_of(128) {
                cancellation.check()?;
            }
            if preserved.contains(&child.node_key()) {
                continue;
            }
            match child {
                InventoryChild::Category(category) => upsert_category_state(&mut next, category, &mut changes)?,
                InventoryChild::Item(item) => upsert_item_state(&mut next, item, &mut changes)?,
            }
        }

        let mut updated_folder =
            next.categories.get(&folder).map(|category| category.as_ref().clone()).ok_or(InventoryError::InconsistentIndex)?;
        updated_folder.server_version = version;
        updated_folder.server_descendent_count = descendent_count;
        let previous = next.categories.get(&folder).ok_or(InventoryError::InconsistentIndex)?;
        if previous.as_ref() != &updated_folder {
            replace_category_record(&mut next, updated_folder)?;
            let updated_folder = next.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
            changes.push(InventoryChange::CategoryUpserted(updated_folder));
        }
        let was_incomplete = !category_is_complete(&next, folder);
        let refresh_generation = next.owner_refresh_generations.get(&folder.owner()).copied().unwrap_or_default();
        next.folder_refresh_generations.insert(folder, refresh_generation);
        let is_incomplete = !category_is_complete(&next, folder);
        crate::state::update_incomplete_count(&mut next, folder.owner(), was_incomplete, is_incomplete)?;
        changes.push(InventoryChange::FolderFetchApplied { folder, child_count: incoming.len() });
        self.publish_cancellable(next, changes, cancellation)
    }

    pub fn set_version(&mut self, folder: CategoryKey, version: ServerVersion) -> Result<ChangeSet, InventoryError> {
        let Some(current) = self.state.categories.get(&folder) else {
            return Err(InventoryError::MissingCategory);
        };
        if current.server_version == version {
            return self.publish(self.state.as_ref().clone(), Vec::new());
        }
        let mut next = self.state.as_ref().clone();
        let mut updated = current.as_ref().clone();
        updated.server_version = version;
        replace_category_record(&mut next, updated)?;
        let updated = next.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
        self.publish(next, vec![InventoryChange::VersionSet { folder, version }, InventoryChange::CategoryUpserted(updated)])
    }
}

pub(crate) fn upsert_category_state(
    state: &mut InventoryState,
    category: Category,
    changes: &mut Vec<InventoryChange>,
) -> Result<(), InventoryError> {
    validate_category(&category)?;
    let key = category.key;
    let previous = state.categories.get(&key).cloned();
    if state.roots.contains(key) {
        if category.parent.is_some() {
            return Err(InventoryError::InvalidRoot);
        }
    } else {
        let parent = category.parent.ok_or(InventoryError::InvalidParent)?;
        if parent.owner() != key.owner() || !state.categories.contains_key(&parent) {
            return Err(InventoryError::MissingParent);
        }
        if previous.as_ref().is_some_and(|previous| previous.parent != category.parent) {
            ensure_no_cycle(state, key, parent)?;
        }
    }
    if previous.as_deref() == Some(&category) {
        return Ok(());
    }
    let parent_changed = previous.as_ref().is_some_and(|previous| previous.parent != category.parent);
    if parent_changed && let Some(old_parent) = previous.as_ref().and_then(|previous| previous.parent) {
        remove_child(state, old_parent, InventoryNodeId::Category(key.id()))?;
    }
    if let Some(parent) = category.parent
        && (previous.is_none() || parent_changed)
    {
        insert_child(state, parent, InventoryNodeId::Category(key.id()))?;
        if let Some(old_parent) = previous.as_ref().and_then(|previous| previous.parent) {
            changes.push(InventoryChange::NodeMoved {
                node: InventoryNodeKey::new(key.owner(), InventoryNodeId::Category(key.id())),
                from: old_parent,
                to: parent,
            });
        }
    }
    replace_category_record(state, category)?;
    let category = state.categories.get(&key).cloned().ok_or(InventoryError::InconsistentIndex)?;
    changes.push(InventoryChange::CategoryUpserted(category));
    Ok(())
}

pub(crate) fn upsert_item_state(state: &mut InventoryState, item: Item, changes: &mut Vec<InventoryChange>) -> Result<(), InventoryError> {
    validate_item(&item)?;
    if !state.categories.contains_key(&item.parent) {
        return Err(InventoryError::MissingParent);
    }
    let key = item.key;
    let previous = state.items.get(&key).cloned();
    if previous.as_deref() == Some(&item) {
        return Ok(());
    }
    if let Some(previous) = &previous
        && previous.parent != item.parent
    {
        remove_child(state, previous.parent, InventoryNodeId::Item(key.id()))?;
        changes.push(InventoryChange::NodeMoved {
            node: InventoryNodeKey::new(key.owner(), InventoryNodeId::Item(key.id())),
            from: previous.parent,
            to: item.parent,
        });
    }
    if previous.as_ref().is_none_or(|previous| previous.parent != item.parent) {
        insert_child(state, item.parent, InventoryNodeId::Item(key.id()))?;
    }
    let item = Arc::new(item);
    state.items.insert(key, Arc::clone(&item));
    changes.push(InventoryChange::ItemUpserted(item));
    Ok(())
}

fn ensure_no_cycle(state: &InventoryState, key: CategoryKey, mut parent: CategoryKey) -> Result<(), InventoryError> {
    let mut visited = HashSet::new();
    loop {
        if parent == key || !visited.insert(parent) {
            return Err(InventoryError::Cycle);
        }
        let category = state.categories.get(&parent).ok_or(InventoryError::MissingParent)?;
        match category.parent {
            Some(ancestor) => parent = ancestor,
            None => return Ok(()),
        }
    }
}

pub(crate) fn remove_item(state: &mut InventoryState, key: ItemKey, changes: &mut Vec<InventoryChange>) -> Result<(), InventoryError> {
    let Some(item) = state.items.get(&key).cloned() else { return Ok(()) };
    remove_child(state, item.parent, InventoryNodeId::Item(key.id()))?;
    state.items.remove(&key);
    changes.push(InventoryChange::ItemRemoved(key));
    Ok(())
}

pub(crate) fn remove_category_subtree(
    state: &mut InventoryState,
    root: CategoryKey,
    changes: &mut Vec<InventoryChange>,
) -> Result<(), InventoryError> {
    let mut removal = SubtreeRemoval::default();
    removal.collect_category(state, root, &HashSet::new(), None)?;
    removal.apply(state, changes, None)
}

#[derive(Default)]
struct SubtreeRemoval {
    categories: Vec<CategoryKey>,
    items: Vec<ItemKey>,
    visited: HashSet<InventoryNodeId>,
    preserved: HashSet<InventoryNodeKey>,
}

impl SubtreeRemoval {
    fn collect_category(
        &mut self,
        state: &InventoryState,
        root: CategoryKey,
        preserved: &HashSet<InventoryNodeKey>,
        cancellation: Option<&InventoryPreparationCancellation>,
    ) -> Result<(), InventoryError> {
        if state.roots.contains(root) {
            return Err(InventoryError::FixedRoot);
        }
        let category = state.categories.get(&root).ok_or(InventoryError::InconsistentIndex)?;
        let parent = category.parent.ok_or(InventoryError::InvalidRoot)?;
        let mut pending = vec![(InventoryNodeId::Category(root.id()), parent)];
        let mut visited_count = 0usize;
        while let Some((node, expected_parent)) = pending.pop() {
            if visited_count.is_multiple_of(128)
                && let Some(cancellation) = cancellation
            {
                cancellation.check()?;
            }
            visited_count = visited_count.saturating_add(1);
            if !self.visited.insert(node) {
                return Err(InventoryError::Cycle);
            }
            let node_key = InventoryNodeKey::new(root.owner(), node);
            if preserved.contains(&node_key) {
                self.preserved.insert(node_key);
                continue;
            }
            match node {
                InventoryNodeId::Category(id) => {
                    let key = CategoryKey::new(root.owner(), id);
                    let category = state.categories.get(&key).ok_or(InventoryError::InconsistentIndex)?;
                    if category.parent != Some(expected_parent) {
                        return Err(InventoryError::InconsistentIndex);
                    }
                    self.categories.push(key);
                    let children = state.children.get(&key).ok_or(InventoryError::InconsistentIndex)?;
                    for (index, child) in children.iter().enumerate() {
                        if index.is_multiple_of(128)
                            && let Some(cancellation) = cancellation
                        {
                            cancellation.check()?;
                        }
                        pending.push((*child, key));
                    }
                }
                InventoryNodeId::Item(id) => {
                    let key = ItemKey::new(root.owner(), id);
                    let item = state.items.get(&key).ok_or(InventoryError::InconsistentIndex)?;
                    if item.parent != expected_parent {
                        return Err(InventoryError::InconsistentIndex);
                    }
                    self.items.push(key);
                }
            }
        }
        Ok(())
    }

    fn collect_item(
        &mut self,
        state: &InventoryState,
        key: ItemKey,
        parent: CategoryKey,
        preserved: &HashSet<InventoryNodeKey>,
        cancellation: Option<&InventoryPreparationCancellation>,
    ) -> Result<(), InventoryError> {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let node = InventoryNodeId::Item(key.id());
        if !self.visited.insert(node) {
            return Err(InventoryError::Cycle);
        }
        let node_key = InventoryNodeKey::new(key.owner(), node);
        if preserved.contains(&node_key) {
            self.preserved.insert(node_key);
            return Ok(());
        }
        let item = state.items.get(&key).ok_or(InventoryError::InconsistentIndex)?;
        if item.parent != parent {
            return Err(InventoryError::InconsistentIndex);
        }
        self.items.push(key);
        Ok(())
    }

    fn apply(
        self,
        state: &mut InventoryState,
        changes: &mut Vec<InventoryChange>,
        cancellation: Option<&InventoryPreparationCancellation>,
    ) -> Result<(), InventoryError> {
        for (index, key) in self.items.into_iter().enumerate() {
            if index.is_multiple_of(128)
                && let Some(cancellation) = cancellation
            {
                cancellation.check()?;
            }
            remove_item(state, key, changes)?;
        }
        for (index, key) in self.categories.into_iter().rev().enumerate() {
            if index.is_multiple_of(128)
                && let Some(cancellation) = cancellation
            {
                cancellation.check()?;
            }
            let category = state.categories.get(&key).cloned().ok_or(InventoryError::InconsistentIndex)?;
            if let Some(parent) = category.parent {
                remove_child(state, parent, InventoryNodeId::Category(key.id()))?;
            }
            remove_category_record(state, key)?;
            changes.push(InventoryChange::CategoryRemoved(key));
        }
        Ok(())
    }
}

fn check_final_capacity(
    state: &InventoryState,
    removal: &SubtreeRemoval,
    new_categories: usize,
    new_items: usize,
    limits: InventoryLimits,
) -> Result<(), InventoryError> {
    let categories_after_removal = state.categories.len().checked_sub(removal.categories.len()).ok_or(InventoryError::InconsistentIndex)?;
    let items_after_removal = state.items.len().checked_sub(removal.items.len()).ok_or(InventoryError::InconsistentIndex)?;
    let category_count = categories_after_removal.checked_add(new_categories).ok_or(InventoryError::TooManyCategories)?;
    let item_count = items_after_removal.checked_add(new_items).ok_or(InventoryError::TooManyItems)?;
    if category_count > limits.max_categories {
        return Err(InventoryError::TooManyCategories);
    }
    if item_count > limits.max_items {
        return Err(InventoryError::TooManyItems);
    }
    Ok(())
}
