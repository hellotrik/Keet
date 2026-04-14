//! 视频曲目：外部 `mpv` 窗口 + JSON IPC（Unix）同步进度/暂停/音量，使 TUI 与音乐行为一致。
//!
//! **顺序**：先拉 mpv 快照（窗口操作为准）→ `poll_input` → 将终端侧暂停/快进/音量推送到 mpv → 绘制。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::Terminal;
use souvlaki::MediaControls;

use crate::crossfeed::CrossfeedPreset;
use crate::effects::EffectsPreset;
use crate::eq::EqPreset;
use crate::media_keys;
use crate::player_ratatui;
use crate::resume::{build_resume_state, save_state};
use crate::state::{PlayerState, UiState};
use crate::track::Track;
use crate::ui::poll_input;
use crate::viz::StatsMonitor;

#[cfg(unix)]
use crate::mpv_ipc::MpvIpc;

/// `true`：用户请求退出整个应用；`false`：继续外层 `playlist` 循环。
pub fn run_mpv_video_session<B: Backend>(
    path: &Path,
    filename: &str,
    ext: &str,
    state: &Arc<PlayerState>,
    ui: &mut UiState,
    playlist: &mut Vec<Track>,
    tui_terminal: &mut Terminal<B>,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
    device_arg: &Option<String>,
    stats: &mut StatsMonitor,
    media_controls: &mut Option<MediaControls>,
) -> bool {
    #[cfg(unix)]
    {
        let sock = std::env::temp_dir().join(format!("keet_mpv_{}.sock", std::process::id()));
        match MpvIpc::spawn(path, sock) {
            Ok(ipc) => {
                return run_with_ipc(
                    ipc,
                    filename,
                    ext,
                    state,
                    ui,
                    playlist,
                    tui_terminal,
                    eq_presets,
                    fx_presets,
                    cf_presets,
                    device_arg,
                    stats,
                    media_controls,
                );
            }
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("mpv IPC 不可用: {}", e));
                }
            }
        }
    }

    run_without_ipc(
        path,
        filename,
        ext,
        state,
        ui,
        playlist,
        tui_terminal,
        eq_presets,
        fx_presets,
        cf_presets,
        device_arg,
        stats,
        media_controls,
    )
}

