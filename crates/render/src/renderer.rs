use std::{
    error, fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use math::DVec3;
use wgpu::{
    AdapterInfo, Buffer, BufferDescriptor, BufferUsages, COPY_BYTES_PER_ROW_ALIGNMENT, CommandEncoderDescriptor, Device, Extent3d, LoadOp,
    Operations, Origin3d, PollError, PollType, Queue, RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor,
    RenderPassTimestampWrites, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};
use world::Entity;

use crate::{
    Limits,
    environment::Evaluated,
    geometry::{GeometryRenderer, ObjectExplanation, PickDetail, PickError, Stats},
    gpu_timing::GpuTimings,
    pipeline::{PostProcess, SCENE_CLEAR, camera_view, projection},
    scene::{AgentPose, Camera, SceneView},
    texture::{TextureCompletion, TextureJob},
};

pub fn required_features() -> wgpu::Features {
    wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::INDIRECT_FIRST_INSTANCE | wgpu::Features::TIMESTAMP_QUERY
}

pub fn required_limits() -> wgpu::Limits {
    wgpu::Limits::default()
}

#[derive(Debug)]
pub enum Error {
    Poll(PollError),
    UnsupportedColorFormat,
    UnsupportedSurfaceSize,
    Gpu(wgpu::Error),
    DeviceLost,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poll(error) => write!(f, "Could not poll the scene GPU: {error}"),
            Self::UnsupportedColorFormat => f.write_str("The GPU has no supported sRGB surface format."),
            Self::UnsupportedSurfaceSize => f.write_str("The scene window exceeds the GPU's supported surface size."),
            Self::Gpu(error) => write!(f, "GPU operation failed: {error}"),
            Self::DeviceLost => f.write_str("The scene GPU device is unavailable."),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Poll(error) => Some(error),
            Self::Gpu(error) => Some(error),
            Self::UnsupportedColorFormat | Self::UnsupportedSurfaceSize | Self::DeviceLost => None,
        }
    }
}

pub struct Renderer {
    device: Arc<Device>,
    queue: Arc<Queue>,
    unorm_format: TextureFormat,
    srgb_format: TextureFormat,
    size: [u32; 2],
    info: AdapterInfo,
    output_texture: Option<Arc<Texture>>,
    output_view: Option<TextureView>,
    geometry: GeometryRenderer,
    gpu_timings: Option<GpuTimings>,
    postprocess: PostProcess,
    last_frame: Instant,
}

