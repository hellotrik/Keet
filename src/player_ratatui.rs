//! 主播放界面 Ratatui 绘制：banner、曲目/进度、可视化、播放列表与歌词视图。
//!
//! **职责**：用单帧 `Terminal::draw` 替代原 `print_status` 的手写光标与 ANSI，减轻 resize 与换行错位。
//! **设计**：与 [`crate::idle_ratatui`] 共用同一 [`Terminal`]；子区域用 `Block` / `Paragraph` / `List` 组合。
//! **输入**：键盘仍由 [`crate::ui::poll_input`] 负责；本模块每帧写入 [`UiState::banner_hotkey_regions`] 供鼠标命中；每帧可调用 [`UiState::take_status`] 一次以与旧逻辑一致。

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use ratatui::backend::Backend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::eq::{self, EqPreset};
use crate::state::{
    BannerHotkey, CellRect, InputMode, PlayerState, UiState, ViewMode, VizMode, VizStyle,
    RING_BUFFER_SIZE,
};
use crate::ui::format_time;
use crate::viz::{
    get_viz_line_count, render_spectrum_horizontal, render_spectrum_vertical, render_vu_meter,
    StatsMonitor,
};

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1B' {
            if it.peek() == Some(&'[') {
                it.next();
                while let Some(&x) = it.peek() {
                    it.next();
                    if x.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn ext_note_style(ext: &str) -> Style {
    let c = match ext {
        "mp3" => Color::Green,
        "ogg" => Color::Magenta,
        "aac" | "m4a" => Color::Red,
        "flac" | "alac" => Color::Cyan,
        "aiff" | "aif" => Color::Cyan,
        "wav" => Color::Yellow,
        _ => Color::Green,
    };
    Style::default().fg(c)
}

fn progress_bar_chars(progress: f64, bar_w: usize, viz_style: VizStyle) -> String {
    let sub = progress * bar_w as f64;
    let full = sub as usize;
    match viz_style {
        VizStyle::Dots => {
            let frac = ((sub - full as f64) * 6.0) as usize;
            const PARTIALS: &[char] = &['⣀', '⣄', '⣤', '⣦', '⣶', '⣷'];
            format!(
                "{}{}{}",
                "⣿".repeat(full),
                if full < bar_w {
                    String::from(PARTIALS[frac.min(5)])
                } else {
                    String::new()
                },
                "⣀".repeat(bar_w.saturating_sub(full + 1))
            )
        }
        VizStyle::Bars => {
            let frac = ((sub - full as f64) * 8.0) as usize;
            const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
            let mut s = String::new();
            s.push_str(&"█".repeat(full));
            if full < bar_w {
                if frac > 0 {
                    s.push(PARTIALS[(frac - 1).min(7)]);
                    s.push_str(&"▏".repeat(bar_w - full - 1));
                } else {
                    s.push_str(&"▏".repeat(bar_w - full));
                }
            }
            s
        }
    }
}

fn banner_plain(ui: &UiState) -> Text<'static> {
    let stripped = strip_ansi(&ui.banner_text);
    Text::from(
        stripped
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect::<Vec<_>>(),
    )
}

fn key_chip_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Black)
}

fn banner_row_push_chip(
    spans: &mut Vec<Span<'static>>,
    regions: &mut Vec<(CellRect, BannerHotkey)>,
    cx: &mut u16,
    y: u16,
    face: &str,
    hk: BannerHotkey,
) {
    let cell = format!(" {face} ");
    let w = cell.chars().count() as u16;
    spans.push(Span::styled(cell, key_chip_style()));
    regions.push((CellRect { x: *cx, y, w, h: 1 }, hk));
    *cx = cx.saturating_add(w);
}

fn banner_row_push_label(spans: &mut Vec<Span<'static>>, cx: &mut u16, s: &str, dim: Style) {
    *cx = cx.saturating_add(s.chars().count() as u16);
    spans.push(Span::styled(s.to_string(), dim));
}

