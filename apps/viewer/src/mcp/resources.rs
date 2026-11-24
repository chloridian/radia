use rmcp::{
    ErrorData, RoleServer,
    model::{ErrorCode, ReadResourceResult, Resource, ResourceContents, ResourceTemplate},
    service::RequestContext,
};

use super::{
    MAX_PNG_RESPONSE_BYTES,
    server::{RadiaServer, response_too_large_error},
    tools::{
        explain_object::explain_object_data, logs::logs_data, region::region_data, screenshot::capture_screenshot_base64,
        settings::settings_data, stats::stats_data, world_query::world_summary_data,
    },
};

pub const OBJECT_URI_TEMPLATE: &str = "viewer://object/{object_id}";
const OBJECT_URI_PREFIX: &str = "viewer://object/";

pub fn static_resources() -> Vec<Resource> {
    vec![
        make_resource(
            "viewer://world",
            "world",
            "World Summary",
            "Active regions and object counts from the World snapshot.",
            "application/json",
        ),
        make_resource(
            "viewer://stats",
            "stats",
            "Viewer Stats",
            "Frame timing, recent spikes, render stats, asset queue stats, and memory stats.",
            "application/json",
        ),
        make_resource(
            "viewer://region",
            "region",
            "Current Region",
            "Current agent region name, handle, SimStats, and parcel state.",
            "application/json",
        ),
        make_resource("viewer://settings", "settings", "Viewer Settings", "Current viewer cvar values and provenance.", "application/json"),
        make_resource(
            "viewer://logs",
            "logs",
            "Recent Logs",
            "Recent WARN and ERROR log entries from the in-memory ring buffer.",
            "application/json",
        ),
        make_resource(
            "viewer://screenshot",
            "screenshot",
            "Viewport Screenshot",
            "Current viewport texture encoded as a PNG image.",
            "image/png",
        ),
    ]
}

pub fn resource_templates() -> Vec<ResourceTemplate> {
    let mut template = ResourceTemplate::new(OBJECT_URI_TEMPLATE, "object");
    template.title = Some("Object Explanation".to_owned());
    template.description = Some("Combined World detail and renderer visibility, mesh, and material explanation for an object.".to_owned());
    template.mime_type = Some("application/json".to_owned());
    vec![template]
}

pub fn resource_for_tool(tool_name: &str) -> Option<Resource> {
    static_resources().into_iter().find(|resource| resource.name == tool_name)
}

pub async fn read_resource(server: &RadiaServer, uri: &str, context: RequestContext<RoleServer>) -> Result<ReadResourceResult, ErrorData> {
    let max_json = server.config.max_response_bytes;
    match uri {
        "viewer://world" => {
            let data = world_summary_data(&server.handle).await.map_err(|msg| ErrorData::internal_error(msg, None))?;
            encode_json_resource(uri, &data, max_json)
        }
        "viewer://stats" => {
            let data = stats_data(&server.handle, None);
            encode_json_resource(uri, &data, max_json)
        }
        "viewer://region" => {
            let data = region_data(&server.handle).await.map_err(|msg| ErrorData::internal_error(msg, None))?;
            encode_json_resource(uri, &data, max_json)
        }
        "viewer://settings" => {
            let data = settings_data(&server.handle, None, None);
            encode_json_resource(uri, &data, max_json)
        }
        "viewer://logs" => {
            let data = logs_data(&server.handle, None, None).map_err(|msg| ErrorData::internal_error(msg, None))?;
            encode_json_resource(uri, &data, max_json)
        }
        "viewer://screenshot" => {
            let (data_base64, _, _) =
                capture_screenshot_base64(&server.handle, context).await.map_err(|msg| ErrorData::internal_error(msg, None))?;
            if data_base64.len() > MAX_PNG_RESPONSE_BYTES {
                return Err(response_too_large_error());
            }
            Ok(ReadResourceResult::new(vec![ResourceContents::BlobResourceContents {
                uri: uri.to_owned(),
                mime_type: Some("image/png".to_owned()),
                blob: data_base64,
                meta: None,
            }]))
        }
        _ => {
            if let Some(object_id) = uri.strip_prefix(OBJECT_URI_PREFIX) {
                if object_id.is_empty() {
                    return Err(ErrorData::invalid_params("object_id is required", None));
                }
                let data = explain_object_data(&server.handle, object_id).await.map_err(|msg| ErrorData::invalid_params(msg, None))?;
                if data.active_session == Some(false) || data.found == Some(false) {
                    return Err(ErrorData::resource_not_found(format!("unknown object: {object_id}"), None));
                }
                encode_json_resource(uri, &data, max_json)
            } else {
                Err(ErrorData::resource_not_found(format!("unknown resource: {uri}"), None))
            }
        }
    }
}

fn encode_json_resource<T: serde::Serialize>(uri: &str, value: &T, max_bytes: usize) -> Result<ReadResourceResult, ErrorData> {
    let text =
        serde_json::to_string(value).map_err(|error| ErrorData::new(ErrorCode(-32603), format!("serialize failed: {error}"), None))?;
    if text.len() > max_bytes {
        return Err(response_too_large_error());
    }
    Ok(ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
        uri: uri.to_owned(),
        mime_type: Some("application/json".to_owned()),
        text,
        meta: None,
    }]))
}

fn make_resource(uri: &str, name: &str, title: &str, description: &str, mime_type: &str) -> Resource {
    let mut resource = Resource::new(uri, name);
    resource.title = Some(title.to_owned());
    resource.description = Some(description.to_owned());
    resource.mime_type = Some(mime_type.to_owned());
    resource
}
