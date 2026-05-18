use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::SunshineConfig;
use crate::error::Result;
use crate::state::AppState;
use crate::sunshine::{PairedClient, PendingPairing, SunshineServiceStatus};

use super::apply::{apply_sunshine_config, try_apply_lock, ConfigApplyOptions};
use super::types::SunshineConfigUpdate;

#[derive(Debug, Serialize)]
pub struct SunshineConfigResponse {
    pub enabled: bool,
    pub bind: String,
    pub http_port: u16,
    pub https_port: u16,
    pub hostname: String,
    pub unique_id: String,
    pub app_id: u32,
    pub app_title: String,
}

impl From<&SunshineConfig> for SunshineConfigResponse {
    fn from(config: &SunshineConfig) -> Self {
        Self {
            enabled: config.enabled,
            bind: config.bind.clone(),
            http_port: config.http_port,
            https_port: config.https_port,
            hostname: config.hostname.clone(),
            unique_id: config.unique_id.clone(),
            app_id: config.app_id,
            app_title: config.app_title.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SunshineStatusResponse {
    pub config: SunshineConfigResponse,
    pub service_status: String,
    pub pending_pairings: Vec<PendingPairing>,
    pub clients: Vec<PairedClient>,
}

#[derive(Debug, Deserialize)]
pub struct SunshinePinRequest {
    pub pin: String,
    pub name: Option<String>,
}

pub async fn get_sunshine_config(
    State(state): State<Arc<AppState>>,
) -> Json<SunshineConfigResponse> {
    Json(SunshineConfigResponse::from(&state.config.get().sunshine))
}

pub async fn get_sunshine_status(
    State(state): State<Arc<AppState>>,
) -> Json<SunshineStatusResponse> {
    let config = state.config.get().sunshine.clone();
    let service = state.sunshine.read().await.clone();

    let (service_status, pending_pairings, clients) = if let Some(service) = service {
        let status = match service.status().await {
            SunshineServiceStatus::Stopped => "stopped".to_string(),
            SunshineServiceStatus::Running => "running".to_string(),
            SunshineServiceStatus::Error(message) => format!("error:{message}"),
        };
        (
            status,
            service.pending_pairings().await,
            service.paired_clients().await,
        )
    } else {
        ("not_initialized".to_string(), Vec::new(), Vec::new())
    };

    Json(SunshineStatusResponse {
        config: SunshineConfigResponse::from(&config),
        service_status,
        pending_pairings,
        clients,
    })
}

pub async fn update_sunshine_config(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SunshineConfigUpdate>,
) -> Result<Json<SunshineConfigResponse>> {
    req.validate()?;

    let _apply_guard = try_apply_lock(&state.config_apply_locks.sunshine, "sunshine")?;
    let old_config = state.config.get().sunshine.clone();

    state
        .config
        .update(|config| {
            req.apply_to(&mut config.sunshine);
        })
        .await?;

    let new_config = state.config.get().sunshine.clone();
    apply_sunshine_config(
        &state,
        &old_config,
        &new_config,
        ConfigApplyOptions::forced(),
    )
    .await?;

    Ok(Json(SunshineConfigResponse::from(&new_config)))
}

pub async fn submit_sunshine_pin(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SunshinePinRequest>,
) -> Result<Json<SunshineStatusResponse>> {
    let service = state.sunshine.read().await.clone().ok_or_else(|| {
        crate::error::AppError::BadRequest("Sunshine service is not running".into())
    })?;

    service.submit_pin(req.pin, req.name).await?;
    Ok(get_sunshine_status(State(state)).await)
}