/// 在 banner 底部两行绘制快捷键提示：第一行灰字；第二行为黑底白字可点击热键并写入 `ui.banner_hotkey_regions`。
fn render_banner_shortcut_rows(frame: &mut Frame, area: Rect, ui: &mut UiState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let row0 = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1.min(area.height),
    };
    let row1 = Rect {
        x: area.x,
        y: area.y.saturating_add(row0.height),
        width: area.width,
        height: area.height.saturating_sub(row0.height),
    };

    let help0 = "{Space} Pause  {↑/↓} Track  {←/→} Seek  {+/-} Vol  {[/]} Bal  {Q} Quit";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(help0, dim))).wrap(Wrap { trim: true }),
        row0,
    );

    if row1.height == 0 {
        return;
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cx = row1.x;
    let y = row1.y;
    let regs = &mut ui.banner_hotkey_regions;

    banner_row_push_chip(&mut spans, regs, &mut cx, y, "E", BannerHotkey::Eq);
    banner_row_push_label(&mut spans, &mut cx, " EQ  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "X", BannerHotkey::Fx);
    banner_row_push_label(&mut spans, &mut cx, " FX  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "C", BannerHotkey::Crossfeed);
    banner_row_push_label(&mut spans, &mut cx, " Crossfeed  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "F", BannerHotkey::Fader);
    banner_row_push_label(&mut spans, &mut cx, " Fader  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "V", BannerHotkey::VizMode);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "B", BannerHotkey::VizStyle);
    banner_row_push_label(&mut spans, &mut cx, " Viz  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "I", BannerHotkey::Info);
    banner_row_push_label(&mut spans, &mut cx, " Info  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "L", BannerHotkey::List);
    banner_row_push_label(&mut spans, &mut cx, " List  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "Y", BannerHotkey::Lyrics);
    banner_row_push_label(&mut spans, &mut cx, " Lyrics  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "O", BannerHotkey::Open);
    banner_row_push_label(&mut spans, &mut cx, " Open  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "P", BannerHotkey::Pick);
    banner_row_push_label(&mut spans, &mut cx, " Pick  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "G", BannerHotkey::Shuffle);
    banner_row_push_label(&mut spans, &mut cx, " Shuffle  ", dim);
    banner_row_push_chip(&mut spans, regs, &mut cx, y, "T", BannerHotkey::LoopToggle);
    banner_row_push_label(&mut spans, &mut cx, " Loop", dim);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
        row1,
    );
}

/// 绘制主界面一帧（含 banner）。终端尺寸变化时由调用方先 [`Terminal::clear`]。
pub fn draw_player<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &[PathBuf],
    name: &str,
    track_info: &str,
    ext: &str,
    eq_preset: &EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
) -> Result<(), B::Error> {
    let flash = ui.take_status();
    terminal
        .draw(|f| {
            draw_player_frame(
                f,
                state,
                ui,
                playlist,
                name,
                track_info,
                ext,
                eq_preset,
                fx_name,
                cf_name,
                stats,
                flash.as_deref(),
            );
        })
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn draw_player_frame(
    frame: &mut Frame,
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &[PathBuf],
    name: &str,
    track_info: &str,
    ext: &str,
    eq_preset: &EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    status_flash: Option<&str>,
) {
    let area = frame.area();
    let term_w = area.width as usize;

    let banner_h = (ui.banner_lines as u16).min(area.height).max(1);
    let [banner_rect, body_rect] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(banner_h), Constraint::Min(0)])
        .areas(area);

    let bblock = Block::default().borders(Borders::BOTTOM);
    let binner = bblock.inner(banner_rect);
    frame.render_widget(bblock, banner_rect);

    ui.banner_hotkey_regions.clear();

    let inner_h = binner.height;
    let help_h: u16 = 2.min(inner_h);
    let para_h = inner_h.saturating_sub(help_h).max(1);

    let banner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(para_h), Constraint::Length(help_h)])
        .split(binner);

    frame.render_widget(
        Paragraph::new(banner_plain(ui))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left),
        banner_chunks[0],
    );
    if help_h > 0 {
        render_banner_shortcut_rows(frame, banner_chunks[1], ui);
    }

    let viz_mode = state.viz_mode();
    let viz_style = state.viz_style();
    let eq_name = &eq_preset.name;
    let eq_curve = eq::render_eq_curve(eq_preset);
    let eq_line = !eq_curve.is_empty();
    let header_lines = 2usize + if eq_line { 1 } else { 0 };
    let viz_only_h = if viz_mode == VizMode::None {
        0
    } else {
        get_viz_line_count(viz_mode, viz_style) + 1
    };

    let track = state.current_track.load(Ordering::Relaxed) + 1;
    let total = state.total_tracks.load(Ordering::Relaxed);
    let icon = if state.is_paused() { "⏸" } else { "▶" };
    let icon_style = if state.is_paused() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };

    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());
    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else {
        0.0
    };
    let bar_w = 20usize.min(term_w.saturating_sub(24).max(8));
    let bar_str = progress_bar_chars(progress, bar_w, viz_style);

    let overhead = format!("[{track}/{total}] ♪  ").len() + track_info.len() + 1;
    let max_name = term_w.saturating_sub(overhead).min(35).max(4);
    let display_name = if name.chars().count() > max_name {
        let take = name.chars().take(max_name.saturating_sub(1)).collect::<String>();
        format!("{take}…")
    } else {
        name.to_string()
    };

    let buf = state.buffer_level.load(Ordering::Relaxed);
    let raw_buf_pct = buf as f32 / RING_BUFFER_SIZE as f32 * 100.0;
    stats.update_buf(raw_buf_pct);
    let buf_pct = stats.smoothed_buf_pct as u32;

    let vol = state.volume.load(Ordering::Relaxed);
    let fader = if state.is_pre_fader() { "pre" } else { "post" };
    let eq_display = if eq_name == "Flat" {
        String::new()
    } else {
        format!(" eq:{eq_name}")
    };
    let fx_display = if fx_name == "None" {
        String::new()
    } else {
        format!(" fx:{fx_name}")
    };
    let cf_display = if cf_name != "Off" {
        format!(" cf:{cf_name}")
    } else {
        String::new()
    };
    let clip_style = if state.is_clipping() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let bal = state.balance_value();
    let bal_display = if bal != 0 {
        if bal < 0 {
            format!(" BAL:L{}%", -bal)
        } else {
            format!(" BAL:R{}%", bal)
        }
    } else {
        String::new()
    };
    let next_viz = match viz_mode.next() {
        VizMode::None => "Off",
        VizMode::VuMeter => "VU",
        VizMode::SpectrumHorizontal => "SpecH",
        VizMode::SpectrumVertical => "SpecV",
    };
    let next_style = match viz_style {
        VizStyle::Dots => "Bars",
        VizStyle::Bars => "Dots",
    };
    let stats_display = if state.show_stats() {
        format!(" cpu:{:.1}% mem:{:.0}M", stats.cpu_usage, stats.memory_mb)
    } else {
        String::new()
    };
    let play_mode = if ui.shuffle { "shuf" } else { "seq" };
    let loop_mode = if ui.repeat { "loop" } else { "once" };

    let line1 = Line::from(vec![
        Span::styled(
            format!("[{track}/{total}] "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("♪", ext_note_style(ext)),
        Span::raw(" "),
        Span::styled(
            display_name,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(track_info, Style::default().fg(Color::DarkGray)),
    ]);

    let line2 = Line::from(vec![
        Span::raw("  "),
        Span::styled(icon, icon_style),
        Span::styled(
            format!(
                " [{cur}/{tot}] {bar_str} {play_mode}/{loop_mode} vol:{vol}%{eq_display}{fx_display}{cf_display} "
            ),
            Style::default(),
        ),
        Span::styled("●", clip_style),
        Span::styled(
            format!("{bal_display} {fader} buf:{buf_pct}%{stats_display} {{V}}:{next_viz} {{B}}:{next_style}"),
            Style::default(),
        ),
    ]);

    match ui.view_mode {
        ViewMode::Playlist => {
            let body_h = body_rect.height as usize;
            let footer_h = 1usize;
            let sep_allow = 1usize;
            let visible_rows = body_h
                .saturating_sub(header_lines + viz_only_h + footer_h + sep_allow)
                .max(1);
            sync_playlist_scroll(ui, playlist, visible_rows);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_lines as u16),
                    Constraint::Length(viz_only_h as u16),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(body_rect);

            render_header_block(frame, chunks[0], &line1, &line2, eq_line, &eq_curve);
            render_viz_block(frame, chunks[1], state, viz_mode, viz_style);
            let list_rows = chunks[2].height as usize;
            render_playlist_list(frame, chunks[2], ui, playlist, list_rows, term_w);
            render_playlist_footer_line(frame, chunks[3], ui, status_flash);
        }
        ViewMode::Lyrics => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_lines as u16),
                    Constraint::Length(viz_only_h as u16),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(body_rect);

            render_header_block(frame, chunks[0], &line1, &line2, eq_line, &eq_curve);
            render_viz_block(frame, chunks[1], state, viz_mode, viz_style);
            let lyric_rows = chunks[2].height as usize;
            render_lyrics_lines(frame, chunks[2], state, ui, lyric_rows);
            render_lyrics_footer_line(frame, chunks[3], ui);
        }
        ViewMode::Player => {
            let status_h = if status_flash.is_some() { 1u16 } else { 0u16 };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_lines as u16),
                    Constraint::Length(viz_only_h as u16),
                    Constraint::Min(0),
                    Constraint::Length(status_h),
                ])
                .split(body_rect);

            render_header_block(frame, chunks[0], &line1, &line2, eq_line, &eq_curve);
            render_viz_block(frame, chunks[1], state, viz_mode, viz_style);
            if let Some(msg) = status_flash {
                let area = chunks[3];
                if area.height > 0 {
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(msg, Style::default().fg(Color::Green)),
                        ]))
                        .wrap(Wrap { trim: true }),
                        area,
                    );
                }
            }
        }
    }
}

