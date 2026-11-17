use std::sync::Arc;

use math::{DVec3, Mat4, Quat, Vec3, normal_matrix};
use wgpu::{
    Buffer, BufferUsages, Device, Queue,
    util::{BufferInitDescriptor, DeviceExt},
};

use super::{Accounting, Lease, TRANSFORM_BYTES, buffer, draw_list::INDIRECT_DRAW_BYTES};
use crate::{
    material::FaceParams,
    pipeline::{FACE_INDEX_BYTES, INSTANCE_BYTES},
};

pub(super) const PARAMETER_BYTES: usize = INSTANCE_BYTES - TRANSFORM_BYTES;
pub(super) const INITIAL_FACE_INSTANCES: usize = 16_384;
pub(super) const SLOT_BYTES: usize = INSTANCE_BYTES + FACE_INDEX_BYTES + INDIRECT_DRAW_BYTES;

pub(super) struct InstanceBuffers {
    pub(super) instances: Buffer,
    pub(super) face_indices: Buffer,
    pub(super) indirect: Buffer,
    capacity: usize,
    cap: usize,
    growths: usize,
    lease: Lease,
}

impl InstanceBuffers {
    pub(super) fn new(device: &Device, memory: &Arc<Accounting>, capacity: usize) -> Self {
        let limits = device.limits();
        let bytes = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
        let cap = usize::try_from(bytes).unwrap_or(usize::MAX) / INSTANCE_BYTES;
        let capacity = capacity.min(cap);
        let (instances, face_indices, indirect) = allocate(device, capacity);
        let lease = memory.charge(capacity * SLOT_BYTES);
        Self { instances, face_indices, indirect, capacity, cap, growths: 0, lease }
    }

    pub(super) fn cap(&self) -> usize {
        self.cap
    }

    pub(super) fn bytes(&self) -> usize {
        self.capacity * INSTANCE_BYTES
    }

    pub(super) fn growths(&self) -> usize {
        self.growths
    }

    pub(super) fn fit(&mut self, device: &Device, queue: &Queue, memory: &Arc<Accounting>, faces: usize) -> bool {
        if faces <= self.capacity {
            return false;
        }
        let mut capacity = self.capacity.max(1);
        while capacity < faces {
            capacity = capacity.saturating_mul(2);
        }
        let capacity = capacity.min(self.cap);
        let (instances, face_indices, indirect) = allocate(device, capacity);
        let old = (
            [
                std::mem::replace(&mut self.instances, instances),
                std::mem::replace(&mut self.face_indices, face_indices),
                std::mem::replace(&mut self.indirect, indirect),
            ],
            std::mem::replace(&mut self.lease, memory.charge(capacity * SLOT_BYTES)),
        );
        queue.on_submitted_work_done(move || drop(old));
        self.capacity = capacity;
        self.growths += 1;
        true
    }
}

fn allocate(device: &Device, capacity: usize) -> (Buffer, Buffer, Buffer) {
    let instances = buffer(device, "Face instances", capacity * INSTANCE_BYTES, BufferUsages::STORAGE);
    let face_indices: Vec<u8> = (0..u32::try_from(capacity).unwrap_or(u32::MAX)).flat_map(u32::to_le_bytes).collect();
    let face_indices = device.create_buffer_init(&BufferInitDescriptor {
        label: Some("Face indices"),
        contents: &face_indices,
        usage: BufferUsages::VERTEX,
    });
    let indirect = buffer(device, "Scene indexed draws", capacity * INDIRECT_DRAW_BYTES, BufferUsages::INDIRECT);
    (instances, face_indices, indirect)
}

pub(super) fn model(position: DVec3, rotation: Quat, scale: Vec3, origin: DVec3) -> Mat4 {
    Mat4::from_scale_rotation_translation(scale, rotation, (position - origin).as_vec3())
}

pub(super) fn scene_origin(position: DVec3) -> DVec3 {
    let snap = |coordinate: f64| (coordinate / 256.0).floor() * 256.0;
    DVec3::new(snap(position.x), snap(position.y), snap(position.z))
}

pub(super) fn instance_base(position: DVec3, rotation: Quat, scale: Vec3, invert_normals: bool, origin: DVec3) -> [u8; TRANSFORM_BYTES] {
    let mut bytes = [0; TRANSFORM_BYTES];
    let normal = normal_matrix(rotation, scale) * if invert_normals { -1.0 } else { 1.0 };
    let values = model(position, rotation, scale, origin)
        .to_cols_array()
        .into_iter()
        .chain([normal.x_axis, normal.y_axis, normal.z_axis].into_iter().flat_map(|column| column.extend(0.0).to_array()));
    for (dest, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        *dest = value.to_le_bytes();
    }
    bytes
}

#[cfg(test)]
pub(super) fn packed_instance(base: &[u8; TRANSFORM_BYTES], params: &FaceParams, scale: Vec3) -> [u8; INSTANCE_BYTES] {
    let mut bytes = [0; INSTANCE_BYTES];
    bytes[..TRANSFORM_BYTES].copy_from_slice(base);
    bytes[TRANSFORM_BYTES..].copy_from_slice(&parameter_part(params, scale));
    bytes
}

