use std::path::PathBuf;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::crossfeed::CrossfeedPreset;
use crate::effects::EffectsPreset;
use crate::eq::EqPreset;
use crate::playlist::keet_config_dir;
use crate::state::{PlayerState, UiState};

#[derive(Serialize, Deserialize)]
pub struct ResumeState {
    pub source_paths: Vec<String>,
    pub track_path: String,
    pub position_secs: f64,
    pub shuffle: bool,
    pub repeat: bool,
    pub volume: u32,
    pub eq_preset: String,
    pub effects_preset: String,
    #[serde(default)]
    pub rg_mode: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub exclusive: Option<bool>,
    #[serde(default)]
    pub crossfeed_preset: Option<String>,
    #[serde(default)]
    pub balance: Option<i32>,
}

fn state_file_path() -> Option<PathBuf> {
    keet_config_dir().map(|d| d.join("state.json"))
}

pub fn save_state(state: &ResumeState) {
    if let Some(path) = state_file_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = std::fs::write(&path, json);
        }
    }
}

pub fn load_state() -> Option<ResumeState> {
    let path = state_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 从当前 UI / 播放状态构造可序列化的断点快照（供 `save_state` 写入磁盘）。
pub fn build_resume_state(
    ui: &UiState,
    playlist: &[PathBuf],
    player_state: &PlayerState,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
    device_name: &Option<String>,
) -> ResumeState {
    ResumeState {
        source_paths: ui
            .source_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        track_path: playlist
            .get(ui.current)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        position_secs: player_state.time_secs(),
        shuffle: ui.shuffle,
        repeat: ui.repeat,
        volume: player_state.volume.load(Ordering::Relaxed),
        eq_preset: eq_presets[player_state.eq_index()].name.clone(),
        effects_preset: fx_presets[player_state.effects_index()].name.clone(),
        rg_mode: Some(player_state.rg_mode().name().to_lowercase()),
        device: device_name.clone(),
        exclusive: Some(player_state.exclusive.load(Ordering::Relaxed)),
        crossfeed_preset: Some(cf_presets[player_state.crossfeed_index()].name.clone()),
        balance: Some(player_state.balance_value()),
    }
}
