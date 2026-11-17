use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use crate::{
    Category, CategoryKey, ChangeSet, Inventory, InventoryChange, InventoryDelta, InventoryError, InventoryNodeId, InventoryNodeKey,
    ServerVersion,
    mutation::{remove_category_subtree, remove_item, upsert_item_state},
    state::{InventoryState, insert_child, remove_child, replace_category_record, validate_category},
};

struct CategoryAncestry<'a> {
    proposed: &'a HashMap<CategoryKey, Category>,
    state: &'a InventoryState,
    resolved: HashSet<CategoryKey>,
    active: HashSet<CategoryKey>,
    path: Vec<CategoryKey>,
    #[cfg(test)]
    visited_nodes: usize,
}

impl<'a> CategoryAncestry<'a> {
    fn new(proposed: &'a HashMap<CategoryKey, Category>, state: &'a InventoryState, capacity: usize) -> Self {
        Self {
            proposed,
            state,
            resolved: HashSet::with_capacity(capacity),
            active: HashSet::with_capacity(capacity),
            path: Vec::with_capacity(capacity),
            #[cfg(test)]
            visited_nodes: 0,
        }
    }

    fn validate_all(&mut self, ordered_keys: &[CategoryKey]) -> Result<(), InventoryError> {
        for start in ordered_keys {
            self.active.clear();
            self.path.clear();
            let mut current = *start;
            loop {
                if self.resolved.contains(&current) {
                    break;
                }
                if !self.active.insert(current) {
                    return Err(InventoryError::Cycle);
                }
                #[cfg(test)]
                {
                    self.visited_nodes += 1;
                }
                let category = self
                    .proposed
                    .get(&current)
                    .or_else(|| self.state.categories.get(&current).map(Arc::as_ref))
                    .ok_or(InventoryError::MissingParent)?;
                self.path.push(current);
                match category.parent {
                    Some(parent) => current = parent,
                    None if self.state.roots.contains(current) => {
                        self.resolved.insert(current);
                        break;
                    }
                    None => return Err(InventoryError::InvalidParent),
                }
            }
            self.resolved.extend(self.path.drain(..));
        }
        Ok(())
    }
}

impl Inventory {
    pub fn apply_delta(&mut self, delta: InventoryDelta) -> Result<ChangeSet, InventoryError> {
        validate_delta_size(&delta, self.limits.max_categories, self.limits.max_items)?;

        let previous = self.state.as_ref();
        let mut next = previous.clone();
        let mut changes = Vec::new();
        let mut refetch = BTreeSet::new();

        let stale_versions = delta
            .updated_category_versions
            .iter()
            .filter_map(|(folder, version)| {
                let current = previous.categories.get(folder)?;
                (current.server_version.is_known() && version.is_known() && version.raw() < current.server_version.raw()).then_some(*folder)
            })
            .collect::<BTreeSet<_>>();
        if !stale_versions.is_empty() {
            for folder in stale_versions {
                if let Some(version) = next.categories.get(&folder).map(|category| category.server_version) {
                    replace_folder_metadata(&mut next, folder, version, None, &mut changes)?;
                    refetch.insert(folder);
                }
            }
            return self.publish_with_refetch(next, changes, refetch.into_iter().collect());
        }

        let created_categories = delta
            .created_categories
            .iter()
            .chain(&delta.updated_categories)
            .map(|category| category.key)
            .filter(|folder| !previous.categories.contains_key(folder))
            .collect::<BTreeSet<_>>();
        apply_categories(&mut next, &delta.created_categories, &delta.updated_categories, &mut changes)?;
        for item in delta.created_items.iter().chain(&delta.updated_items) {
            upsert_item_state(&mut next, item.clone(), &mut changes)?;
        }
        for key in &delta.removed_items {
            remove_item(&mut next, *key, &mut changes)?;
        }
        for key in &delta.removed_categories {
            if next.categories.contains_key(key) {
                if next.roots.contains(*key) {
                    return Err(InventoryError::FixedRoot);
                }
                remove_category_subtree(&mut next, *key, &mut changes)?;
            }
        }

        let touched = touched_parents(previous, &next, &delta);
        for folder in &touched {
            if previous.categories.contains_key(folder) {
                account_local_update(previous, &mut next, *folder, &mut changes, &mut refetch)?;
            }
        }
        for (folder, version) in &delta.updated_category_versions {
            apply_authoritative_version(previous, &mut next, *folder, *version, touched.contains(folder), &mut changes, &mut refetch)?;
        }
        for folder in created_categories {
            if next.categories.contains_key(&folder) {
                validate_new_folder(&mut next, folder, &mut changes, &mut refetch)?;
            }
        }

        let categories = next.categories.len();
        let items = next.items.len();
        if categories > self.limits.max_categories {
            return Err(InventoryError::TooManyCategories);
        }
        if items > self.limits.max_items {
            return Err(InventoryError::TooManyItems);
        }
        self.publish_with_refetch(next, changes, refetch.into_iter().collect())
    }
}

