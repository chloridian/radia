use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use assets::{Failure, Image, TextureDecodeJob, TextureHandle, TextureId, TextureUsage, WeakTexture, mesh::Mesh};
use world::Entity;

use crate::scene::{
    Budget, Detail, DrawnMeshKey, MeshBuildError, SculptMeshKey, SharedMesh,
    budget::MeshWork,
    mesh::retained_bytes,
    sculpt::{image_to_rgba, sculpt_placeholder_mesh, tessellate_sculpt_cancellable},
};

enum SculptInput {
    Image(Image),
    Decode(TextureDecodeJob),
}

pub(in crate::scene) struct SculptJob {
    key: SculptMeshKey,
    input: SculptInput,
    work: MeshWork,
    cancelled: Arc<AtomicBool>,
}

impl SculptJob {
    pub(in crate::scene) fn run(self) -> SculptCompletion {
        let result = if self.cancelled.load(Ordering::Acquire) {
            Err(MeshBuildError::Cancelled)
        } else {
            utils::isolate(|| {
                let image = match self.input {
                    SculptInput::Image(image) => Ok(image),
                    SculptInput::Decode(job) => job.run().map_err(|failure| match failure {
                        Failure::Cancelled => MeshBuildError::Cancelled,
                        _ => MeshBuildError::InvalidParameters,
                    }),
                };
                image.and_then(|image| {
                    let rgba = image_to_rgba(&image, &self.cancelled)?;
                    tessellate_sculpt_cancellable(
                        image.size(),
                        &rgba,
                        self.key.sculpt_type,
                        self.key.flags,
                        self.key.detail,
                        &self.cancelled,
                    )
                })
            })
            .unwrap_or(Err(MeshBuildError::Panicked))
        };
        SculptCompletion { key: self.key, result, work: self.work, cancelled: self.cancelled }
    }
}

pub(in crate::scene) struct SculptCompletion {
    key: SculptMeshKey,
    result: Result<Mesh, MeshBuildError>,
    work: MeshWork,
    cancelled: Arc<AtomicBool>,
}

pub(super) enum SculptResolution {
    Pending,
    Placeholder,
    Ready(DrawnMeshKey, SharedMesh),
}

enum SculptState {
    Queued,
    Running(Weak<AtomicBool>),
    Ready(SharedMesh),
    Failed,
}

struct SculptEntry {
    users: HashSet<Entity>,
    priority: u32,
    texture: WeakTexture,
    state: SculptState,
}

#[derive(Default)]
pub(super) struct SculptAssets {
    placeholder: Option<SharedMesh>,
    entries: HashMap<SculptMeshKey, SculptEntry>,
    entity_key: HashMap<Entity, SculptMeshKey>,
    entity_texture: HashMap<Entity, TextureId>,
    texture_users: HashMap<TextureId, (WeakTexture, usize)>,
}

impl SculptAssets {
    pub(super) fn placeholder(&mut self, budget: &Budget) -> Option<SharedMesh> {
        if let Some(placeholder) = &self.placeholder {
            return Some(placeholder.clone());
        }
        let mesh = sculpt_placeholder_mesh().ok()?;
        let lease = budget.reserve(retained_bytes(&mesh))?;
        let shared = SharedMesh::new(mesh, lease);
        self.placeholder = Some(shared.clone());
        Some(shared)
    }

    pub(super) fn previous_detail(&self, entity: Entity) -> Option<Detail> {
        self.entity_key.get(&entity).map(|key| key.detail)
    }

    pub(super) fn request(&mut self, entity: Entity, key: SculptMeshKey, texture: &TextureHandle, priority: u32) -> SculptResolution {
        self.track_texture(entity, texture);
        if self.entity_key.get(&entity) != Some(&key) {
            self.detach_key(entity);
            self.entity_key.insert(entity, key);
            let entry = self.entries.entry(key).or_insert_with(|| SculptEntry {
                users: HashSet::new(),
                priority,
                texture: texture.downgrade(),
                state: SculptState::Queued,
            });
            entry.users.insert(entity);
            entry.priority = entry.priority.max(priority);
            entry.texture = texture.downgrade();
        } else if let Some(entry) = self.entries.get_mut(&key) {
            entry.priority = entry.priority.max(priority);
            entry.texture = texture.downgrade();
        }
        match self.entries.get(&key).map(|entry| &entry.state) {
            Some(SculptState::Ready(mesh)) => SculptResolution::Ready(DrawnMeshKey::Sculpt(key), mesh.clone()),
            Some(SculptState::Failed) => SculptResolution::Placeholder,
            Some(SculptState::Queued | SculptState::Running(_)) | None => SculptResolution::Pending,
        }
    }

