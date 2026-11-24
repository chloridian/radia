use rmcp::{
    ErrorData,
    handler::server::{router::prompt::PromptRouter, wrapper::Parameters},
    model::{CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptResult, PromptMessage, Reference, Role},
    prompt, prompt_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{resources::OBJECT_URI_TEMPLATE, server::RadiaServer};
use crate::inspection::ViewerInspectionHandle;

pub const MAX_COMPLETION_VALUES: usize = 50;

#[derive(Deserialize, JsonSchema)]
pub struct DiagnoseObjectArgs {
    #[schemars(description = "Object UUID to inspect.")]
    pub object_id: String,
}

#[prompt_router]
impl RadiaServer {
    #[prompt(name = "diagnose_scene", description = "Diagnose objects present in World but missing from the renderer, or an empty scene.")]
    async fn diagnose_scene(&self) -> Result<GetPromptResult, ErrorData> {
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            "Diagnose why objects in World are missing from the renderer or why the scene is empty.\n\
             Call these tools in order:\n\
             1. `build` to confirm viewer version, GPU adapter, and device limits.\n\
             2. `stats` and `world` (`query=\"summary\"`, then `query=\"objects\"`) to compare World object counts against rendered draws.\n\
             3. `explain_object` on a few representative objects to check visibility, culling, mesh residency, and material shading.\n\
             4. `logs` at `warn` level for recent renderer, asset, or session warnings.\n\
             Conclude with the specific stage where objects are dropped or delayed and the evidence from each tool.",
        )])
        .with_description("Diagnose missing rendered objects or an empty scene."))
    }

    #[prompt(name = "diagnose_object", description = "Diagnose why a specific object is missing, culled, or shaded unexpectedly.")]
    async fn diagnose_object(&self, Parameters(args): Parameters<DiagnoseObjectArgs>) -> Result<GetPromptResult, ErrorData> {
        let object_id = args.object_id.trim();
        if object_id.is_empty() {
            return Err(ErrorData::invalid_params("missing required argument: object_id", None));
        }
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Diagnose object `{object_id}`.\n\
                 Call these tools/resources in order:\n\
                 1. Read `viewer://object/{object_id}` (or call `explain_object`) to inspect World detail, placement, culling, mesh residency, and per-face materials.\n\
                 2. Call `world` (`query=\"object\"`) if you need revision and attachment metadata.\n\
                 3. Call `textures` and `logs` (`level=\"warn\"`) if a mesh or texture is pending or failed.\n\
                 Conclude with why `{object_id}` is hidden, unmeshed, or shaded as observed."
            ),
        )])
        .with_description(format!("Diagnose object {object_id}.")))
    }

    #[prompt(
        name = "diagnose_performance",
        description = "Diagnose frame time spikes, queue backpressure, texture stalls, and network issues."
    )]
    async fn diagnose_performance(&self) -> Result<GetPromptResult, ErrorData> {
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            "Diagnose viewer performance.\n\
             Call these tools in order:\n\
             1. `stats` to inspect frame time, recent spikes, GPU timings, and memory.\n\
             2. `queues` to check bounded subsystem queue depths and full/rejected counts.\n\
             3. `textures` to check fetch/decode queue depths, discard levels, and slow in-flight fetches.\n\
             4. `network` to check per-region packet rates, unacked/resent counts, and capability status.\n\
             5. `logs` at `warn` level for stalls or backpressure warnings.\n\
             Conclude with the primary bottleneck and supporting numbers.",
        )])
        .with_description("Diagnose viewer frame time and subsystem bottlenecks."))
    }
}

pub fn build_prompt_router() -> PromptRouter<RadiaServer> {
    RadiaServer::prompt_router()
}

pub async fn complete(request: &CompleteRequestParams, handle: &ViewerInspectionHandle) -> CompleteResult {
    let matches_target = match &request.r#ref {
        Reference::Prompt(prompt) => prompt.name == "diagnose_object" && request.argument.name == "object_id",
        Reference::Resource(resource) => resource.uri == OBJECT_URI_TEMPLATE && request.argument.name == "object_id",
        _ => false,
    };
    if !matches_target {
        return CompleteResult::default();
    }
    let Some(session) = handle.session() else {
        return CompleteResult::default();
    };
    let Ok(page) = session.world.objects(None, MAX_COMPLETION_VALUES + 1).await else {
        return CompleteResult::default();
    };
    let candidates = page.objects.into_iter().map(|item| item.id.to_string()).collect();
    CompleteResult::new(bound_completions(candidates, &request.argument.value))
}

pub fn bound_completions(candidates: Vec<String>, prefix: &str) -> CompletionInfo {
    let filtered: Vec<String> = candidates.into_iter().filter(|value| value.starts_with(prefix)).collect();
    let total = u32::try_from(filtered.len()).ok();
    let has_more = filtered.len() > MAX_COMPLETION_VALUES;
    let values: Vec<String> = filtered.into_iter().take(MAX_COMPLETION_VALUES).collect();
    CompletionInfo::with_all_values(values).map_or_else(
        |_| CompletionInfo::default(),
        |mut info| {
            info.total = total;
            info.has_more = Some(has_more);
            info
        },
    )
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{MAX_COMPLETION_VALUES, bound_completions};

    #[test]
    fn completion_values_are_capped_at_fifty() {
        let candidates: Vec<String> = (1u128..=120).map(|id| Uuid::from_u128(id).to_string()).collect();
        let info = bound_completions(candidates, "");
        assert_eq!(info.values.len(), MAX_COMPLETION_VALUES);
        assert_eq!(info.has_more, Some(true));
        assert_eq!(info.total, Some(120));
    }
}