fn validate_delta_size(delta: &InventoryDelta, max_categories: usize, max_items: usize) -> Result<(), InventoryError> {
    let category_upserts =
        delta.created_categories.len().checked_add(delta.updated_categories.len()).ok_or(InventoryError::TooManyDeltaOperations)?;
    let item_upserts = delta.created_items.len().checked_add(delta.updated_items.len()).ok_or(InventoryError::TooManyDeltaOperations)?;
    if category_upserts > max_categories
        || delta.removed_categories.len() > max_categories
        || delta.updated_category_versions.len() > max_categories
        || item_upserts > max_items
        || delta.removed_items.len() > max_items
    {
        return Err(InventoryError::TooManyDeltaOperations);
    }
    Ok(())
}

fn apply_categories(
    state: &mut InventoryState,
    created: &[Category],
    updated: &[Category],
    changes: &mut Vec<InventoryChange>,
) -> Result<(), InventoryError> {
    let mut categories = HashMap::with_capacity(created.len().saturating_add(updated.len()));
    let mut ordered_keys = Vec::with_capacity(created.len().saturating_add(updated.len()));
    for input in created.iter().chain(updated) {
        validate_category(input)?;
        let mut category = input.clone();
        if let Some(current) = state.categories.get(&category.key) {
            category.server_version = current.server_version;
            category.server_descendent_count = current.server_descendent_count;
        }
        if !categories.contains_key(&category.key) {
            ordered_keys.push(category.key);
        }
        categories.insert(category.key, category);
    }

    for key in &ordered_keys {
        let category = categories.get(key).ok_or(InventoryError::InconsistentIndex)?;
        if state.roots.contains(category.key) {
            if category.parent.is_some() {
                return Err(InventoryError::InvalidRoot);
            }
        } else {
            let parent = category.parent.ok_or(InventoryError::InvalidParent)?;
            if !categories.contains_key(&parent) && !state.categories.contains_key(&parent) {
                return Err(InventoryError::MissingParent);
            }
        }
    }

    CategoryAncestry::new(&categories, state, ordered_keys.len()).validate_all(&ordered_keys)?;

    let categories = ordered_keys
        .into_iter()
        .map(|key| categories.remove(&key).ok_or(InventoryError::InconsistentIndex))
        .collect::<Result<Vec<_>, _>>()?;
    let previous = categories.iter().map(|category| (category.clone(), state.categories.get(&category.key).cloned())).collect::<Vec<_>>();

    for (category, current) in &previous {
        if let Some(current) = current
            && current.parent != category.parent
            && let Some(parent) = current.parent
        {
            remove_child(state, parent, InventoryNodeId::Category(category.key.id()))?;
        }
    }
    for (category, current) in &previous {
        if current.as_deref() != Some(category) {
            replace_category_record(state, category.clone())?;
        }
    }
    for (category, current) in &previous {
        if current.as_ref().is_none_or(|current| current.parent != category.parent)
            && let Some(parent) = category.parent
        {
            insert_child(state, parent, InventoryNodeId::Category(category.key.id()))?;
        }
    }
    for (category, current) in previous {
        if let Some(from) = current.as_ref().and_then(|current| current.parent)
            && current.as_ref().is_some_and(|current| current.parent != category.parent)
            && let Some(to) = category.parent
        {
            changes.push(InventoryChange::NodeMoved {
                node: InventoryNodeKey::new(category.key.owner(), InventoryNodeId::Category(category.key.id())),
                from,
                to,
            });
        }
        if current.as_deref() != Some(&category) {
            let category = state.categories.get(&category.key).cloned().ok_or(InventoryError::InconsistentIndex)?;
            changes.push(InventoryChange::CategoryUpserted(category));
        }
    }
    Ok(())
}