impl Renderer {
    pub fn new(
        device: Arc<Device>,
        queue: Arc<Queue>,
        info: AdapterInfo,
        format: TextureFormat,
        width: u32,
        height: u32,
        limits: Limits,
    ) -> Result<Self, Error> {
        let srgb_format = format.add_srgb_suffix();
        if !srgb_format.is_srgb() {
            return Err(Error::UnsupportedColorFormat);
        }
        let unorm_format = format.remove_srgb_suffix();
        let timestamp_queries = device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        tracing::info!(
            format = ?unorm_format,
            srgb_format = ?srgb_format,
            timestamp_queries,
            "scene renderer configured"
        );
        let geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba16Float, limits);
        let gpu_timings = if timestamp_queries { GpuTimings::new(&device, queue.get_timestamp_period()) } else { None };
        let postprocess = PostProcess::new(&device, &queue, srgb_format);
        let mut renderer = Self {
            device,
            queue,
            unorm_format,
            srgb_format,
            size: [0, 0],
            info,
            output_texture: None,
            output_view: None,
            geometry,
            gpu_timings,
            postprocess,
            last_frame: Instant::now(),
        };
        renderer.resize(width, height);
        Ok(renderer)
    }

    pub fn retire(self) -> Retirement {
        let device = Arc::clone(&self.device);
        drop(self);
        Retirement { device }
    }

    pub fn poll(&mut self) -> Result<Progress, Error> {
        self.geometry.release_textures();
        let status = self.device.poll(PollType::Poll).map_err(Error::Poll)?;
        if let Some(gpu_timings) = &mut self.gpu_timings {
            gpu_timings.poll();
        }
        let now = Instant::now();
        let redraw_at = self.geometry.redraw_deadline().filter(|deadline| *deadline > now);
        Ok(Progress {
            redraw: self.geometry.needs_frame(now) || self.postprocess.needs_frame(),
            pending: !status.is_queue_empty(),
            redraw_at,
        })
    }

    pub fn adapter_info(&self) -> &AdapterInfo {
        &self.info
    }

    pub fn output_texture(&self) -> Option<(Arc<Texture>, [u32; 2])> {
        self.output_texture.as_ref().map(|texture| (Arc::clone(texture), self.size))
    }

    pub fn stats(&self) -> Stats {
        self.geometry.stats
    }

    pub fn gpu_timings(&self) -> Option<(f64, f64)> {
        self.gpu_timings.as_ref()?.latest()
    }

    pub fn gpu_timing_supported(&self) -> bool {
        self.gpu_timings.is_some()
    }

    pub fn pick(&self, camera: Camera, ray: DVec3) -> Result<Option<DVec3>, PickError> {
        self.geometry.pick(camera, ray)
    }

    pub fn pick_detailed(&self, camera: Camera, ray: DVec3) -> Result<Option<PickDetail>, PickError> {
        self.geometry.pick_detailed(camera, ray)
    }

    pub fn explain_object(&self, view: &SceneView, entity: Entity) -> ObjectExplanation {
        self.geometry.explain_object(view, entity)
    }

    pub fn begin_capture_rgba(&self) -> Option<CaptureReadback> {
        let texture = self.output_texture.as_ref()?;
        let [width, height] = self.size;
        if width == 0 || height == 0 {
            return None;
        }
        let unpadded = width.checked_mul(4)?;
        let align = COPY_BYTES_PER_ROW_ALIGNMENT;
        let bytes_per_row = unpadded.checked_add(align - 1)? / align * align;
        let buffer_size = u64::from(bytes_per_row).checked_mul(u64::from(height))?;
        let buffer = self.device.create_buffer(&BufferDescriptor {
            label: Some("Scene capture readback"),
            size: buffer_size,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&CommandEncoderDescriptor { label: Some("Scene capture") });
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo { texture, mip_level: 0, origin: Origin3d::ZERO, aspect: TextureAspect::All },
            TexelCopyBufferInfo {
                buffer: &buffer,
                layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(height) },
            },
            Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        Some(CaptureReadback { device: Arc::clone(&self.device), buffer, width, height, bytes_per_row, format: self.unorm_format })
    }

    pub fn next_texture_job(&mut self) -> Option<TextureJob> {
        self.geometry.next_texture_job()
    }

    pub fn complete_texture(&mut self, completion: TextureCompletion) {
        self.geometry.complete_texture(completion);
    }

    pub fn texture_soft_cap_mib(&self) -> u32 {
        self.geometry.texture_soft_cap_mib()
    }

    pub fn vram_budget_bytes(&self) -> usize {
        self.geometry.vram_budget_bytes()
    }

    pub fn set_texture_soft_cap(&mut self, mib: u32) {
        self.geometry.set_texture_soft_cap(mib);
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.size == [width, height] && (width == 0 || height == 0 || self.output_texture.is_some()) {
            return;
        }
        self.size = [width, height];
        if width == 0 || height == 0 {
            self.output_texture = None;
            self.output_view = None;
            return;
        }
        let max_dim = self.device.limits().max_texture_dimension_2d;
        if width > max_dim || height > max_dim {
            self.output_texture = None;
            self.output_view = None;
            return;
        }
        let view_formats = [self.srgb_format];
        let texture = Arc::new(self.device.create_texture(&TextureDescriptor {
            label: Some("Scene output"),
            size: Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: self.unorm_format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC,
            view_formats: &view_formats,
        }));
        let view = texture.create_view(&TextureViewDescriptor { format: Some(self.srgb_format), ..Default::default() });
        self.output_texture = Some(texture);
        self.output_view = Some(view);
        self.postprocess.resize(&self.device, self.size);
        let depth = self.geometry.depth(&self.device, self.size);
        self.postprocess.set_depth_view(&self.device, depth.as_ref());
    }

    pub fn draw(
        &mut self,
        scene: Option<(&SceneView, Camera)>,
        agent_pose: Option<AgentPose>,
        frame_input: FrameInput<'_>,
    ) -> Result<bool, Error> {
        self.poll()?;
        if self.size.contains(&0) {
            return Ok(false);
        }
        if self.size.into_iter().any(|n| n > self.device.limits().max_texture_dimension_2d) {
            return Err(Error::UnsupportedSurfaceSize);
        }
        let Some(view) = self.output_view.as_ref() else {
            return Ok(false);
        };
        let Some(admission) = self.geometry.admit() else {
            return Ok(false);
        };
        let mut encoder = self.device.create_command_encoder(&CommandEncoderDescriptor { label: Some("Scene frame") });
        let timestamp_queries = self.gpu_timings.as_ref().filter(|timings| timings.recording()).map(GpuTimings::query_set);
        let now = Instant::now();
        let delta_seconds = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        let depth = if let Some((scene, camera)) = scene {
            let depth = self.geometry.depth(&self.device, self.size);
            if depth.is_some() {
                self.geometry.prepare_with_environment(&self.device, &mut encoder, scene, agent_pose, camera, self.size, frame_input);
            }
            depth
        } else {
            None
        };
        if depth.is_none() {
            self.geometry.skip();
        }
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Scene geometry"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: self.postprocess.scene_view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(SCENE_CLEAR), store: StoreOp::Store },
                })],
                depth_stencil_attachment: depth.as_ref().map(|view| RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(Operations { load: LoadOp::Clear(0.0), store: StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: timestamp_queries.map(|query_set| RenderPassTimestampWrites {
                    query_set,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                ..Default::default()
            });
            if depth.is_some() {
                self.geometry.draw(&mut pass);
            }
        }
        let inverse_view_projection = scene.map_or(math::Mat4::IDENTITY, |(_, camera)| projection(camera, self.size).inverse());
        let view_matrix = scene.map_or(math::Mat4::IDENTITY, |(_, camera)| camera_view(camera));
        let camera_position = scene.map(|(_, camera)| camera.position());
        let scene_version = scene.map_or(0, |(view, _)| view.sequence());
        self.postprocess.render(
            &self.queue,
            &mut encoder,
            view,
            frame_input.environment,
            delta_seconds,
            camera_position,
            frame_input.water_height,
            inverse_view_projection,
            view_matrix,
            scene_version,
            self.geometry.texture_revision(),
        );
        if timestamp_queries.is_some()
            && let Some(gpu_timings) = &self.gpu_timings
        {
            gpu_timings.resolve(&mut encoder);
        }
        self.queue.submit([encoder.finish()]);
        if timestamp_queries.is_some()
            && let Some(gpu_timings) = &mut self.gpu_timings
        {
            gpu_timings.submitted();
        }
        self.geometry.submitted(admission);
        Ok(true)
    }
}

