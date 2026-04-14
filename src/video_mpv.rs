//! 视频曲目：外部 `mpv` 窗口 + JSON IPC（Unix）同步进度/暂停/音量，使 TUI 与音乐行为一致。
//!
//! **顺序**：先拉 mpv 快照（窗口操作为准）→ `poll_input` → 将终端侧暂停/快进/音量推送到 mpv → 绘制。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::mpsc;
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
use crate::track::{MediaKind, Track};
use crate::ui::poll_input;
use crate::viz::StatsMonitor;

#[cfg(unix)]
use crate::mpv_ipc::MpvIpc;

#[cfg(target_os = "macos")]
fn restore_tui_focus_best_effort_macos() {
    // mpv 在切片（loadfile replace）时可能会把窗口激活到前台。
    // 这里不依赖 mpv 的 nofocus 参数，而是在切换成功后把焦点尽量拉回到宿主终端应用。
    //
    // Cursor/VS Code 集成终端一般属于 “Cursor” App；独立终端常见是 iTerm/Terminal。
    let script = r#"
        tell application "System Events"
          set appNames to {"Cursor", "iTerm", "Terminal"}
          repeat with n in appNames
            try
              tell application (contents of n) to activate
              exit repeat
            end try
          end repeat
        end tell
    "#;
    let _ = Command::new("osascript")
        .args(["-e", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(target_os = "macos"))]
fn restore_tui_focus_best_effort_macos() {}

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
    ipc: MpvIpc,
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

    let track_info = "视频 · mpv（终端与窗口均可控）";
    let mut cur_filename = filename.to_string();
    let mut cur_ext = ext.to_string();
    let mut last_ui = Instant::now()
        .checked_sub(Duration::from_millis(60))
        .unwrap_or_else(Instant::now);
    let mut last_vol = vol0;
    let mut last_paused = state.is_paused();
    let mut eof_reached = false;

    enum MpvCmd {
        SetPause(bool),
        SetVolume(u32),
        SeekRelative(i64),
        LoadReplace { target: usize, path: std::path::PathBuf },
        Kill,
    }

    enum MpvEvent {
        Snapshot(crate::mpv_ipc::MpvPoll),
        LoadResult { target: usize, ok: bool },
        Fatal(String),
        Exited,
    }

    fn try_queue_video_load_by_index(
        tx: &mpsc::Sender<MpvCmd>,
        target: usize,
        ui: &mut UiState,
        playlist: &[Track],
        state: &PlayerState,
    ) -> Option<(String, String)> {
        if target >= playlist.len() {
            return None;
        }
        if playlist[target].kind != MediaKind::Video {
            return None;
        }

        let path = &playlist[target].path;
        let _ = tx.send(MpvCmd::LoadReplace {
            target,
            path: path.clone(),
        });

        ui.current = target;
        state.current_track.store(ui.current, Ordering::Relaxed);

        // Immediately reset TUI time line; next snapshot will overwrite with accurate values.
        state.set_external_time_duration(0.0, 0.0);
        state.set_paused(false);

        let filename = ui.metadata_cache.display_name(ui.current, path);
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        Some((filename, ext))
    }

    // --- IPC worker thread (owning mpv IPC) ---
    let (cmd_tx, cmd_rx) = mpsc::channel::<MpvCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<MpvEvent>();

    let worker = thread::spawn(move || {
        let mut ipc = ipc;

        // Best-effort initial sync (do not block UI thread).
        let _ = ipc.set_volume_keet(vol0);
        restore_tui_focus_best_effort_macos();

        let mut next_snap = Instant::now();
        loop {
            // Drain commands with a small timeout so we can keep snapshot cadence.
            match cmd_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(cmd) => match cmd {
                    MpvCmd::SetPause(p) => {
                        let _ = ipc.set_pause(p);
                    }
                    MpvCmd::SetVolume(v) => {
                        let _ = ipc.set_volume_keet(v);
                    }
                    MpvCmd::SeekRelative(delta) => {
                        if delta != 0 {
                            let _ = ipc.seek_relative(delta);
                        }
                    }
                    MpvCmd::LoadReplace { target, path } => {
                        let ok = ipc.loadfile_replace(&path).is_ok();
                        let _ = evt_tx.send(MpvEvent::LoadResult { target, ok });
                        if ok {
                            restore_tui_focus_best_effort_macos();
                        }
                    }
                    MpvCmd::Kill => {
                        let _ = ipc.kill_child();
                        let _ = evt_tx.send(MpvEvent::Exited);
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = ipc.kill_child();
                    let _ = evt_tx.send(MpvEvent::Exited);
                    break;
                }
            }

            if Instant::now() >= next_snap {
                match ipc.poll_snapshot() {
                    Ok(snap) => {
                        let _ = evt_tx.send(MpvEvent::Snapshot(snap));
                    }
                    Err(e) => {
                        let _ = evt_tx.send(MpvEvent::Fatal(format!("mpv IPC poll failed: {e}")));
                        let _ = ipc.kill_child();
                        let _ = evt_tx.send(MpvEvent::Exited);
                        break;
                    }
                }
                next_snap = Instant::now() + Duration::from_millis(50);
            }
        }
    });

    let mut pending_load_target: Option<usize> = None;

    loop {
        if state.should_quit() {
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
            state.clear_external_playback();
            return true;
        }

        let tick_ui = last_ui.elapsed() >= Duration::from_millis(50);

        // mpv → 状态：从后台线程拉取最新快照（非阻塞，避免 UI 卡顿）
        while let Ok(ev) = evt_rx.try_recv() {
            match ev {
                MpvEvent::Snapshot(snap) => {
                    state.set_external_time_duration(snap.time_pos, snap.duration);
                    state.set_paused(snap.paused);
                    eof_reached = snap.eof_reached;
                }
                MpvEvent::LoadResult { target, ok } => {
                    // If load failed, fall back to restarting session in outer loop.
                    if !ok {
                        // Align current index so outer loop continues at the intended target.
                        ui.current = target;
                        state.current_track.store(ui.current, Ordering::Relaxed);
                        state.clear_external_playback();
                        return false;
                    }
                    // Load succeeded: clear pending state.
                    if pending_load_target == Some(target) {
                        pending_load_target = None;
                    }
                }
                MpvEvent::Fatal(msg) => {
                    if let Ok(mut err) = state.decode_error.lock() {
                        *err = Some(msg);
                    }
                    state.clear_external_playback();
                    return false;
                }
                MpvEvent::Exited => {
                    state.clear_external_playback();
                    return false;
                }
            }
        }

        if tick_ui {
            if let Some(mc) = media_controls.as_mut() {
                media_keys::update_metadata(mc, &cur_filename, state.total_secs());
                media_keys::update_playback(mc, state.is_paused(), state.time_secs());
            }
        }

        if poll_input(state, ui, playlist) {
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
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
        let paused = state.is_paused();
        if paused != last_paused {
            let _ = cmd_tx.send(MpvCmd::SetPause(paused));
            last_paused = paused;
        }
        let v = state.volume.load(Ordering::Relaxed);
        if v != last_vol {
            let _ = cmd_tx.send(MpvCmd::SetVolume(v));
            last_vol = v;
        }
        let sk = state.take_seek();
        if sk != 0 {
            let _ = cmd_tx.send(MpvCmd::SeekRelative(sk));
        }

        if let Some(j) = state.take_jump() {
            let target = j.min(playlist.len().saturating_sub(1));
            if let Some((f, e)) =
                try_queue_video_load_by_index(&cmd_tx, target, ui, playlist, state)
            {
                cur_filename = f;
                cur_ext = e;
                eof_reached = false;
                pending_load_target = Some(target);
                continue;
            }
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
            state.clear_external_playback();
            ui.current = target;
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_next() {
            let target = (ui.current + 1).min(playlist.len());
            if let Some((f, e)) =
                try_queue_video_load_by_index(&cmd_tx, target, ui, playlist, state)
            {
                cur_filename = f;
                cur_ext = e;
                eof_reached = false;
                pending_load_target = Some(target);
                continue;
            }
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
            state.clear_external_playback();
            ui.current = target;
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }
        if state.take_skip_prev() {
            let target = ui.current.saturating_sub(1);
            if let Some((f, e)) =
                try_queue_video_load_by_index(&cmd_tx, target, ui, playlist, state)
            {
                cur_filename = f;
                cur_ext = e;
                eof_reached = false;
                pending_load_target = Some(target);
                continue;
            }
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
            state.clear_external_playback();
            ui.current = target;
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
        }

        // mpv 在 --keep-open=yes 下不会自然退出；用 eof-reached 触发“下一条”并复用窗口。
        if eof_reached && pending_load_target.is_none() {
            let target = (ui.current + 1).min(playlist.len());
            if let Some((f, e)) =
                try_queue_video_load_by_index(&cmd_tx, target, ui, playlist, state)
            {
                cur_filename = f;
                cur_ext = e;
                eof_reached = false;
                pending_load_target = Some(target);
                continue;
            }
            let _ = cmd_tx.send(MpvCmd::Kill);
            let _ = worker.join();
            state.clear_external_playback();
            ui.current = target;
            state.current_track.store(ui.current, Ordering::Relaxed);
            return false;
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
                &cur_filename,
                track_info,
                &cur_ext,
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
            "--ontop=no",
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