fn touched_parents(previous: &InventoryState, next: &InventoryState, delta: &InventoryDelta) -> BTreeSet<CategoryKey> {
    let mut touched = BTreeSet::new();
    for key in delta
        .created_categories
        .iter()
        .chain(&delta.updated_categories)
        .map(|category| category.key)
        .chain(delta.removed_categories.iter().copied())
    {
        let before = previous.categories.get(&key);
        let after = next.categories.get(&key);
        add_changed_parents(&mut touched, next, before.and_then(|category| category.parent), after.and_then(|category| category.parent));
    }
    for key in delta.created_items.iter().chain(&delta.updated_items).map(|item| item.key).chain(delta.removed_items.iter().copied()) {
        let before = previous.items.get(&key);
        let after = next.items.get(&key);
        add_changed_parents(&mut touched, next, before.map(|item| item.parent), after.map(|item| item.parent));
    }
    touched
}

fn add_changed_parents(
    touched: &mut BTreeSet<CategoryKey>,
    next: &InventoryState,
    before: Option<CategoryKey>,
    after: Option<CategoryKey>,
) {
    if before != after {
        for parent in before.into_iter().chain(after) {
            if next.categories.contains_key(&parent) {
                touched.insert(parent);
            }
        }
    }
}

fn account_local_update(
    previous: &InventoryState,
    next: &mut InventoryState,
    folder: CategoryKey,
    changes: &mut Vec<InventoryChange>,
    refetch: &mut BTreeSet<CategoryKey>,
) -> Result<(), InventoryError> {
    let before = previous.categories.get(&folder).ok_or(InventoryError::InconsistentIndex)?;
    let before_children = previous.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
    let after_children = next.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
    let count_was_known = before.server_version.is_known()
        && before.server_descendent_count.and_then(|count| usize::try_from(count).ok()) == Some(before_children);
    let next_version = count_was_known
        .then(|| before.server_version.raw().checked_add(1).map(ServerVersion::from_raw))
        .flatten()
        .filter(|version| version.is_known());
    let next_count = count_was_known.then(|| u32::try_from(after_children).ok()).flatten();
    let version = next_version.unwrap_or(before.server_version);
    let next_count = next_count.filter(|count| usize::try_from(*count).ok() == Some(after_children));
    replace_folder_metadata(next, folder, version, next_count, changes)?;
    if next_version.is_none() || next_count.is_none() {
        refetch.insert(folder);
    }
    Ok(())
}

fn validate_new_folder(
    state: &mut InventoryState,
    folder: CategoryKey,
    changes: &mut Vec<InventoryChange>,
    refetch: &mut BTreeSet<CategoryKey>,
) -> Result<(), InventoryError> {
    let category = state.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
    let actual_count = state.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
    let count_matches = category.server_descendent_count.and_then(|count| usize::try_from(count).ok()) == Some(actual_count);
    if !category.server_version.is_known() || !count_matches {
        replace_folder_metadata(state, folder, category.server_version, None, changes)?;
        refetch.insert(folder);
    }
    Ok(())
}

fn apply_authoritative_version(
    previous: &InventoryState,
    next: &mut InventoryState,
    folder: CategoryKey,
    version: ServerVersion,
    locally_touched: bool,
    changes: &mut Vec<InventoryChange>,
    refetch: &mut BTreeSet<CategoryKey>,
) -> Result<(), InventoryError> {
    let Some(current) = next.categories.get(&folder).cloned() else { return Ok(()) };
    let was_created = !previous.categories.contains_key(&folder);
    if !version.is_known() {
        replace_folder_metadata(next, folder, current.server_version, None, changes)?;
        refetch.insert(folder);
        return Ok(());
    }
    if was_created {
        let child_count = next.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
        let count_matches = current.server_descendent_count.and_then(|count| usize::try_from(count).ok()) == Some(child_count);
        let count = (version.is_known() && count_matches).then_some(current.server_descendent_count).flatten();
        replace_folder_metadata(next, folder, version, count, changes)?;
        if !version.is_known() || count.is_none() {
            refetch.insert(folder);
        }
        return Ok(());
    }

    let predicted = current.server_version;
    let matches_local = locally_touched && predicted.is_known() && predicted == version;
    if matches_local {
        if !folder_is_complete(next, folder)? {
            refetch.insert(folder);
        }
        replace_folder_metadata(next, folder, version, current.server_descendent_count, changes)?;
        return Ok(());
    }

    if !locally_touched && current.server_version == version {
        return Ok(());
    }

    replace_folder_metadata(next, folder, version, None, changes)?;
    refetch.insert(folder);
    Ok(())
}

