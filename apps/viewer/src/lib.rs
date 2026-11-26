#![expect(clippy::indexing_slicing)]

mod agent_movement;
pub mod asset_coordinator;
pub(crate) mod camera;
pub mod gui;
pub mod inspection;
pub mod inventory;
pub mod limits;
pub mod logging;
pub mod mcp;
pub mod pressure;
mod remembered_login;
mod scene_feed;
mod session;
pub mod settings;
#[cfg(test)]
mod test_helpers;
mod worker_pool;
pub mod world_reader;
