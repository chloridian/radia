mod agent_motion;
mod coordinator;
mod environment;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod experience_environment;
mod inbound;
mod object_cache;
mod object_media;
mod outbound;
mod recovery;
mod region_lookup;
mod terrain;
mod world;

pub(crate) use agent_motion::AgentMotionPublisher;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use agent_motion::AgentMotionSample;
pub(crate) use coordinator::{
    AuthenticationUpdate, CredentialPersistence, FavoritesUpdate, PresentationMailbox, PresentationSnapshot, SessionEnd,
    SessionReadyUpdate, ViewerUpdate, WriteLock, drain_session, save_favorite_store, write_lock,
};
#[cfg(test)]
pub(crate) use coordinator::{FolderFetchApply, apply_inventory_fetch, apply_inventory_udp, confirm_pending_moves};
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use experience_environment::EnvironmentInbox;
pub(crate) use inbound::{ChatMessage, display_message, format_avatar_name};
pub(crate) use network::{Session, SessionHandle};
pub(crate) use outbound::CameraUplink;
pub(crate) use region_lookup::{RegionGridCell, RegionLookup, RegionLookupError, map_request_coordinates};

#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
pub(crate) use experience_environment::EnvironmentCommand;
