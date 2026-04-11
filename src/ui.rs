use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal;

use crate::state::{BannerHotkey, InputMode, PlayerState, UiState, ViewMode};

#[cfg(target_os = "macos")]
fn choose_path_with_dialog_macos() -> Option<PathBuf> {
    use std::process::Command;
    let script = r#"
      try
        set p to choose folder with prompt "选择要播放的目录（或包含音频的文件夹）"
        POSIX path of p
      on error number -128
        ""
      end try
    "#;
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let t = s.trim();
    if t.is_empty() { None } else { Some(PathBuf::from(t)) }
}

#[cfg(target_os = "windows")]
fn choose_path_with_dialog_windows() -> Option<PathBuf> {
    use std::process::Command;

    // Use Shell.Application BrowseForFolder to show the standard folder picker.
    // Returns an empty string on cancel.
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$shell = New-Object -ComObject Shell.Application
$folder = $shell.BrowseForFolder(0, '选择要播放的目录（或包含音频的文件夹）', 0, 0)
if ($null -eq $folder) { '' } else { $folder.Self.Path }
"#;

    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let t = s.trim();
    if t.is_empty() { None } else { Some(PathBuf::from(t)) }
}

fn choose_path_with_dialog() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        return choose_path_with_dialog_macos();
    }
    #[cfg(target_os = "windows")]
    {
        return choose_path_with_dialog_windows();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

fn read_path_from_user(ui: &mut UiState, prompt: &str) -> Option<PathBuf> {
    // Temporarily exit raw mode so stdin line editing works.
    //
    // Important: don't let the prompt permanently shift the TUI down.
    // We save/restore the cursor position and clear the prompt line(s) afterwards.
    //
    // 必须同时关闭 SGR 鼠标：否则鼠标移动/点击会以 ^[[<… 序列进入 stdin，
    // 在行缓冲模式下被 echo 到提示行（用户按 O 打开路径时尤为明显）。
    print!("\x1B[s"); // Save cursor position
    let _ = io::stdout().flush();

    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = io::stdout().flush();
    let _ = terminal::disable_raw_mode();
    print!("\n\r\x1B[0m\x1B[?25h\x1B[2K{prompt}");
    let _ = io::stdout().flush();

    let mut s = String::new();
    let ok = io::stdin().read_line(&mut s).is_ok();

    // Clean prompt area and return to previous cursor position
    print!("\r\x1B[2K\x1B[u"); // Clear current line, restore cursor
    let _ = io::stdout().flush();

    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let _ = io::stdout().flush();
    print!("\x1B[?25l");
    let _ = io::stdout().flush();

    // macOS Terminal sometimes glitches cursor restore after scroll; force a full redraw
    // (same path as terminal resize) so layout is stable without manual resizing.
    ui.terminal_resized = true;

    if !ok {
        return None;
    }
    let t = s.trim();
    if t.is_empty() { return None; }
    Some(PathBuf::from(t))
}

fn switch_source_paths(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>, new_source: PathBuf) {
    if !new_source.exists() {
        ui.set_status(format!("路径不存在: {}", new_source.display()));
        return;
    }

    let old_playlist = playlist.clone();

    let mut combined: Vec<PathBuf> = Vec::new();
    match crate::playlist::build_playlist(&new_source, false) {
        Ok(tracks) => combined.extend(tracks),
        Err(e) => {
            ui.set_status(format!("无法读取: {} ({})", new_source.display(), e));
            return;
        }
    }

    // Deduplicate by canonical path (same logic as main.rs)
    let mut seen = std::collections::HashSet::new();
    combined.retain(|p| {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        seen.insert(key)
    });

    if combined.is_empty() {
        ui.set_status("目录内没有可播放音频".to_string());
        return;
    }

    *playlist = combined;
    ui.source_paths = vec![new_source];
    ui.current = 0;
    ui.cursor = 0;
    ui.scroll_offset = 0;
    ui.filtered_indices.clear();
    ui.view_mode = ViewMode::Player;
    ui.playlist_dirty = true;
    ui.session_idle = false;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);

    // Reindex metadata cache: cancel old scan, remap entries, spawn new scan
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, &old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));

    // Force producer restart with fresh playlist snapshot.
    state.jump_to(0);
    ui.set_status(format!("已切换目录: {}", ui.source_paths[0].display()));
}

pub fn format_time(secs: f64) -> String {
    format!("{:02}:{:02}", (secs / 60.0) as u32, (secs % 60.0) as u32)
}

