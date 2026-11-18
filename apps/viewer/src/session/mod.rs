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

pub(crate) use network::{Session, SessionHandle};

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use self::agent_motion::AgentMotionSample;
#[cfg(test)]
pub(crate) use self::coordinator::{FolderFetchApply, apply_inventory_fetch, apply_udp, confirm_pending_moves};
#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
pub(crate) use self::experience_environment::EnvironmentCommand;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use self::experience_environment::EnvironmentInbox;
pub(crate) use self::{
    agent_motion::AgentMotionPublisher,
    coordinator::{
        AuthenticationUpdate, CredentialPersistence, FavoritesUpdate, PresentationMailbox, PresentationSnapshot, SessionEnd,
        SessionReadyUpdate, ViewerUpdate, WriteLock, drain_session, save_favorite_store, write_lock,
    },
    inbound::{ChatMessage, display_message, format_avatar_name},
    outbound::CameraUplink,
    region_lookup::{RegionGridCell, RegionLookup, RegionLookupError, map_request_coordinates},
};
