use std::collections::HashSet;

use assets::{Priority, TextureHandle, TextureId};
use wgpu::Device;
use world::{Entity, Region};

use super::GeometryRenderer;
use crate::{
    material::{FaceMaterial, FaceParams, TextureBinding},
    scene::{Object, SceneView},
    texture::{MaterialTextures, Textures},
};

#[derive(Clone, Default)]
pub(super) struct MaterialImages {
    pub(super) base: Option<TextureHandle>,
    pub(super) emissive: Option<TextureHandle>,
}

impl MaterialImages {
    pub(super) fn new(material: Option<&FaceMaterial>, textures: MaterialTextures) -> Self {
        let Some(material) = material else { return Self::default() };
        let image = |binding: Option<&TextureBinding>| binding.and_then(TextureBinding::image).cloned();
        Self {
            base: textures.base.and(image(material.base_color_texture.as_ref())),
            emissive: textures.emissive.key().and(image(material.emissive_texture.as_ref())),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.base.is_none() && self.emissive.is_none()
    }

    fn iter(&self) -> impl Iterator<Item = &TextureHandle> {
        self.base.iter().chain(&self.emissive)
    }
}

#[derive(Clone)]
pub(super) struct TextureDemand {
    pub(super) key: MaterialTextures,
    pub(super) images: MaterialImages,
}

#[derive(Clone)]
pub(super) struct TerrainDemand {
    pub(super) region: Region,
    pub(super) textures: [Option<TextureHandle>; 4],
}

impl GeometryRenderer {
    pub(super) fn request_frame_textures(&mut self, view: &SceneView, background: &[Entity], gated: &[Entity]) {
        self.requested_textures.clear();
        self.visible_textures.clear();
        self.visible_texture_requests.clear();
        let epoch = self.epoch;
        {
            let (draw_list, requested, visible, visible_requests, textures) = (
                &self.draw_list,
                &mut self.requested_textures,
                &mut self.visible_textures,
                &mut self.visible_texture_requests,
                &mut self.textures,
            );
            for handle in draw_list.texture_demands().iter().flat_map(|demand| demand.images.iter()) {
                if visible.insert(handle.id()) {
                    visible_requests.push(handle.clone());
                }
                if requested.insert(handle.id()) {
                    textures.request(handle, epoch, Priority::Visible);
                }
            }
            for demand in draw_list.terrain_demands() {
                for handle in demand.textures.iter().flatten() {
                    if visible.insert(handle.id()) {
                        visible_requests.push(handle.clone());
                    }
                    if requested.insert(handle.id()) {
                        textures.request(handle, epoch, Priority::Visible);
                    }
                }
            }
        }
        for entity in gated {
            if let Some(object) = view.object(*entity).filter(|_| view.object_is_live(*entity))
                && let Some(faces) = object.materials()
            {
                for face in faces {
                    self.request_material_texture(face, Priority::Visible);
                }
            }
        }
        for entity in background {
            if let Some(object) = view.object(*entity).filter(|_| view.object_is_live(*entity)) {
                self.request_background_textures(object);
            }
        }
    }

    pub(super) fn prepare_texture_bindings(
        &mut self,
        device: &Device,
        layout: bool,
        changed: &HashSet<TextureId>,
        changed_regions: &[Region],
    ) -> usize {
        let epoch = self.epoch;
        let (draw_list, textures, pending) = (&self.draw_list, &mut self.textures, &mut self.pending_textures);
        if layout {
            let entries = draw_list.texture_demands().len() + draw_list.terrain_demands().len();
            pending.clear();
            textures.retain_terrain_bindings(draw_list.terrain_regions());
            textures.retain_material_bindings(draw_list.texture_keys());
            for demand in draw_list.texture_demands() {
                prepare_material(device, textures, pending, demand.key);
            }
            for demand in draw_list.terrain_demands() {
                textures.prepare_terrain_binding(device, demand.region, &demand.textures, epoch);
                for handle in demand.textures.iter().flatten() {
                    update_pending(pending, handle.id(), textures.is_resident(handle.id()));
                }
            }
            return entries;
        }
        if changed.is_empty() && changed_regions.is_empty() {
            return 0;
        }
        let entries = draw_list.texture_demands().len() + draw_list.terrain_demands().len();
        for demand in draw_list.texture_demands().iter().filter(|demand| demand.key.keys().any(|key| changed.contains(&key.id))) {
            prepare_material(device, textures, pending, demand.key);
        }
        for demand in draw_list.terrain_demands().iter().filter(|demand| {
            changed_regions.contains(&demand.region) || demand.textures.iter().flatten().any(|handle| changed.contains(&handle.id()))
        }) {
            textures.prepare_terrain_binding(device, demand.region, &demand.textures, epoch);
            for handle in demand.textures.iter().flatten() {
                update_pending(pending, handle.id(), textures.is_resident(handle.id()));
            }
        }
        entries
    }

    fn request_background_textures(&mut self, object: &Object) {
        let Some(faces) = object.materials() else { return };
        for face in faces {
            self.request_material_texture(face, Priority::Background);
        }
    }

    fn request_material_texture(&mut self, face: &FaceMaterial, priority: Priority) {
        let images = MaterialImages::new(Some(face), FaceParams::new(Some(face)).textures);
        for texture in images.iter() {
            if priority == Priority::Background && !texture.info().is_some_and(|info| info.size.into_iter().all(|edge| edge <= 32)) {
                self.requested_textures.insert(texture.id());
                continue;
            }
            if priority == Priority::Visible && self.visible_textures.insert(texture.id()) {
                self.visible_texture_requests.push(texture.clone());
            }
            if self.requested_textures.insert(texture.id()) {
                self.textures.request(texture, self.epoch, priority);
            }
        }
    }
}

fn prepare_material(device: &Device, textures: &mut Textures, pending: &mut HashSet<TextureId>, key: MaterialTextures) {
    textures.prepare_material_binding(device, key);
    for key in key.keys() {
        update_pending(pending, key.id, textures.is_resident(key.id));
    }
}

fn update_pending(pending: &mut HashSet<TextureId>, id: TextureId, resident: bool) {
    if resident {
        pending.remove(&id);
    } else {
        pending.insert(id);
    }
}
