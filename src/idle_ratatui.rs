//! 「列表已播完」空闲态的 Ratatui 绘制层。
//!
//! **职责**：在 `session_idle` 时用分区布局（标题 / 说明 / 状态条）呈现界面，与主界面共用 Ratatui，减少残影与对齐问题。
//! **设计**：与主界面共用同一 [`Terminal`]（由 `main` 创建）；退出空闲时置 `ui.terminal_resized` 以便下一帧整屏重绘主界面。
//! **输入**：复用 [`crate::ui::poll_input`]，保证 O / P / G / T / Q 等与主界面一致。
//! **注意**：工程将 Ratatui 设为 0.30 + `crossterm_0_29`，与 `Cargo.toml` 中的 `crossterm = "0.29"` 对齐；raw 模式与事件仍由 `main` 统一管理，此处只负责帧绘制。

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;

use crate::crossfeed::CrossfeedPreset;
use crate::effects::EffectsPreset;
use crate::eq::EqPreset;
use crate::resume::{build_resume_state, save_state};
use crate::state::{PlayerState, UiState};
use crate::ui::poll_input;

/// 绘制一帧空闲界面（标题区、操作说明、当前预设摘要）。
fn draw_idle_frame(
    frame: &mut Frame,
    state: &PlayerState,
    ui: &mut UiState,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
) {
    ui.banner_hotkey_regions.clear();
    ui.transport_click_regions.clear();
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(4),
        ])
        .split(area);

    let eq = &eq_presets[state.eq_index()];
    let fx = &fx_presets[state.effects_index()].name;
    let cf = &cf_presets[state.crossfeed_index()].name;
    let play_mode = if ui.shuffle { "shuf" } else { "seq" };
    let loop_mode = if ui.repeat { "loop" } else { "once" };

    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(
            Line::from(vec![
                Span::styled("Keet ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled("(已播完)", Style::default().fg(Color::Yellow)),
            ]),
        );
    let header_area = header_block.inner(chunks[0]);
    frame.render_widget(header_block, chunks[0]);
    frame.render_widget(
        Paragraph::new("本列表已播放完毕，可添加音乐或切换播放模式。")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        header_area,
    );

    let body_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("操作", Style::default().fg(Color::DarkGray)));
    let body_area = body_block.inner(chunks[1]);
    frame.render_widget(body_block, chunks[1]);
    frame.render_widget(
        Paragraph::new("按 O 输入路径 · P 选择目录 · G 随机 · T 列表循环 · Q 退出")
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::Gray)),
        body_area,
    );

    let foot_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("状态", Style::default().fg(Color::DarkGray)));
    let foot_area = foot_block.inner(chunks[2]);
    frame.render_widget(foot_block, chunks[2]);
    let foot_line = format!("{play_mode}/{loop_mode}  ·  eq:{}  ·  fx:{}  ·  cf:{}", eq.name, fx, cf);
    frame.render_widget(
        Paragraph::new(foot_line).wrap(Wrap { trim: true }),
        foot_area,
    );
}

/// 进入「已播完」空闲循环：绘制 Ratatui 界面并轮询输入，直到恢复播放或应用退出。
///
/// # 参数
/// - `device_arg`：输出设备名（写入断点快照）。
///
/// # 返回
/// - `Ok(true)`：用户通过 `poll_input` 请求退出整个应用。
/// - `Ok(false)`：已离开空闲态（继续播放或主循环其它分支），调用方应全屏重绘（`terminal_resized` 已置位）。
pub fn run_session_idle<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    eq_presets: &[EqPreset],
    fx_presets: &[EffectsPreset],
    cf_presets: &[CrossfeedPreset],
    device_arg: &Option<String>,
) -> Result<bool, B::Error> {
    loop {
        if state.should_quit() || !ui.session_idle {
            break;
        }

        terminal.draw(|f| {
            draw_idle_frame(f, state, ui, eq_presets, fx_presets, cf_presets);
        })?;

        if poll_input(state, ui, playlist) {
            return Ok(true);
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

        if state.should_quit() || !ui.session_idle {
            break;
        }

        thread::sleep(Duration::from_millis(50));
    }

    ui.terminal_resized = true;
    Ok(false)
}