/// 与旧 `print_status` 一致：保证列表光标在可视窗口内并钳制 `scroll_offset`。
fn sync_playlist_scroll(ui: &mut UiState, playlist: &[PathBuf], visible_rows: usize) {
    let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
    let items: Vec<usize> = if search_active && ui.filtered_indices.is_empty() {
        vec![]
    } else if !search_active && ui.filtered_indices.is_empty() {
        (0..playlist.len()).collect()
    } else {
        ui.filtered_indices.clone()
    };

    let scroll_margin = 4.min(visible_rows / 2).max(0);

    if ui.cursor >= ui.scroll_offset + visible_rows.saturating_sub(scroll_margin) {
        ui.scroll_offset = ui
            .cursor
            .saturating_sub(visible_rows.saturating_sub(scroll_margin + 1));
    }
    if ui.cursor < ui.scroll_offset + scroll_margin {
        ui.scroll_offset = ui.cursor.saturating_sub(scroll_margin);
    }

    let max_offset = items.len().saturating_sub(visible_rows);
    ui.scroll_offset = ui.scroll_offset.min(max_offset);
}

fn render_header_block(
    frame: &mut Frame,
    area: Rect,
    line1: &Line,
    line2: &Line,
    eq_line: bool,
    eq_curve: &str,
) {
    if area.height == 0 {
        return;
    }
    let inner = area;
    let mut row = 0u16;
    if inner.height > row {
        frame.render_widget(
            Paragraph::new(line1.clone()).wrap(Wrap { trim: true }),
            row_rect(inner, row),
        );
        row += 1;
    }
    if inner.height > row {
        frame.render_widget(
            Paragraph::new(line2.clone()).wrap(Wrap { trim: true }),
            row_rect(inner, row),
        );
        row += 1;
    }
    if eq_line && inner.height > row {
        frame.render_widget(
            Paragraph::new(strip_ansi(eq_curve)).wrap(Wrap { trim: true }),
            row_rect(inner, row),
        );
    }
}

