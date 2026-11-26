#![expect(clippy::indexing_slicing)]

mod benchmark;
pub mod environment;
mod geometry;
mod gpu_timing;
mod limits;
pub mod material;
mod pipeline;
mod renderer;
pub mod scene;
#[cfg(test)]
mod test_support;
mod texture;
mod workers;

#[doc(hidden)]
pub use self::benchmark::GeometryPrepare;
pub use self::{
    geometry::{FaceExplanation, MeshResidency, ObjectExplanation, OmissionReason, PickDetail, PickError, Stats, VisibilityStatus},
    limits::{Limits, RESERVED_THREADS, auto_worker_threads},
    renderer::{CaptureReadback, Error, FrameInput, Progress, Renderer, Retirement, required_features, required_limits},
    texture::{CompressionQuality, TextureCompletion, TextureJob},
    workers::{WorkerBudget, WorkerKind, WorkerStats},
};
