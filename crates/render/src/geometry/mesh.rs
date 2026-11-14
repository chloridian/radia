use std::mem::size_of;

use wgpu::Device;

use super::{GeometryRenderer, MAX_MESH_UPLOADS, UPLOAD_BYTES};
use crate::geometry::storage::Allocation;
use crate::pipeline::VERTEX_BYTES;
use crate::scene::SharedMesh;

pub(super) struct GpuMesh {
    pub(super) allocation: Allocation,
    pub(super) used: u64,
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
        allow_eviction: bool,
    ) -> MeshUpload {
        let key = shared.id();
        if let Some(mesh) = self.meshes.get_mut(&key) {
            mesh.used = self.epoch;
            self.mesh_budget_blocked.remove(&key);
            return MeshUpload::Ready;
        }
        let mesh = shared.mesh();
        let Some(bytes) = mesh_upload_bytes(shared, composition) else { return MeshUpload::CapacityBlocked };
        if *uploaded >= MAX_MESH_UPLOADS
            || (bytes > UPLOAD_BYTES.saturating_sub(self.stats.uploaded_bytes) && (*uploaded > 0 || !allow_eviction))
        {
            return MeshUpload::Deferred;
        }
        let Some(allocation) =
            self.storage.allocate(device, mesh.vertices().len(), mesh.indices().len(), composition.is_some(), &self.memory)
        else {
            let retirement = if allow_eviction { self.retire_mesh() } else { MeshRetirement::CapacityBlocked };
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
        self.meshes.insert(key, GpuMesh { allocation, used: self.epoch });
        self.mesh_budget_blocked.remove(&key);
        self.stats.uploaded_bytes += bytes;
        *uploaded += 1;
        MeshUpload::Ready
    }

    fn retire_mesh(&mut self) -> MeshRetirement {
        if self.retired.len() == 4 {
            return MeshRetirement::Deferred;
        }
        let mut oldest = None;
        let mut used_this_epoch = false;
        for (id, mesh) in &self.meshes {
            if self.cache.admission_protected.contains(id) || self.draw_list.pins_mesh(*id) {
                continue;
            }
            if mesh.used == self.epoch {
                used_this_epoch = true;
                continue;
            }
            if oldest.is_none_or(|(_, used)| mesh.used < used) {
                oldest = Some((*id, mesh.used));
            }
        }
        let Some((old, _)) = oldest else {
            return if used_this_epoch { MeshRetirement::Deferred } else { MeshRetirement::CapacityBlocked };
        };
        let mesh = self.meshes.remove(&old).expect("selected resident");
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