#[derive(Clone, Copy, Default)]
pub struct FrameInput<'a> {
    pub water_height: Option<f64>,
    pub environment: Option<&'a Evaluated>,
    pub cloud_scroll: [f32; 2],
    pub animation_time: f32,
    pub water_scroll: [f32; 4],
}

pub struct Progress {
    pub redraw: bool,
    pub pending: bool,
    pub redraw_at: Option<Instant>,
}

pub struct CaptureReadback {
    pub device: Arc<Device>,
    pub buffer: Buffer,
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
    pub format: TextureFormat,
}

pub struct Retirement {
    device: Arc<Device>,
}

impl Retirement {
    pub fn poll(&self) -> Result<bool, Error> {
        self.device.poll(PollType::Poll).map(|status| status.is_queue_empty()).map_err(Error::Poll)
    }

    pub fn wait(self) -> Result<(), Error> {
        self.device
            .poll(PollType::Wait { submission_index: None, timeout: Some(Duration::from_millis(500)) })
            .map(|_| ())
            .map_err(Error::Poll)
    }
}

#[cfg(test)]
mod tests {
    use wgpu::naga::{
        AddressSpace, ImageClass, Module, TypeInner,
        front::wgsl,
        proc::Layouter,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shaders_validate() {
        for (name, source) in [
            ("atmosphere", include_str!("shaders/atmosphere.wgsl")),
            (
                "surface",
                concat!(include_str!("shaders/atmosphere.wgsl"), include_str!("shaders/water.wgsl"), include_str!("shaders/surface.wgsl")),
            ),
            (
                "terrain",
                concat!(
                    include_str!("shaders/atmosphere.wgsl"),
                    include_str!("shaders/water.wgsl"),
                    include_str!("shaders/surface.wgsl"),
                    "\n",
                    include_str!("shaders/terrain.wgsl")
                ),
            ),
            (
                "void water",
                concat!(
                    include_str!("shaders/atmosphere.wgsl"),
                    include_str!("shaders/water.wgsl"),
                    include_str!("shaders/void_water.wgsl")
                ),
            ),
            ("sky", concat!(include_str!("shaders/atmosphere.wgsl"), include_str!("shaders/water.wgsl"), include_str!("shaders/sky.wgsl"))),
            ("stars", include_str!("shaders/stars.wgsl")),
            ("mipmap", include_str!("shaders/mipmap.wgsl")),
            (
                "exposure",
                concat!(include_str!("shaders/atmosphere.wgsl"), include_str!("shaders/water.wgsl"), include_str!("shaders/exposure.wgsl")),
            ),
            (
                "tonemap",
                concat!(include_str!("shaders/atmosphere.wgsl"), include_str!("shaders/water.wgsl"), include_str!("shaders/tonemap.wgsl")),
            ),
        ] {
            let module = wgsl::parse_str(source).unwrap_or_else(|error| panic!("{name}: {}", error.emit_to_string(source)));
            Validator::new(ValidationFlags::all(), Capabilities::all()).validate(&module).unwrap_or_else(|error| panic!("{name}: {error}"));
            validate_default_limits(name, &module);
        }
    }

    fn validate_default_limits(name: &str, module: &Module) {
        let limits = wgpu::Limits::default();
        let mut bindings_per_group = vec![0_u32; limits.max_bind_groups as usize];
        let mut sampled_textures = 0_u32;
        let mut samplers = 0_u32;
        let mut layouter = Layouter::default();
        layouter.update(module.to_ctx()).unwrap_or_else(|error| panic!("{name}: {error}"));
        for (_, global) in module.global_variables.iter() {
            if let Some(binding) = global.binding.as_ref() {
                assert!(
                    binding.group < limits.max_bind_groups,
                    "{name}: bind group {} reaches the default limit {}",
                    binding.group,
                    limits.max_bind_groups
                );
                let Some(group_count) = bindings_per_group.get_mut(binding.group as usize) else {
                    panic!("{name}: group {} is not represented by default limits", binding.group);
                };
                *group_count += 1;
                assert!(
                    binding.binding < limits.max_bindings_per_bind_group,
                    "{name}: binding {} reaches the default per-group limit {}",
                    binding.binding,
                    limits.max_bindings_per_bind_group
                );
            }
            if global.space == AddressSpace::Uniform {
                let size = layouter[global.ty].size;
                assert!(
                    u64::from(size) <= limits.max_uniform_buffer_binding_size,
                    "{name}: uniform is {size} bytes; default limit is {}",
                    limits.max_uniform_buffer_binding_size
                );
            }
            if global.space == AddressSpace::Handle {
                match module.types[global.ty].inner {
                    TypeInner::Image { class: ImageClass::Sampled { .. } | ImageClass::Depth { .. }, .. } => sampled_textures += 1,
                    TypeInner::Sampler { .. } => samplers += 1,
                    _ => {}
                }
            }
        }
        assert!(bindings_per_group.into_iter().all(|count| count <= limits.max_bindings_per_bind_group));
        assert!(
            sampled_textures <= limits.max_sampled_textures_per_shader_stage,
            "{name}: {sampled_textures} sampled textures; default stage limit is {}",
            limits.max_sampled_textures_per_shader_stage
        );
        assert!(
            samplers <= limits.max_samplers_per_shader_stage,
            "{name}: {samplers} samplers; default stage limit is {}",
            limits.max_samplers_per_shader_stage
        );
    }
}
