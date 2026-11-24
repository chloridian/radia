use rmcp::{
    ErrorData,
    model::{ErrorCode, ReadResourceResult, Resource, ResourceContents, ResourceTemplate},
};
use serde_json::{Value, json};
use tokio::time;
use uuid::Uuid;

use super::{
    MAX_PNG_RESPONSE_BYTES,
    server::McpServerConfig,
    tools::{ToolCallContext, ToolContent, find_tool, screenshot},
};
use crate::inspection::ViewerInspectionHandle;

pub const OBJECT_URI_TEMPLATE: &str = "viewer://object/{local_id}";
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
            "text/plain",
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

pub async fn read_resource(
    uri: &str,
    handle: &ViewerInspectionHandle,
    config: McpServerConfig,
    ctx: ToolCallContext,
) -> Result<ReadResourceResult, ErrorData> {
    match uri {
        "viewer://world" => read_json_tool(uri, "world", json!({ "query": "summary" }), "application/json", handle, config).await,
        "viewer://stats" => read_json_tool(uri, "stats", json!({}), "application/json", handle, config).await,
        "viewer://region" => read_json_tool(uri, "region", json!({}), "application/json", handle, config).await,
        "viewer://settings" => read_json_tool(uri, "settings", json!({}), "application/json", handle, config).await,
        "viewer://logs" => read_json_tool(uri, "logs", json!({}), "text/plain", handle, config).await,
        "viewer://screenshot" => read_screenshot_resource(uri, handle, config, ctx).await,
        _ => {
            if let Some(id_segment) = uri.strip_prefix(OBJECT_URI_PREFIX) {
                read_object_resource(uri, id_segment, handle, config).await
            } else {
                Err(ErrorData::resource_not_found(format!("unknown resource: {uri}"), None))
            }
        }
    }
}

fn make_resource(uri: &str, name: &str, title: &str, description: &str, mime_type: &str) -> Resource {
    let mut resource = Resource::new(uri, name);
    resource.title = Some(title.to_owned());
    resource.description = Some(description.to_owned());
    resource.mime_type = Some(mime_type.to_owned());
    resource
}

async fn read_json_tool(
    uri: &str,
    tool_name: &str,
    args: Value,
    mime_type: &str,
    handle: &ViewerInspectionHandle,
    config: McpServerConfig,
) -> Result<ReadResourceResult, ErrorData> {
    let Some(tool) = find_tool(tool_name) else {
        return Err(ErrorData::internal_error("backing tool missing", None));
    };
    let outcome = time::timeout(config.request_timeout, (tool.handler)(handle.clone(), args))
        .await
        .map_err(|_| ErrorData::new(ErrorCode(-32001), "resource read timed out", None))?;
    let content = outcome.map_err(|message| ErrorData::internal_error(message, None))?;
    let ToolContent::Json(value) = content else {
        return Err(ErrorData::internal_error("unexpected non-JSON tool output", None));
    };
    let text = value.to_string();
    let limit = tool.max_response_bytes.min(config.max_response_bytes);
    if text.len() > limit {
        return Err(ErrorData::new(ErrorCode(-32002), "response exceeded size limit", None));
    }
    Ok(ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
        uri: uri.to_owned(),
        mime_type: Some(mime_type.to_owned()),
        text,
        meta: None,
    }]))
}

async fn read_screenshot_resource(
    uri: &str,
    handle: &ViewerInspectionHandle,
    config: McpServerConfig,
    ctx: ToolCallContext,
) -> Result<ReadResourceResult, ErrorData> {
    let outcome = time::timeout(config.request_timeout, screenshot::call_with_context(handle.clone(), json!({}), ctx))
        .await
        .map_err(|_| ErrorData::new(ErrorCode(-32001), "resource read timed out", None))?;
    let content = outcome.map_err(|message| ErrorData::internal_error(message, None))?;
    let ToolContent::ImagePng { data_base64, .. } = content else {
        return Err(ErrorData::internal_error("unexpected non-image screenshot output", None));
    };
    if data_base64.len() > MAX_PNG_RESPONSE_BYTES {
        return Err(ErrorData::new(ErrorCode(-32002), "response exceeded size limit", None));
    }
    Ok(ReadResourceResult::new(vec![ResourceContents::BlobResourceContents {
        uri: uri.to_owned(),
        mime_type: Some("image/png".to_owned()),
        blob: data_base64,
        meta: None,
    }]))
}

async fn read_object_resource(
    uri: &str,
    id_segment: &str,
    handle: &ViewerInspectionHandle,
    config: McpServerConfig,
) -> Result<ReadResourceResult, ErrorData> {
    if id_segment.is_empty() {
        return Err(ErrorData::invalid_params("object id is required", None));
    }
    let object_id = if Uuid::parse_str(id_segment).is_ok() {
        id_segment.to_owned()
    } else if let Ok(local_id) = id_segment.parse::<u32>() {
        let Some(session) = handle.session() else {
            return Err(ErrorData::resource_not_found(format!("unknown object: {id_segment}"), None));
        };
        let page = session
            .world
            .objects(None, 256)
            .await
            .map_err(|error| ErrorData::internal_error(format!("world objects query failed: {error}"), None))?;
        let Some(found) = page.objects.into_iter().find(|item| item.address.local_id().get() == local_id) else {
            return Err(ErrorData::resource_not_found(format!("unknown object: {id_segment}"), None));
        };
        found.id.to_string()
    } else {
        return Err(ErrorData::invalid_params(format!("invalid object id: {id_segment}"), None));
    };

    let Some(tool) = find_tool("explain_object") else {
        return Err(ErrorData::internal_error("backing tool missing", None));
    };
    let outcome = time::timeout(config.request_timeout, (tool.handler)(handle.clone(), json!({ "object_id": object_id })))
        .await
        .map_err(|_| ErrorData::new(ErrorCode(-32001), "resource read timed out", None))?;
    let content = outcome.map_err(|message| ErrorData::internal_error(message, None))?;
    let ToolContent::Json(value) = content else {
        return Err(ErrorData::internal_error("unexpected non-JSON tool output", None));
    };
    if value.get("active_session").and_then(Value::as_bool) == Some(false) || value.get("found").and_then(Value::as_bool) == Some(false) {
        return Err(ErrorData::resource_not_found(format!("unknown object: {id_segment}"), None));
    }
    let text = value.to_string();
    let limit = tool.max_response_bytes.min(config.max_response_bytes);
    if text.len() > limit {
        return Err(ErrorData::new(ErrorCode(-32002), "response exceeded size limit", None));
    }
    Ok(ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
        uri: uri.to_owned(),
        mime_type: Some("application/json".to_owned()),
        text,
        meta: None,
    }]))
}
