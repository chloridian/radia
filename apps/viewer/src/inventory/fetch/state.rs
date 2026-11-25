use std::collections::{HashSet, VecDeque};

use inventory::{CategoryKey, InventorySnapshot};

use super::{MAX_AIS_BATCH, MAX_LEGACY_BATCH};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct BackgroundRequest {
    pub(super) folder: CategoryKey,
    pub(super) depth: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SourceKind {
    Ais,
    Legacy,
    Unavailable,
}

pub(super) struct BackgroundPlan {
    pub(super) parent: Option<CategoryKey>,
    pub(super) folders: Vec<CategoryKey>,
    pub(super) depth: u8,
}

pub(super) fn next_background_job(
    background: &mut VecDeque<BackgroundRequest>,
    queued: &mut HashSet<CategoryKey>,
    snapshot: &InventorySnapshot,
    in_flight: &HashSet<CategoryKey>,
    failed: &HashSet<CategoryKey>,
    source_kind: impl Fn(inventory::InventoryOwner) -> SourceKind,
) -> Option<BackgroundPlan> {
    let mut index = 0;
    while index < background.len() {
        let request = background[index];
        if snapshot.is_complete(request.folder) || failed.contains(&request.folder) {
            background.remove(index);
            queued.remove(&request.folder);
            continue;
        }
        if in_flight.contains(&request.folder) {
            index += 1;
            continue;
        }
        let request = background.remove(index)?;
        queued.remove(&request.folder);
        match source_kind(request.folder.owner()) {
            SourceKind::Ais => {
                let parent = if request.depth > 0 { snapshot.category(request.folder).and_then(|category| category.parent) } else { None };
                let mut folders = vec![request.folder];
                if let Some(parent_key) = parent {
                    let mut next_index = index;
                    while folders.len() < MAX_AIS_BATCH && next_index < background.len() {
                        let next = background[next_index];
                        if snapshot.is_complete(next.folder) || failed.contains(&next.folder) {
                            background.remove(next_index);
                            queued.remove(&next.folder);
                            continue;
                        }
                        if in_flight.contains(&next.folder)
                            || next.depth != request.depth
                            || next.folder.owner() != request.folder.owner()
                            || snapshot.category(next.folder).and_then(|category| category.parent) != Some(parent_key)
                        {
                            next_index += 1;
                            continue;
                        }
                        background.remove(next_index);
                        queued.remove(&next.folder);
                        folders.push(next.folder);
                    }
                }
                let batch_parent = (folders.len() > 1).then_some(parent).flatten();
                return Some(BackgroundPlan { parent: batch_parent, folders, depth: request.depth });
            }
            SourceKind::Legacy => {
                let mut folders = vec![request.folder];
                let mut next_index = index;
                while folders.len() < MAX_LEGACY_BATCH {
                    let Some(next) = background.get(next_index).copied() else { break };
                    if next.folder.owner() != request.folder.owner() {
                        break;
                    }
                    if snapshot.is_complete(next.folder) || failed.contains(&next.folder) {
                        background.remove(next_index);
                        queued.remove(&next.folder);
                        continue;
                    }
                    if in_flight.contains(&next.folder) {
                        next_index += 1;
                        continue;
                    }
                    background.remove(next_index);
                    queued.remove(&next.folder);
                    folders.push(next.folder);
                }
                return Some(BackgroundPlan { parent: None, folders, depth: 0 });
            }
            SourceKind::Unavailable => return Some(BackgroundPlan { parent: None, folders: vec![request.folder], depth: 0 }),
        }
    }
    None
}

pub(super) fn enqueue_background(
    request: BackgroundRequest,
    snapshot: &InventorySnapshot,
    failed: &HashSet<CategoryKey>,
    background: &mut VecDeque<BackgroundRequest>,
    queued: &mut HashSet<CategoryKey>,
) {
    if snapshot.is_complete(request.folder) || failed.contains(&request.folder) || !queued.insert(request.folder) {
        return;
    }
    background.push_back(request);
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
