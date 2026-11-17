use std::mem::size_of;

use math::Frustum;
use wgpu::Device;

use super::{GeometryRenderer, MAX_MESH_UPLOADS, UPLOAD_BYTES};
use crate::geometry::storage::Allocation;
use crate::pipeline::VERTEX_BYTES;
use crate::scene::{Camera, MeshId, SharedMesh};

pub(super) struct GpuMesh {
    pub(super) allocation: Allocation,
}

#[derive(Debug)]
pub(super) enum MeshUpload {
    Ready,
    Deferred,
    CapacityBlocked,
}

enum MeshRetirement {
    Retired,
    Deferred,
    CapacityBlocked,
}

impl GeometryRenderer {
    pub(super) fn upload_mesh(
        &mut self,
        device: &Device,
        shared: &SharedMesh,
        composition: Option<&[f32]>,
        uploaded: &mut usize,
        eviction: Option<(Camera, &Frustum)>,
    ) -> MeshUpload {
        let key = shared.id();
        if self.meshes.contains_key(&key) {
            self.mesh_budget_blocked.remove(&key);
            return MeshUpload::Ready;
        }
        let mesh = shared.mesh();
        let Some(bytes) = mesh_upload_bytes(shared, composition) else { return MeshUpload::CapacityBlocked };
        if *uploaded >= MAX_MESH_UPLOADS || (bytes > UPLOAD_BYTES.saturating_sub(self.stats.uploaded_bytes) && *uploaded > 0) {
            return MeshUpload::Deferred;
        }
        let Some(allocation) =
            self.storage.allocate(device, mesh.vertices().len(), mesh.indices().len(), composition.is_some(), &self.memory)
        else {
            let retirement = match eviction {
                Some((camera, frustum)) => self.retire_mesh(camera, frustum),
                None => MeshRetirement::CapacityBlocked,
            };
            self.mesh_budget_blocked.insert(key);
            return match retirement {
                MeshRetirement::Retired | MeshRetirement::Deferred => MeshUpload::Deferred,
                MeshRetirement::CapacityBlocked if !self.retired.is_empty() => MeshUpload::Deferred,
                MeshRetirement::CapacityBlocked => MeshUpload::CapacityBlocked,
            };
        };
        let mut packed = Vec::with_capacity(bytes);
        for vertex in mesh.vertices() {
            floats(&mut packed, vertex.position.to_array());
            floats(&mut packed, vertex.normal.to_array());
            floats(&mut packed, vertex.uv.to_array());
        }
        self.queue.write_buffer(&allocation.buffers().vertices, u64::from(allocation.vertices().start) * VERTEX_BYTES as u64, &packed);
        if let Some(composition) = composition {
            let mut packed = Vec::with_capacity(std::mem::size_of_val(composition));
            floats_slice(&mut packed, composition);
            let buffer = allocation.buffers().composition.as_ref().expect("composition allocation has a terrain buffer");
            self.queue.write_buffer(buffer, u64::from(allocation.vertices().start) * size_of::<f32>() as u64, &packed);
        }
        packed.clear();
        for index in mesh.indices() {
            packed.extend_from_slice(&index.to_le_bytes());
        }
        self.queue.write_buffer(&allocation.buffers().indices, u64::from(allocation.indices().start) * size_of::<u32>() as u64, &packed);
        self.meshes.insert(key, GpuMesh { allocation });
        self.mesh_budget_blocked.remove(&key);
        self.stats.uploaded_bytes += bytes;
        *uploaded += 1;
        MeshUpload::Ready
    }

    fn retire_mesh(&mut self, camera: Camera, frustum: &Frustum) -> MeshRetirement {
        if self.retired.len() == 4 {
            return MeshRetirement::Deferred;
        }
        let evictable = |id: &MeshId| {
            self.meshes.contains_key(id) && !self.cache.admission_protected.contains(id) && !self.draw_list.shows_mesh(*id, camera, frustum)
        };
        let unwanted = self.meshes.keys().find(|id| !self.cache.mesh_order_by_id.contains_key(*id) && evictable(id));
        let victim = unwanted
            .copied()
            .or_else(|| self.cache.mesh_order.iter().rev().filter(|demand| !demand.visible).map(|demand| demand.mesh).find(evictable));
        let Some((old, mesh)) = victim.and_then(|old| self.meshes.remove_entry(&old)) else { return MeshRetirement::CapacityBlocked };
        self.draw_list.evict(old);
        self.cache.admission_protected.remove(&old);
        self.pending_meshes.remove(&old);
        self.mesh_budget_blocked.remove(&old);
        let vertices = mesh.allocation.vertices().len();
        let bytes = vertices
            .saturating_mul(VERTEX_BYTES)
            .saturating_add(mesh.allocation.indices().len().saturating_mul(size_of::<u32>()))
            .saturating_add(if mesh.allocation.buffers().composition.is_some() { vertices.saturating_mul(size_of::<f32>()) } else { 0 });
        self.stats.gpu_mesh_retirements += 1;
        self.stats.gpu_mesh_retired_bytes = self.stats.gpu_mesh_retired_bytes.saturating_add(bytes);
        self.retired.push(mesh);
        MeshRetirement::Retired
    }
}

pub(super) fn mesh_upload_bytes(shared: &SharedMesh, composition: Option<&[f32]>) -> Option<usize> {
    let mesh = shared.mesh();
    if composition.is_some_and(|values| values.len() != mesh.vertices().len()) {
        return None;
    }
    mesh.vertices()
        .len()
        .checked_mul(VERTEX_BYTES)
        .and_then(|vertices| mesh.indices().len().checked_mul(4).and_then(|indices| vertices.checked_add(indices)))
        .and_then(|bytes| composition.map_or(Some(bytes), |values| values.len().checked_mul(size_of::<f32>())?.checked_add(bytes)))
}

fn floats<const N: usize>(bytes: &mut Vec<u8>, values: [f32; N]) {
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}

fn floats_slice(bytes: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
#[path = "mesh_tests.rs"]
mod tests;
