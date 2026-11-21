mod environment;
mod timing;
mod window;

use std::{error::Error, fmt, time::Duration};

use render::scene;

pub(crate) use self::window::{SceneView, SceneViewConfig};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

#[derive(Debug)]
pub(crate) enum ViewportError {
    WindowOpen(String),
    GpuContextUnavailable,
    Renderer(render::Error),
    ScenePublication(scene::ApplyError),
}

impl fmt::Display for ViewportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowOpen(error) => write!(f, "Could not open the scene window: {error}"),
            Self::GpuContextUnavailable => f.write_str("The shared GPU context is unavailable."),
            Self::Renderer(error) => fmt::Display::fmt(error, f),
            Self::ScenePublication(error) => write!(f, "Scene publication failed: {error:?}"),
        }
    }
}

impl Error for ViewportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Renderer(error) => Some(error),
            Self::ScenePublication(error) => Some(error),
            Self::WindowOpen(_) | Self::GpuContextUnavailable => None,
        }
    }
}