#[cfg(unix)]
fn run_with_ipc<B: Backend>(
    mut ipc: MpvIpc,
    filename: &str,
    ext: &str,
    state: &Arc<PlayerState>,
    ui: &mut UiState,
    playlist: &mut Vec<Track>,
    tui_terminal: &mut Terminal<B>,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
    device_arg: &Option<String>,
    stats: &mut StatsMonitor,
    media_controls: &mut Option<MediaControls>,
) -> bool {
    state.set_external_playback_active(true);
    state.track_info_ready.store(true, Ordering::Relaxed);
    let vol0 = state.volume.load(Ordering::Relaxed);
    let _ = ipc.set_volume_keet(vol0);

    let track_info = "视频 · mpv（终端与窗口均可控）";
    let mut last_ui = Instant::now()
        .checked_sub(Duration::from_millis(60))
        .unwrap_or_else(Instant::now);
    let mut last_vol = vol0;

    loop {
        if state.should_quit() {
            let _ = ipc.kill_child();
            state.clear_external_playback();
            return true;
        }

        let tick_ui = last_ui.elapsed() >= Duration::from_millis(50);

        // mpv → 状态：进度、暂停（与绘制同频，减轻 IPC 压力）
        if tick_ui {
            if let Ok(snap) = ipc.poll_snapshot() {
                state.set_external_time_duration(snap.time_pos, snap.duration);
                state.set_paused(snap.paused);
            }
        }

        if tick_ui {
            if let Some(mc) = media_controls.as_mut() {
                media_keys::update_metadata(mc, filename, state.total_secs());
                media_keys::update_playback(mc, state.is_paused(), state.time_secs());
            }
        }

        if poll_input(state, ui, playlist) {
            let _ = ipc.kill_child();
            state.clear_external_playback();
            return true;
        }

        if ui.pending_resume_save {
            save_state(&build_resume_state(
                ui,
                playlist,
                state,
                eq_presets,
                fx_presets,
                cf_presets,
                device_arg,
            ));
            ui.pending_resume_save = false;
        }

        // 终端侧 → mpv：暂停、音量、相对跳转
        let _ = ipc.set_pause(state.is_paused());
        let v = state.volume.load(Ordering::Relaxed);
        if v != last_vol {
            let _ = ipc.set_volume_keet(v);
            last_vol = v;
        }
        let sk = state.take_seek();
        if sk != 0 {
            let _ = ipc.seek_relative(sk);
        }

        if let Some(j) = state.take_jump() {
            let _ = ipc.kill_child();
            state.clear_external_playback();
            ui.current = j.min(playlist.len().saturating_sub(1));
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_next() {
            let _ = ipc.kill_child();
            state.clear_external_playback();
            ui.current = (ui.current + 1).min(playlist.len());
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_prev() {
            let _ = ipc.kill_child();
            state.clear_external_playback();
            ui.current = ui.current.saturating_sub(1);
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }

        match ipc.try_wait_child() {
            Ok(Some(_)) => {
                state.clear_external_playback();
                ui.current = (ui.current + 1).min(playlist.len());
                state.current_track.store(ui.current, Ordering::Relaxed);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                state.clear_external_playback();
                ui.current = (ui.current + 1).min(playlist.len());
                state.current_track.store(ui.current, Ordering::Relaxed);
                break;
            }
        }

        if tick_ui {
            if ui.terminal_resized {
                ui.terminal_resized = false;
                let _ = tui_terminal.clear();
            }
            let _ = player_ratatui::draw_player(
                tui_terminal,
                state,
                ui,
                playlist,
                filename,
                track_info,
                ext,
                &eq_presets[state.eq_index()],
                &fx_presets[state.effects_index()].name,
                &cf_presets[state.crossfeed_index()].name,
                stats,
            );
            last_ui = Instant::now();
        }

        media_keys::poll();
        thread::sleep(Duration::from_millis(10));
    }

    false
}

fn run_without_ipc<B: Backend>(
    path: &Path,
    filename: &str,
    ext: &str,
    state: &Arc<PlayerState>,
    ui: &mut UiState,
    playlist: &mut Vec<Track>,
    tui_terminal: &mut Terminal<B>,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
    device_arg: &Option<String>,
    stats: &mut StatsMonitor,
    media_controls: &mut Option<MediaControls>,
) -> bool {
    let mut child: Option<Child> = match Command::new("mpv")
        .args([
            "--really-quiet",
            "--no-terminal",
            "--force-window=yes",
            "--keep-open=no",
            "--geometry=+0+0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => Some(c),
        Err(e) => {
            if let Ok(mut err) = state.decode_error.lock() {
                *err = Some(format!("无法启动 mpv（请安装并加入 PATH）: {}", e));
            }
            ui.current = (ui.current + 1).min(playlist.len());
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
    };

    state.track_info_ready.store(true, Ordering::Relaxed);
    let track_info = "视频 · mpv（无 IPC：进度/暂停请在播放器窗口操作）";
    let mut last_ui = Instant::now();

    loop {
        if state.should_quit() {
            if let Some(ref mut c) = child {
                let _ = c.kill();
            }
            return true;
        }

        if let Some(mc) = media_controls.as_mut() {
            media_keys::update_metadata(mc, filename, 0.0);
            media_keys::update_playback(mc, state.is_paused(), 0.0);
        }

        if poll_input(state, ui, playlist) {
            if let Some(ref mut c) = child {
                let _ = c.kill();
            }
            return true;
        }

        if ui.pending_resume_save {
            save_state(&build_resume_state(
                ui,
                playlist,
                state,
                eq_presets,
                fx_presets,
                cf_presets,
                device_arg,
            ));
            ui.pending_resume_save = false;
        }

        if let Some(j) = state.take_jump() {
            if let Some(ref mut c) = child {
                let _ = c.kill();
            }
            ui.current = j.min(playlist.len().saturating_sub(1));
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_next() {
            if let Some(ref mut c) = child {
                let _ = c.kill();
            }
            ui.current = (ui.current + 1).min(playlist.len());
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_prev() {
            if let Some(ref mut c) = child {
                let _ = c.kill();
            }
            ui.current = ui.current.saturating_sub(1);
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }

        if let Some(ref mut c) = child {
            match c.try_wait() {
                Ok(Some(_status)) => {
                    ui.current = (ui.current + 1).min(playlist.len());
                    state.current_track.store(ui.current, Ordering::Relaxed);
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    ui.current = (ui.current + 1).min(playlist.len());
                    state.current_track.store(ui.current, Ordering::Relaxed);
                    break;
                }
            }
        } else {
            break;
        }

        if last_ui.elapsed() >= Duration::from_millis(50) {
            if ui.terminal_resized {
                ui.terminal_resized = false;
                let _ = tui_terminal.clear();
            }
            let _ = player_ratatui::draw_player(
                tui_terminal,
                state,
                ui,
                playlist,
                filename,
                track_info,
                ext,
                &eq_presets[state.eq_index()],
                &fx_presets[state.effects_index()].name,
                &cf_presets[state.crossfeed_index()].name,
                stats,
            );
            last_ui = Instant::now();
        }

        media_keys::poll();
        thread::sleep(Duration::from_millis(20));
    }

    false
}
