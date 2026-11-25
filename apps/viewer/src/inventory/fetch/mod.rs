mod ais;
mod decode;
mod legacy;
mod scheduler;
mod state;

use std::{error, fmt, time::Duration};

use inventory::{CategoryKey, InventoryChild, InventoryError, InventoryRevision, ServerVersion};
use network::capabilities;
use protocol::inventory::DecodeError;
use tokio::sync::oneshot;

pub use self::scheduler::{InventoryFetchHandle, InventoryFetchOutcome, InventoryFetchRequestError, InventoryLoadingStatus};
pub(super) use self::scheduler::{fetch_single_folder, fetch_single_items};
pub(crate) use self::{
    decode::{to_category, to_item},
    scheduler::InventoryFetchController,
};

const MAX_FETCH_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_AIS_DEPTH: u8 = 16;
const MAX_AIS_BATCH: usize = 20;
const MAX_LEGACY_BATCH: usize = 10;
const MAX_FETCH_RETRIES: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchFailureCause {
    HttpStatus(u16),
    Timeout,
    Transport,
}

impl fmt::Display for FetchFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(status) => write!(formatter, "HTTP {status}"),
            Self::Timeout => formatter.write_str("request timed out"),
            Self::Transport => formatter.write_str("transport failure"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestFailure {
    Cancelled,
    Forbidden,
    Missing(u16),
    HttpStatus(u16),
    FolderTooLarge,
    Transient { cause: FetchFailureCause, retry_after: Option<Duration> },
    InvalidResponse(DecodeError),
    Failed,
}

fn classify_request_error(error: network::Error) -> RequestFailure {
    match error {
        network::Error::CapabilitySessionClosed | network::Error::CapabilityRequestCanceled => RequestFailure::Cancelled,
        network::Error::CapabilityHttp(error) if error.kind == capabilities::ErrorKind::Cancelled => RequestFailure::Cancelled,
        network::Error::CapabilityHttp(error) if error.status.is_some_and(|status| status.as_u16() == 403) => RequestFailure::Forbidden,
        network::Error::CapabilityHttp(error) if error.status.is_some_and(|status| matches!(status.as_u16(), 404 | 410)) => {
            RequestFailure::Missing(error.status.map_or(404, |status| status.as_u16()))
        }
        network::Error::CapabilityHttp(error) if error.kind == capabilities::ErrorKind::Timeout => {
            RequestFailure::Transient { cause: FetchFailureCause::Timeout, retry_after: error.retry_after }
        }
        network::Error::CapabilityHttp(error) if error.kind == capabilities::ErrorKind::Transport => {
            RequestFailure::Transient { cause: FetchFailureCause::Transport, retry_after: error.retry_after }
        }
        network::Error::CapabilityHttp(error) if error.status.is_some_and(|status| status.is_server_error() || status.as_u16() == 429) => {
            RequestFailure::Transient {
                cause: FetchFailureCause::HttpStatus(error.status.map_or(503, |status| status.as_u16())),
                retry_after: error.retry_after,
            }
        }
        network::Error::CapabilityHttp(error) if error.kind == capabilities::ErrorKind::ResourceExhausted => RequestFailure::FolderTooLarge,
        network::Error::CapabilityHttp(error) if error.status.is_some() => {
            RequestFailure::HttpStatus(error.status.map_or(400, |status| status.as_u16()))
        }
        network::Error::CapabilityHttp(error) if error.kind == capabilities::ErrorKind::InvalidResponse => {
            RequestFailure::InvalidResponse(DecodeError::MalformedLlsd)
        }
        network::Error::Capability(_) => RequestFailure::InvalidResponse(DecodeError::UnexpectedStructure("capability response")),
        _ => RequestFailure::Failed,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryFetchError {
    CapabilityUnavailable,
    FolderTooLarge,
    InvalidResponse(DecodeError),
    RetryExhausted(FetchFailureCause),
    HttpStatus(u16),
    Cancelled,
    RequestFailed,
    Model(InventoryError),
}

impl fmt::Display for InventoryFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityUnavailable => formatter.write_str("inventory fetch capability is unavailable"),
            Self::FolderTooLarge => formatter.write_str("inventory folder exceeds the server fetch limit"),
            Self::InvalidResponse(error) => write!(formatter, "inventory server returned an invalid folder response: {error}"),
            Self::RetryExhausted(cause) => write!(formatter, "inventory folder fetch retries were exhausted: {cause}"),
            Self::HttpStatus(status) => write!(formatter, "inventory folder request failed with HTTP {status}"),
            Self::Cancelled => formatter.write_str("inventory folder fetch was cancelled"),
            Self::RequestFailed => formatter.write_str("inventory folder request failed"),
            Self::Model(error) => write!(formatter, "{error}"),
        }
    }
}

impl error::Error for InventoryFetchError {}

impl From<InventoryError> for InventoryFetchError {
    fn from(error: InventoryError) -> Self {
        Self::Model(error)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FolderFetch {
    pub(crate) folder: CategoryKey,
    pub(crate) version: ServerVersion,
    pub(crate) descendent_count: Option<u32>,
    pub(crate) children: Vec<InventoryChild>,
    pub(crate) provenance: FolderFetchProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FolderFetchProvenance {
    Untracked,
    StartedAt { folder: Option<InventoryRevision>, owner: u64 },
}

#[derive(Debug)]
pub(crate) enum FolderApplyOutcome {
    Applied,
    Stale,
    Rejected,
}

#[derive(Debug)]
pub(crate) enum InventoryFetchEvent {
    Folder { fetch: FolderFetch, applied: oneshot::Sender<FolderApplyOutcome> },
    Failed { folder: CategoryKey, error: InventoryFetchError },
    BackgroundDone,
}