fn folder_is_complete(state: &InventoryState, folder: CategoryKey) -> Result<bool, InventoryError> {
    let category = state.categories.get(&folder).ok_or(InventoryError::InconsistentIndex)?;
    let children = state.children.get(&folder).ok_or(InventoryError::InconsistentIndex)?.len();
    Ok(category.server_version.is_known()
        && category.server_descendent_count.and_then(|count| usize::try_from(count).ok()) == Some(children))
}

fn replace_folder_metadata(
    state: &mut InventoryState,
    folder: CategoryKey,
    version: ServerVersion,
    descendent_count: Option<u32>,
    changes: &mut Vec<InventoryChange>,
) -> Result<(), InventoryError> {
    let current = state.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if current.server_version == version && current.server_descendent_count == descendent_count {
        return Ok(());
    }
    let mut updated = current.as_ref().clone();
    updated.server_version = version;
    updated.server_descendent_count = descendent_count;
    replace_category_record(state, updated)?;
    let updated = state.categories.get(&folder).cloned().ok_or(InventoryError::InconsistentIndex)?;
    if current.server_version != version {
        changes.push(InventoryChange::VersionSet { folder, version });
    }
    changes.push(InventoryChange::CategoryUpserted(updated));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use utils::FolderId;
    use uuid::Uuid;

    use super::CategoryAncestry;
    use crate::{Category, CategoryKey, FolderType, Inventory, InventoryLimits, InventoryOwner, ServerVersion};

    fn category(owner: InventoryOwner, id: u128, parent: Option<u128>) -> Category {
        Category {
            key: CategoryKey::new(owner, FolderId::new(Uuid::from_u128(id))),
            parent: parent.map(|parent| CategoryKey::new(owner, FolderId::new(Uuid::from_u128(parent)))),
            preferred_folder_type: FolderType::None,
            name: Arc::from("folder"),
            server_version: ServerVersion::UNKNOWN,
            server_descendent_count: None,
        }
    }

    fn chain(owner: InventoryOwner, first_id: u128, count: u128, root_id: u128) -> Vec<Category> {
        (0..count)
            .map(|index| {
                let id = first_id + index;
                let parent = if index == 0 { root_id } else { id - 1 };
                category(owner, id, Some(parent))
            })
            .collect()
    }

    #[test]
    fn chain_validation_is_linear() {
        let owner = InventoryOwner::new(Uuid::from_u128(1));
        let root = category(owner, 10, None);
        let inventory = Inventory::new(root, None, InventoryLimits::default()).expect("root is valid");
        let proposed = chain(owner, 1_000, 1_024, 10);
        let proposed_by_key = proposed.iter().cloned().map(|category| (category.key, category)).collect::<HashMap<_, _>>();
        let parent_first = proposed.iter().map(|category| category.key).collect::<Vec<_>>();
        let reverse = parent_first.iter().rev().copied().collect::<Vec<_>>();
        let graph_nodes = proposed.len() + 1;

        for ordered_keys in [parent_first, reverse] {
            let mut ancestry = CategoryAncestry::new(&proposed_by_key, &inventory.state, ordered_keys.len());
            ancestry.validate_all(&ordered_keys).expect("chain reaches its root");
            assert!(ancestry.visited_nodes <= graph_nodes * 2, "visited {} nodes for {graph_nodes} graph nodes", ancestry.visited_nodes);
        }
    }

    #[test]
    fn shared_ancestry_is_reused() {
        let owner = InventoryOwner::new(Uuid::from_u128(1));
        let root = category(owner, 10, None);
        let existing = chain(owner, 100_000, 512, 10);
        let parent_id = 100_511;
        let inventory =
            Inventory::from_categories(root, None, existing.clone(), InventoryLimits::default()).expect("existing chain is valid");
        let proposed = (0..512_u128).map(|index| category(owner, 200_000 + index, Some(parent_id))).collect::<Vec<_>>();
        let proposed_by_key = proposed.iter().cloned().map(|category| (category.key, category)).collect::<HashMap<_, _>>();
        let ordered_keys = proposed.iter().map(|category| category.key).collect::<Vec<_>>();
        let graph_nodes = existing.len() + proposed.len() + 1;
        let mut ancestry = CategoryAncestry::new(&proposed_by_key, &inventory.state, ordered_keys.len());

        ancestry.validate_all(&ordered_keys).expect("each candidate reaches the shared root");

        assert!(ancestry.visited_nodes <= graph_nodes * 2, "visited {} nodes for {graph_nodes} graph nodes", ancestry.visited_nodes);
    }
}