/// 执行 banner 第二行热键对应的动作（键盘与鼠标共用）。
fn apply_banner_hotkey(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>, hk: BannerHotkey) {
    match hk {
        BannerHotkey::Eq => state.cycle_eq(),
        BannerHotkey::Fx => state.cycle_effects(),
        BannerHotkey::Crossfeed => state.cycle_crossfeed(),
        BannerHotkey::Fader => state.toggle_pre_fader(),
        BannerHotkey::VizMode => state.cycle_viz_mode(),
        BannerHotkey::VizStyle => state.toggle_viz_style(),
        BannerHotkey::Info => state.toggle_stats(),
        BannerHotkey::List => {
            ui.view_mode = match ui.view_mode {
                ViewMode::Player | ViewMode::Lyrics => {
                    ui.cursor = ui.current;
                    ensure_cursor_visible(ui, playlist);
                    ViewMode::Playlist
                }
                ViewMode::Playlist => ViewMode::Player,
            };
        }
        BannerHotkey::Lyrics => {
            ui.view_mode = match ui.view_mode {
                ViewMode::Player | ViewMode::Playlist => {
                    ui.lyrics_scroll = 0;
                    ui.lyrics_auto_scroll = true;
                    ViewMode::Lyrics
                }
                ViewMode::Lyrics => ViewMode::Player,
            };
        }
        BannerHotkey::Open => {
            if let Some(p) = read_path_from_user(ui, "打开目录/文件: ") {
                switch_source_paths(state, ui, playlist, p);
            } else {
                ui.set_status("已取消".to_string());
            }
        }
        BannerHotkey::Pick => {
            if let Some(p) = choose_path_with_dialog() {
                switch_source_paths(state, ui, playlist, p);
            } else {
                ui.set_status("已取消".to_string());
            }
        }
        BannerHotkey::Shuffle => {
            ui.shuffle = !ui.shuffle;
            ui.pending_resume_save = true;
            ui.set_status(if ui.shuffle {
                "随机播放：开（再次列表循环时重排）".to_string()
            } else {
                "随机播放：关".to_string()
            });
        }
        BannerHotkey::LoopToggle => {
            ui.repeat = !ui.repeat;
            ui.pending_resume_save = true;
            if ui.session_idle && ui.repeat && !playlist.is_empty() {
                ui.current = 0;
                ui.session_idle = false;
            }
            ui.set_status(if ui.repeat {
                "列表循环：开".to_string()
            } else {
                "列表循环：关".to_string()
            });
        }
    }
}

