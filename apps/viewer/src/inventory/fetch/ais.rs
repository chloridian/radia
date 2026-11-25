use inventory::{CategoryKey, InventoryOwner, Item};
use network::capabilities::{CapabilityUrl, Client};
use protocol::inventory::{DecodeError, decode_ais_children, decode_ais_item, decode_ais_library_children};
use utils::FolderId;

use super::{
    FolderFetch, InventoryFetchError, MAX_AIS_DEPTH, MAX_FETCH_RESPONSE_BYTES, RequestFailure, classify_request_error,
    decode::{ais_batch_folders, ais_folders, to_item},
};

pub(super) async fn fetch_once(
    client: &Client,
    capability: &CapabilityUrl,
    folder: CategoryKey,
    depth: u8,
    is_library: bool,
) -> Result<Vec<FolderFetch>, RequestFailure> {
    let folder_id = folder.id().uuid().to_string();
    let url = capability
        .with_path_segments(&["category", &folder_id, "children"])
        .map_err(|_| RequestFailure::Failed)?
        .with_query_pair("depth", &depth.min(MAX_AIS_DEPTH).to_string());
    let body = client
        .get_inventory_bytes_with_accept(&url, "application/llsd+xml", MAX_FETCH_RESPONSE_BYTES)
        .await
        .map_err(classify_request_error)?;
    let category = if is_library {
        decode_ais_library_children(&body, folder.id().uuid())
    } else {
        decode_ais_children(&body, folder.id().uuid(), folder.owner().id())
    }
    .map_err(classify_decode_error)?;
    ais_folders(&category, folder, is_library).map_err(|error| match error {
        InventoryFetchError::FolderTooLarge => RequestFailure::FolderTooLarge,
        InventoryFetchError::InvalidResponse(decode) => RequestFailure::InvalidResponse(decode),
        _ => RequestFailure::InvalidResponse(DecodeError::UnexpectedStructure("ais folder decode")),
    })
}

pub(super) async fn fetch_batch_once(
    client: &Client,
    capability: &CapabilityUrl,
    parent: CategoryKey,
    folders: &[CategoryKey],
    depth: u8,
    is_library: bool,
) -> Result<Vec<FolderFetch>, RequestFailure> {
    let parent_id = parent.id().uuid().to_string();
    let child_ids = folders.iter().map(|folder| folder.id().uuid().to_string()).collect::<Vec<_>>();
    let url = capability
        .with_path_segments(&["category", &parent_id, "children"])
        .map_err(|_| RequestFailure::Failed)?
        .with_query_pair("depth", &depth.min(MAX_AIS_DEPTH).to_string())
        .with_csv_query_pair("children", &child_ids);
    let body = client
        .get_inventory_bytes_with_accept(&url, "application/llsd+xml", MAX_FETCH_RESPONSE_BYTES)
        .await
        .map_err(classify_request_error)?;
    let category = if is_library {
        decode_ais_library_children(&body, parent.id().uuid())
    } else {
        decode_ais_children(&body, parent.id().uuid(), parent.owner().id())
    }
    .map_err(classify_decode_error)?;
    ais_batch_folders(&category, parent, folders, is_library).map_err(|error| match error {
        InventoryFetchError::FolderTooLarge => RequestFailure::FolderTooLarge,
        InventoryFetchError::InvalidResponse(decode) => RequestFailure::InvalidResponse(decode),
        _ => RequestFailure::InvalidResponse(DecodeError::UnexpectedStructure("ais batch decode")),
    })
}

pub(super) async fn fetch_item_once(
    client: &Client,
    capability: &CapabilityUrl,
    owner: InventoryOwner,
    item_id: uuid::Uuid,
) -> Result<Item, RequestFailure> {
    let item_id_text = item_id.to_string();
    let url = capability.with_path_segments(&["item", &item_id_text]).map_err(|_| RequestFailure::Failed)?;
    let body = client
        .get_inventory_bytes_with_accept(&url, "application/llsd+xml", MAX_FETCH_RESPONSE_BYTES)
        .await
        .map_err(classify_request_error)?;
    let item = decode_ais_item(&body, item_id, owner.id()).map_err(classify_decode_error)?;
    let parent = item.parent_id.ok_or(RequestFailure::InvalidResponse(DecodeError::ParentMismatch))?;
    to_item(&item, owner, CategoryKey::new(owner, FolderId::new(parent))).map_err(|error| match error {
        InventoryFetchError::InvalidResponse(decode) => RequestFailure::InvalidResponse(decode),
        _ => RequestFailure::InvalidResponse(DecodeError::UnexpectedStructure("ais item decode")),
    })
}

fn classify_decode_error(error: DecodeError) -> RequestFailure {
    match error {
        DecodeError::TooLarge => RequestFailure::FolderTooLarge,
        other => RequestFailure::InvalidResponse(other),
    }
}