    pub(super) fn detach_key(&mut self, entity: Entity) {
        let Some(key) = self.entity_key.remove(&entity) else { return };
        let empty = self.entries.get_mut(&key).is_some_and(|entry| {
            entry.users.remove(&entity);
            entry.users.is_empty()
        });
        if empty && let Some(entry) = self.entries.remove(&key) {
            cancel_state(&entry.state);
        }
    }

    pub(super) fn untrack_texture(&mut self, entity: Entity) {
        let Some(id) = self.entity_texture.remove(&entity) else { return };
        let remove = self.texture_users.get_mut(&id).is_some_and(|(_, count)| {
            *count = count.saturating_sub(1);
            *count == 0
        });
        if remove
            && let Some((weak, _)) = self.texture_users.remove(&id)
            && let Some(texture) = weak.upgrade()
        {
            texture.set_usage(TextureUsage::RenderOnly);
        }
    }

    pub(super) fn remove_entity(&mut self, entity: Entity) {
        self.detach_key(entity);
        self.untrack_texture(entity);
    }

    pub(super) fn clear(&mut self) {
        for entry in self.entries.values() {
            cancel_state(&entry.state);
        }
        for (weak, _) in self.texture_users.values() {
            if let Some(texture) = weak.upgrade() {
                texture.set_usage(TextureUsage::RenderOnly);
            }
        }
        self.entries.clear();
        self.entity_key.clear();
        self.entity_texture.clear();
        self.texture_users.clear();
        self.placeholder = None;
    }

    pub(super) fn start(&mut self, budget: &Budget) -> Option<SculptJob> {
        for entry in self.entries.values_mut() {
            if let SculptState::Running(cancelled) = &entry.state
                && cancelled.upgrade().is_none()
            {
                entry.state = SculptState::Queued;
            }
        }
        let selected = self
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                if !matches!(entry.state, SculptState::Queued) {
                    return None;
                }
                let texture = entry.texture.upgrade()?;
                Some((*key, entry.priority, texture))
            })
            .max_by_key(|(_, priority, _)| *priority);
        let (key, _, texture) = selected?;
        let input = if let Some(image) = texture.image() { SculptInput::Image(image) } else { SculptInput::Decode(texture.decode_job()?) };
        let work = budget.mesh_work()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.state = SculptState::Running(Arc::downgrade(&cancelled));
        }
        Some(SculptJob { key, input, work, cancelled })
    }

    pub(super) fn complete(&mut self, mut completion: SculptCompletion) -> Vec<Entity> {
        let Some(entry) = self.entries.get_mut(&completion.key) else {
            return Vec::new();
        };
        if !matches!(
            &entry.state,
            SculptState::Running(cancelled) if Weak::ptr_eq(cancelled, &Arc::downgrade(&completion.cancelled))
        ) {
            return Vec::new();
        }
        match completion.result {
            Ok(mesh) => {
                let bytes = retained_bytes(&mesh);
                if let Some(lease) = completion.work.lease.split(bytes) {
                    entry.state = SculptState::Ready(SharedMesh::new(mesh, lease));
                } else {
                    entry.state = SculptState::Failed;
                }
            }
            Err(MeshBuildError::Cancelled) => {
                entry.state = SculptState::Queued;
                return Vec::new();
            }
            Err(_) => {
                entry.state = SculptState::Failed;
            }
        }
        entry.users.iter().copied().collect()
    }

    pub(super) fn waiting(&self) -> bool {
        self.entries.values().any(|entry| matches!(entry.state, SculptState::Queued | SculptState::Running(_)))
    }

    fn track_texture(&mut self, entity: Entity, texture: &TextureHandle) {
        let id = texture.id();
        if self.entity_texture.get(&entity) == Some(&id) {
            return;
        }
        self.untrack_texture(entity);
        self.entity_texture.insert(entity, id);
        let entry = self.texture_users.entry(id).or_insert_with(|| {
            texture.set_usage(TextureUsage::MainAndRender);
            (texture.downgrade(), 0)
        });
        entry.1 += 1;
    }
}

impl Drop for SculptAssets {
    fn drop(&mut self) {
        self.clear();
    }
}

fn cancel_state(state: &SculptState) {
    if let SculptState::Running(cancelled) = state
        && let Some(cancelled) = cancelled.upgrade()
    {
        cancelled.store(true, Ordering::Release);
    }
}