pub fn poll_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) -> bool {
    // Drain all pending events for responsive input
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        if let Event::Resize(_, _) = ev {
            ui.terminal_resized = true;
            continue;
        }

        if let Event::Mouse(me) = ev {
            if me.kind != MouseEventKind::Down(MouseButton::Left) {
                continue;
            }
            if !matches!(ui.input_mode, InputMode::Normal) {
                continue;
            }
            let col = me.column;
            let row = me.row;
            for (r, hk) in &ui.banner_hotkey_regions {
                if col >= r.x
                    && col < r.x.saturating_add(r.w)
                    && row >= r.y
                    && row < r.y.saturating_add(r.h)
                {
                    apply_banner_hotkey(state, ui, playlist, *hk);
                    break;
                }
            }
            continue;
        }

        let k = match ev {
            Event::Key(k) => k,
            _ => continue,
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }

            // In text input mode, route to text handler
            match &ui.input_mode {
                InputMode::Search(_) | InputMode::SavePlaylist(_) => {
                    return handle_text_input(state, ui, playlist, k);
                }
                InputMode::Normal => {}
            }

            // Lyrics view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Lyrics {
                match k {
                    KeyEvent { code: KeyCode::Char('w'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        ui.lyrics_scroll = ui.lyrics_scroll.saturating_sub(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('s'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        if let Some(ref lyrics) = ui.lyrics {
                            let max = lyrics.line_count().saturating_sub(1);
                            if ui.lyrics_scroll < max {
                                ui.lyrics_scroll += 1;
                            }
                        }
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('d'), .. } => {
                        ui.lyrics_offset += 0.5;
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        ui.lyrics_offset -= 0.5;
                        continue;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } |
                    KeyEvent { code: KeyCode::Char('y'), .. } => {
                        ui.view_mode = ViewMode::Player;
                        continue;
                    }
                    _ => {} // Fall through to global keys
                }
            }

            // Playlist view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Playlist {
                match k {
                    KeyEvent { code: KeyCode::Up, .. } => {
                        playlist_cursor_up(ui);
                        continue; // Drain remaining events for smooth scrolling
                    }
                    KeyEvent { code: KeyCode::Down, .. } => {
                        playlist_cursor_down(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Enter, .. } => {
                        let target = if ui.filtered_indices.is_empty() {
                            ui.cursor
                        } else {
                            ui.filtered_indices.get(ui.cursor).copied().unwrap_or(ui.cursor)
                        };
                        state.jump_to(target);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('/'), .. } => {
                        ui.input_mode = InputMode::Search(String::new());
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('d'), .. } |
                    KeyEvent { code: KeyCode::Delete, .. } => {
                        remove_track(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } => {
                        ui.view_mode = ViewMode::Player;
                        return false;
                    }
                    _ => {} // Fall through to global keys
                }
            }

            // Global keys (work in all view modes)
            match k {
                KeyEvent { code: KeyCode::Char(' '), .. } => state.toggle_pause(),
                KeyEvent { code: KeyCode::Up, .. } => state.next(),
                KeyEvent { code: KeyCode::Down, .. } => state.prev(),
                KeyEvent { code: KeyCode::Right, .. } => state.seek(10),
                KeyEvent { code: KeyCode::Left, .. } => state.seek(-10),
                KeyEvent { code: KeyCode::Char('v'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::VizMode),
                KeyEvent { code: KeyCode::Char('+'), .. } |
                KeyEvent { code: KeyCode::Char('='), .. } => state.volume_up(),
                KeyEvent { code: KeyCode::Char('-'), .. } => state.volume_down(),
                KeyEvent { code: KeyCode::Char('e'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::Eq),
                KeyEvent { code: KeyCode::Char('x'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::Fx),
                KeyEvent { code: KeyCode::Char('f'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::Fader),
                KeyEvent { code: KeyCode::Char('b'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::VizStyle),
                KeyEvent { code: KeyCode::Char('l'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::List),
                KeyEvent { code: KeyCode::Char('y'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::Lyrics),
                KeyEvent { code: KeyCode::Char('s'), .. } => {
                    ui.input_mode = InputMode::SavePlaylist(String::new());
                }
                KeyEvent { code: KeyCode::Char('r'), .. } => {
                    rescan(state, ui, playlist);
                }
                KeyEvent { code: KeyCode::Char('g'), .. } => {
                    apply_banner_hotkey(state, ui, playlist, BannerHotkey::Shuffle);
                }
                KeyEvent { code: KeyCode::Char('t'), .. } => {
                    apply_banner_hotkey(state, ui, playlist, BannerHotkey::LoopToggle);
                }
                KeyEvent { code: KeyCode::Char('o'), .. } => {
                    apply_banner_hotkey(state, ui, playlist, BannerHotkey::Open);
                }
                KeyEvent { code: KeyCode::Char('p'), .. } => {
                    apply_banner_hotkey(state, ui, playlist, BannerHotkey::Pick);
                }
                KeyEvent { code: KeyCode::Char('q'), .. } |
                KeyEvent { code: KeyCode::Esc, .. } => { state.quit(); return true; }
                KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } => {
                    state.quit(); return true;
                }
                KeyEvent { code: KeyCode::Char('c'), .. } => {
                    apply_banner_hotkey(state, ui, playlist, BannerHotkey::Crossfeed);
                }
                KeyEvent { code: KeyCode::Char('i'), .. } => apply_banner_hotkey(state, ui, playlist, BannerHotkey::Info),
                KeyEvent { code: KeyCode::Char('['), .. } => state.balance_left(),
                KeyEvent { code: KeyCode::Char(']'), .. } => state.balance_right(),
                _ => {}
            }
    }
    false
}

fn handle_text_input(state: &PlayerState, ui: &mut UiState, _playlist: &mut Vec<PathBuf>, key: KeyEvent) -> bool {
    match &mut ui.input_mode {
        InputMode::Search(ref mut query) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Enter => {
                    let target = if ui.filtered_indices.is_empty() {
                        ui.cursor
                    } else {
                        ui.filtered_indices.get(ui.cursor).copied().unwrap_or(0)
                    };
                    state.jump_to(target);
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Up => {
                    playlist_cursor_up(ui);
                }
                KeyCode::Down => {
                    playlist_cursor_down(ui, _playlist);
                }
                _ => {}
            }
        }
        InputMode::SavePlaylist(ref mut name) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                }
                KeyCode::Enter => {
                    let save_name = name.clone();
                    ui.input_mode = InputMode::Normal;
                    if !save_name.is_empty() {
                        match crate::playlist::save_m3u(_playlist, &save_name) {
                            Ok(path) => {
                                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                                ui.set_status(format!("Saved {} tracks to {}", _playlist.len(), fname));
                            }
                            Err(e) => {
                                ui.set_status(format!("Save failed: {}", e));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => {
                    name.push(c);
                }
                _ => {}
            }
        }
        InputMode::Normal => {}
    }
    false
}

fn rebuild_filter(ui: &mut UiState, playlist: &[PathBuf]) {
    let query = match &ui.input_mode {
        InputMode::Search(q) => q.to_lowercase(),
        _ => return,
    };

    if query.is_empty() {
        ui.filtered_indices.clear();
        ui.cursor = 0;
        ui.scroll_offset = 0;
        return;
    }

    let cache = &ui.metadata_cache;
    ui.filtered_indices = playlist.iter()
        .enumerate()
        .filter(|(i, p)| {
            cache.search_matches(*i, p, &query)
        })
        .map(|(i, _)| i)
        .collect();

    ui.cursor = 0;
    ui.scroll_offset = 0;
}

fn playlist_cursor_up(ui: &mut UiState) {
    if ui.cursor > 0 {
        ui.cursor -= 1;
        if ui.cursor < ui.scroll_offset {
            ui.scroll_offset = ui.cursor;
        }
    }
}

fn playlist_cursor_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    if ui.cursor < max {
        ui.cursor += 1;
    }
}

fn ensure_cursor_visible(ui: &mut UiState, _playlist: &[PathBuf]) {
    if ui.cursor < ui.scroll_offset {
        ui.scroll_offset = ui.cursor;
    }
}

fn remove_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if playlist.len() <= 1 {
        ui.set_status("Can't remove the last track".to_string());
        return;
    }

    // Resolve cursor to actual playlist index
    let track_idx = if ui.filtered_indices.is_empty() {
        ui.cursor
    } else {
        match ui.filtered_indices.get(ui.cursor) {
            Some(&idx) => idx,
            None => return,
        }
    };
    if track_idx >= playlist.len() { return; }

    let removed_name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);

    // Track removed path so repeat cycle doesn't bring it back
    if let Ok(canon) = std::fs::canonicalize(&playlist[track_idx]) {
        ui.removed_paths.insert(canon);
    } else {
        ui.removed_paths.insert(playlist[track_idx].clone());
    }

    // Remove from playlist and metadata cache
    playlist.remove(track_idx);
    ui.metadata_cache.remove_at(track_idx);

    // Adjust current track index
    if track_idx == ui.current {
        // Removing current track: ui.current now points to the right next track
        ui.current = ui.current.min(playlist.len().saturating_sub(1));
        state.next(); // Signal producer to skip current track
        ui.current_track_removed = true; // dirty handler should jump to ui.current, not ui.current+1
    } else if track_idx < ui.current {
        ui.current -= 1;
    }

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);
    ui.playlist_dirty = true;

    // Rebuild filter if searching, otherwise just adjust cursor
    if !ui.filtered_indices.is_empty() {
        rebuild_filter(ui, playlist);
    }
    let max_cursor = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    if ui.cursor > max_cursor {
        ui.cursor = max_cursor;
    }

    ui.set_status(format!("Removed: {}", removed_name));
}

fn rescan(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    use std::sync::atomic::Ordering;

    let old_playlist = playlist.clone();
    let current_track_path = playlist.get(ui.current).cloned();
    let mut total_added = 0usize;
    let mut total_removed = 0usize;
    let mut had_error = false;

    for source in ui.source_paths.clone() {
        match crate::playlist::rescan_playlist(
            &source,
            playlist,
            current_track_path.as_deref(),
        ) {
            Ok((added, removed)) => {
                total_added += added;
                total_removed += removed;
            }
            Err(_) => { had_error = true; }
        }
    }

    // Deduplicate after rescan
    let mut seen = std::collections::HashSet::new();
    playlist.retain(|p| {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        seen.insert(key)
    });

    // Find current track's new index
    if let Some(ref track_path) = current_track_path {
        if let Some(new_idx) = playlist.iter().position(|p| p == track_path) {
            ui.current = new_idx;
        } else {
            ui.current = ui.current.min(playlist.len().saturating_sub(1));
        }
    }

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);

    // Reindex metadata cache: cancel old scan, remap entries, spawn new scan
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, &old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));

    if playlist.is_empty() || (playlist.len() == 1 && total_removed > 0 && current_track_path.is_some()) {
        ui.set_status("All files removed, finishing current track".to_string());
    } else if total_added == 0 && total_removed == 0 && !had_error {
        ui.set_status("No changes found".to_string());
    } else if had_error && total_added == 0 && total_removed == 0 {
        ui.set_status("Rescan failed for some sources".to_string());
    } else {
        ui.set_status(format!("+{} added, -{} removed", total_added, total_removed));
    }
}
