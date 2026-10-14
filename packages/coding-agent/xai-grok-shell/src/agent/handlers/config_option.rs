//! Standard ACP session configuration writes.
//!
//! Flow: validate the selector -> serialize with other client dispatches -> use
//! the existing model-change path -> return the freshly resolved selectors.
//! Model and provider permission checks stay in `set_session_model`. Effort IDs
//! are resolved against the session's current model, including provider aliases.

use agent_client_protocol::{self as acp, Agent};

use crate::agent::mvp_agent::MvpAgent;
use crate::agent::session_config::{
    CONFIG_ID_MODEL, CONFIG_ID_REASONING_EFFORT, legacy_session_effort_options,
};
use xai_grok_sampling_types::{REASONING_EFFORT_META_KEY, reasoning_effort_meta_value};

/// Apply one model or reasoning selector and return the current ACP options.
///
/// Main callers: MvpAgent's session/set_config_option request handler.
/// Why this exists: editors use the standard ACP settings request, while the
/// app's existing model-change path already owns model and provider validation.
pub(crate) async fn apply(
    agent: &MvpAgent,
    args: acp::SetSessionConfigOptionRequest,
) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
    let value = args.value.as_value_id().ok_or_else(|| {
        acp::Error::invalid_params().data("model and reasoning_effort require string values")
    })?;
    let session_id = args.session_id;
    let handle = agent
        .session_handle_waiting_for_load(&session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
    let request = match args.config_id.0.as_ref() {
        CONFIG_ID_MODEL => {
            acp::SetSessionModelRequest::new(session_id.clone(), acp::ModelId::new(value.0.clone()))
        }
        CONFIG_ID_REASONING_EFFORT => {
            let models = agent.models_manager.models();
            let catalog_id =
                crate::agent::models::resolve_catalog_key(&models, &handle.model_id)
                    .ok_or_else(|| acp::Error::invalid_params().data("unknown session model"))?;
            let model_id = catalog_id.0.as_ref();
            if !agent
                .models_manager
                .model_supports_reasoning_effort(model_id)
            {
                return Err(acp::Error::invalid_params()
                    .data("this model does not support reasoning_effort"));
            }
            let mut options = agent.models_manager.model_reasoning_efforts(model_id);
            if options.is_empty() {
                options = legacy_session_effort_options();
            }
            let effort = options
                .iter()
                .find(|option| option.id == value.0.as_ref())
                .ok_or_else(|| {
                    acp::Error::invalid_params().data("unknown reasoning_effort value")
                })?;
            let mut meta = serde_json::Map::new();
            meta.insert(
                REASONING_EFFORT_META_KEY.to_owned(),
                reasoning_effort_meta_value(effort.value),
            );
            // Use the catalog key so the existing model-change path applies effort
            // capability checks to the same entry we validated above.
            acp::SetSessionModelRequest::new(session_id.clone(), catalog_id).meta(meta)
        }
        other => {
            return Err(
                acp::Error::invalid_params().data(format!("unknown config option: {other}"))
            );
        }
    };
    agent.set_session_model(request).await?;
    let state = agent.model_state(Some(&session_id));
    let options = agent.acp_config_options_for_session(Some(&session_id), &state);
    Ok(acp::SetSessionConfigOptionResponse::new(options))
}