fn row_rect(r: Rect, row: u16) -> Rect {
    Rect {
        x: r.x,
        y: r.y.saturating_add(row),
        width: r.width,
        height: 1.min(r.height.saturating_sub(row)),
    }
}

fn render_viz_block(
    frame: &mut Frame,
    area: Rect,
    state: &PlayerState,
    viz_mode: VizMode,
    viz_style: VizStyle,
) {
    if area.height == 0 || viz_mode == VizMode::None {
        return;
    }
    let sep = Block::default().borders(Borders::TOP);
    let inner = sep.inner(area);
    frame.render_widget(sep, area);
    let lines: Vec<String> = match viz_mode {
        VizMode::None => vec![],
        VizMode::VuMeter => render_vu_meter(state, viz_style),
        VizMode::SpectrumHorizontal => render_spectrum_horizontal(state, viz_style),
        VizMode::SpectrumVertical => render_spectrum_vertical(state, viz_style),
    };
    let mut row = 0u16;
    for s in lines {
        if row >= inner.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(strip_ansi(&s)).wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y + row,
                width: inner.width,
                height: 1,
            },
        );
        row += 1;
    }
}

fn render_playlist_list(
    frame: &mut Frame,
    area: Rect,
    ui: &UiState,
    playlist: &[PathBuf],
    visible_rows: usize,
    term_w: usize,
) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled("播放列表", Style::default().fg(Color::DarkGray)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
    let items: Vec<usize> = if search_active && ui.filtered_indices.is_empty() {
        vec![]
    } else if !search_active && ui.filtered_indices.is_empty() {
        (0..playlist.len()).collect()
    } else {
        ui.filtered_indices.clone()
    };

    let rows = visible_rows.max(1).min(inner.height as usize);
    let display_items: Vec<usize> = items
        .iter()
        .skip(ui.scroll_offset)
        .take(rows)
        .copied()
        .collect();

    if items.is_empty() && search_active {
        frame.render_widget(
            Paragraph::new("(无匹配项)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let list_items: Vec<ListItem<'static>> = display_items
        .iter()
        .enumerate()
        .map(|(row, &track_idx)| {
            let list_pos = ui.scroll_offset + row;
            let is_playing = track_idx == ui.current;
            let is_cursor = list_pos == ui.cursor;
            let fname = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);
            let marker = if is_playing { "▶" } else { " " };
            let num = format!("{:>4}", track_idx + 1);
            let line = if is_cursor && is_playing {
                Line::from(vec![
                    Span::raw(format!(" {marker} ")),
                    Span::styled(
                        format!("{num}  {fname}"),
                        Style::default()
                            .fg(Color::Green)
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else if is_cursor {
                Line::from(vec![
                    Span::raw(format!(" {marker} ")),
                    Span::styled(
                        format!("{num}  {fname}"),
                        Style::default().bg(Color::DarkGray),
                    ),
                ])
            } else if is_playing {
                Line::from(vec![
                    Span::raw(format!(" {marker} ")),
                    Span::styled(format!("{num}  {fname}"), Style::default().fg(Color::Green)),
                ])
            } else {
                Line::from(vec![
                    Span::raw(format!(" {marker} ")),
                    Span::styled(num, Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("  {fname}")),
                ])
            };
            let _ = term_w;
            ListItem::new(line)
        })
        .collect();

    let mut list_state = ListState::default();
    let rel = ui.cursor.saturating_sub(ui.scroll_offset);
    if rel < list_items.len() {
        list_state.select(Some(rel));
    }

    let list = List::new(list_items).highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, inner, &mut list_state);
}

fn render_playlist_footer_line(frame: &mut Frame, area: Rect, ui: &UiState, status_flash: Option<&str>) {
    let footer = match &ui.input_mode {
        InputMode::Search(query) => format!("/ {query}_"),
        InputMode::SavePlaylist(name) => format!("Save playlist as: {name}_"),
        InputMode::Normal => {
            if let Some(msg) = status_flash {
                msg.to_string()
            } else {
                "[L] close  [↑↓] scroll  [Enter] play  [/] search  [D] remove  [S] save".to_string()
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_lyrics_lines(
    frame: &mut Frame,
    area: Rect,
    state: &PlayerState,
    ui: &mut UiState,
    visible_rows: usize,
) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled("歌词", Style::default().fg(Color::DarkGray)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = visible_rows.max(1).min(inner.height as usize);
    if let Some(ref lyrics) = ui.lyrics {
        let total_lines = lyrics.line_count();
        let adjusted_time = state.time_secs() + ui.lyrics_offset;
        let current_line = lyrics.current_line(adjusted_time);

        if lyrics.is_synced() && ui.lyrics_auto_scroll {
            if let Some(cur) = current_line {
                let half = rows / 2;
                ui.lyrics_scroll = cur.saturating_sub(half);
            }
        }

        let mut scroll = ui.lyrics_scroll;
        if total_lines > rows {
            scroll = scroll.min(total_lines.saturating_sub(rows));
        } else {
            scroll = 0;
        }
        ui.lyrics_scroll = scroll;

        for row in 0..rows {
            let line_idx = scroll + row;
            let line = if line_idx < total_lines {
                let text = lyrics.line_text(line_idx);
                let is_current = current_line == Some(line_idx);
                if is_current {
                    Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(Color::DarkGray),
                    ))
                }
            } else {
                Line::default()
            };
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    x: inner.x,
                    y: inner.y + row as u16,
                    width: inner.width,
                    height: 1,
                },
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new("(无歌词)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn render_lyrics_footer_line(frame: &mut Frame, area: Rect, ui: &UiState) {
    let is_synced = ui.lyrics.as_ref().map(|l| l.is_synced()).unwrap_or(false);
    let offset_display = if is_synced && ui.lyrics_offset != 0.0 {
        format!("  offset:{:+.1}s", ui.lyrics_offset)
    } else {
        String::new()
    };
    let sync_hint = if is_synced { "  [A/D] sync" } else { "" };
    let t = format!("[Y] close  [W/S] scroll{sync_hint}{offset_display}");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            t,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}