pub(super) fn parameter_part(params: &FaceParams, scale: Vec3) -> [u8; PARAMETER_BYTES] {
    let mut bytes = [0; PARAMETER_BYTES];
    let values = params
        .color
        .into_iter()
        .chain(params.uv.into_iter().flatten())
        .chain(params.params)
        .chain(scale.extend(0.0).to_array())
        .chain(params.emissive.into_iter().chain([0.0]))
        .chain(params.emissive_uv.into_iter().flatten());
    for (dest, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        *dest = value.to_le_bytes();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use math::Mat3;
    use wgpu::naga::{TypeInner, front::wgsl, proc::Layouter};

    use super::*;

    #[test]
    fn instance_attributes_match_shader() {
        let base = [0xa5; TRANSFORM_BYTES];
        let mut params = FaceParams::new(None);
        params.color = [0.125, 0.25, 0.5, 0.75];
        params.uv = [[2.0, 3.0, 4.0, 0.0], [5.0, 6.0, 7.0, 0.0]];
        params.params = [1.0, 2.0, 0.7, 1.0];
        params.emissive = [f32::MAX, 0.5, 1e30];
        params.emissive_uv = [[11.0, 12.0, 13.0, 0.0], [14.0, 15.0, 16.0, 0.0]];
        let bytes = packed_instance(&base, &params, Vec3::new(8.0, 9.0, 10.0));
        assert_eq!(&bytes[..112], &base, "model and normal matrices lead the face record");
        let attributes: Vec<_> = bytes[112..].as_chunks::<4>().0.iter().map(|word| f32::from_le_bytes(*word)).collect();
        assert_eq!(
            attributes[..20],
            [0.125, 0.25, 0.5, 0.75, 2.0, 3.0, 4.0, 0.0, 5.0, 6.0, 7.0, 0.0, 1.0, 2.0, 0.7, 1.0, 8.0, 9.0, 10.0, 0.0],
            "material vectors follow the transform"
        );
        assert_eq!(
            attributes[20..],
            [f32::MAX, 0.5, 1e30, 0.0, 11.0, 12.0, 13.0, 0.0, 14.0, 15.0, 16.0, 0.0],
            "finite emissive passes through unclamped"
        );

        let source = concat!(
            include_str!("../shaders/atmosphere.wgsl"),
            include_str!("../shaders/water.wgsl"),
            include_str!("../shaders/surface.wgsl")
        );
        let module = wgsl::parse_str(source).expect("surface shader parses");
        let mut layouter = Layouter::default();
        layouter.update(module.to_ctx()).expect("surface shader types lay out");
        let (face, ty) = module.types.iter().find(|(_, ty)| ty.name.as_deref() == Some("Face")).expect("surface shader declares Face");
        let TypeInner::Struct { members, .. } = &ty.inner else { panic!("Face is a struct") };
        let offsets: Vec<_> = members.iter().map(|member| (member.name.as_deref().unwrap_or_default(), member.offset)).collect();
        assert_eq!(offsets[..7], [("model", 0), ("normal", 64), ("color", 112), ("u", 128), ("v", 144), ("params", 160), ("scale", 176)]);
        assert_eq!(offsets[7..], [("emissive", 192), ("emissive_u", 208), ("emissive_v", 224)]);
        assert_eq!(layouter[face].to_stride() as usize, INSTANCE_BYTES, "storage array stride matches the packed record");
        assert_eq!(bytes.len(), INSTANCE_BYTES);
    }
    #[test]
    fn nonuniform_normals_upload() {
        let rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let scale = Vec3::new(2.0, 3.0, 4.0);
        let bytes = instance_base(DVec3::ZERO, rotation, scale, false, DVec3::ZERO);
        let column = |index: usize| {
            let start = 64 + index * 16;
            Vec3::new(
                f32::from_le_bytes(bytes[start..start + 4].try_into().expect("normal x")),
                f32::from_le_bytes(bytes[start + 4..start + 8].try_into().expect("normal y")),
                f32::from_le_bytes(bytes[start + 8..start + 12].try_into().expect("normal z")),
            )
        };
        let normal = Mat3::from_cols(column(0), column(1), column(2));
        assert!(normal.x_axis.distance(Vec3::Y) < 1e-6);
        assert!(normal.y_axis.distance(Vec3::new(-2.0 / 3.0, 0.0, 0.0)) < 1e-6);
        assert!(normal.z_axis.distance(Vec3::Z * 0.5) < 1e-6);
        let local_normal = Vec3::new(1.0, 1.0, 0.0).normalize();
        let local_tangent = Vec3::new(1.0, -1.0, 0.0);
        let world_normal = normal * local_normal;
        let world_tangent = rotation * (scale * local_tangent);
        assert!(world_normal.dot(world_tangent).abs() < 1e-5);
    }

    #[test]
    fn singular_scale_uploads_finite_normals() {
        let bytes = instance_base(DVec3::ZERO, Quat::IDENTITY, Vec3::ZERO, false, DVec3::ZERO);
        let normals: Vec<_> = bytes[64..112].as_chunks::<4>().0.iter().map(|component| f32::from_le_bytes(*component)).collect();

        assert!(normals.iter().all(|component| component.is_finite()));
    }
}
