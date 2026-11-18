mod controller;
mod delta;
mod handle;
mod policy;
mod types;

pub(crate) use self::{
    controller::{InventoryEditController, InventoryEditEvent},
    delta::AisDelta,
    types::MoveExpectation,
};
pub use self::{
    handle::InventoryEditHandle,
    types::{InventoryEditOutcome, InventoryEditRejection},
};

#[cfg(test)]
mod controller_tests;
