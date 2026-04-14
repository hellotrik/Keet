use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU8, AtomicU32, AtomicU64, AtomicI32, AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;
use std::path::PathBuf;

pub const RING_BUFFER_SIZE: usize = 48000 * 2 * 4; // ~4 sec stereo
pub const VIZ_BUFFER_SIZE: usize = 8192; // Small buffer for viz tap from audio callback

// Visualization constants
pub const FFT_SIZE: usize = 4096;
pub const SPECTRUM_BANDS: usize = 31;
pub const VIZ_DECAY: f32 = 0.70; // Smoothing factor for spectrum (lower = more responsive)

pub const GRAVITY: f32 = 0.04;    // Constant fall speed for main bars
pub const DOT_GRAVITY: f32 = 0.025; // Slower fall for the peak dots
pub const ATTACK: f32 = 0.7;       // Snappiness of the rise
pub const HOLD_TIME: u8 = 10;      // Frames for the dot to "hang"

// ANSI color codes
pub const C_RESET: &str = "\x1B[0m";
pub const C_DIM: &str = "\x1B[2m";
pub const C_CYAN: &str = "\x1B[36m";
pub const C_GREEN: &str = "\x1B[32m";
pub const C_YELLOW: &str = "\x1B[33m";
pub const C_MAGENTA: &str = "\x1B[35m";
pub const C_RED: &str = "\x1B[31m";

// Visualization style
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VizStyle {
    Bars = 0,
    Dots = 1,
}

impl VizStyle {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => VizStyle::Dots,
            _ => VizStyle::Bars,
        }
    }

}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RgMode {
    Track = 0,
    Album = 1,
    Off = 2,
}

impl RgMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => RgMode::Album,
            2 => RgMode::Off,
            _ => RgMode::Track,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RgMode::Track => "Track",
            RgMode::Album => "Album",
            RgMode::Off => "Off",
        }
    }
}

// Visualization modes
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VizMode {
    None = 0,
    VuMeter = 1,
    SpectrumHorizontal = 2,
    SpectrumVertical = 3,
}

impl VizMode {
    pub fn next(self) -> Self {
        match self {
            VizMode::None => VizMode::VuMeter,
            VizMode::VuMeter => VizMode::SpectrumHorizontal,
            VizMode::SpectrumHorizontal => VizMode::SpectrumVertical,
            VizMode::SpectrumVertical => VizMode::None,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => VizMode::VuMeter,
            2 => VizMode::SpectrumHorizontal,
            3 => VizMode::SpectrumVertical,
            _ => VizMode::None,
        }
    }

}

pub struct PlayerState {
    // Control flags
    pub(crate) paused: AtomicBool,
    pub(crate) quit: AtomicBool,
    pub(crate) skip_next: AtomicBool,
    pub(crate) skip_prev: AtomicBool,
    pub(crate) seek_request: AtomicI64,
    pub(crate) jump_to_track: AtomicI64,

    // Track info
    pub(crate) current_track: AtomicUsize,
    pub(crate) total_tracks: AtomicUsize,
    pub(crate) sample_rate: AtomicU64,      // Source file sample rate
    pub(crate) output_rate: AtomicU64,      // Output stream sample rate
    pub(crate) total_samples: AtomicU64,    // Total samples in source file
    pub(crate) samples_played: AtomicU64,   // Samples played (at output rate)
    pub(crate) channels: AtomicUsize,
    pub(crate) bits_per_sample: AtomicUsize,

    // Producer status
    pub(crate) producer_done: AtomicBool,
    pub(crate) track_info_ready: AtomicBool,

    // Buffer level (updated by producer, read by UI)
    pub(crate) buffer_level: AtomicUsize,

    // Seek flush: number of samples consumer should discard (for instant seek)
    pub(crate) discard_samples: AtomicU64,

    // Signal consumer to reset its local counter (for seek)
    pub(crate) reset_consumer_counter: AtomicBool,

    // Visualization state
    pub(crate) viz_mode: AtomicU8,
    pub(crate) peak_left: AtomicU32,
    pub(crate) peak_right: AtomicU32,
    pub(crate) spectrum: [AtomicU32; SPECTRUM_BANDS],   // L channel (or mono for vertical)
    pub(crate) spectrum_r: [AtomicU32; SPECTRUM_BANDS], // R channel
    pub(crate) peak_dots: [AtomicU32; SPECTRUM_BANDS],

    pub(crate) vu_peak_dot_l: AtomicU32,
    pub(crate) vu_peak_dot_r: AtomicU32,

    // Volume (0-150, stored as percentage, 100 = unity gain)
    pub(crate) volume: AtomicU32,

    // EQ preset index and count
    pub(crate) eq_preset_index: AtomicUsize,
    pub(crate) eq_preset_count: AtomicUsize,
    pub(crate) eq_changed: AtomicBool,

    // Effects preset index and count
    pub(crate) effects_preset_index: AtomicUsize,
    pub(crate) effects_preset_count: AtomicUsize,
    pub(crate) effects_changed: AtomicBool,

    // Pre/post-fader metering (false = post-fader, true = pre-fader)
    pub(crate) pre_fader: AtomicBool,

    // Show CPU/memory stats in status line
    pub(crate) show_stats: AtomicBool,

    // Crossfade duration in seconds (0 = disabled)
    pub(crate) crossfade_secs: AtomicU32,

    // Visualization style (bars vs dots)
    pub(crate) viz_style: AtomicU8,

    // Decode error from producer thread (None = no error)
    pub(crate) decode_error: Mutex<Option<String>>,

    // Track transition signaling (gapless playback)
    pub(crate) track_transition_count: AtomicUsize,
    pub(crate) producer_track_index: AtomicUsize,

    // ReplayGain mode
    pub(crate) rg_mode: AtomicU8,

    // Clipping indicator
    pub(crate) clipping: AtomicBool,

    // Crossfeed preset index and count
    pub(crate) crossfeed_preset_index: AtomicUsize,
    pub(crate) crossfeed_preset_count: AtomicUsize,
    pub(crate) crossfeed_changed: AtomicBool,

    // Stereo balance (-100 to +100, 0 = center)
    pub(crate) balance: AtomicI32,

    // Exclusive mode
    pub(crate) exclusive: AtomicBool,
    pub(crate) rate_change_needed: AtomicBool,
    pub(crate) next_track_rate: AtomicU32,

    // Stream error (device disconnected etc.)
    pub(crate) stream_error: AtomicBool,

    /// 外部 mpv 播放时：为真则 [`Self::time_secs`] / [`Self::total_secs`] 来自下方微秒字段（与音频样本计数互斥）。
    pub(crate) external_playback_active: AtomicBool,
    pub(crate) external_time_us: AtomicU64,
    pub(crate) external_duration_us: AtomicU64,

    // --- Video view controls (mpv IPC session) ---
    // These are requests coming from the input layer; the mpv control loop consumes them.
    pub(crate) video_zoom_steps: AtomicI32,
    pub(crate) video_pan_x_steps: AtomicI32,
    pub(crate) video_pan_y_steps: AtomicI32,
    pub(crate) video_step_adjust: AtomicI32,
    pub(crate) video_view_reset: AtomicBool,
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            skip_next: AtomicBool::new(false),
            skip_prev: AtomicBool::new(false),
            seek_request: AtomicI64::new(0),
            jump_to_track: AtomicI64::new(-1),
            current_track: AtomicUsize::new(0),
            total_tracks: AtomicUsize::new(0),
            sample_rate: AtomicU64::new(44100),
            output_rate: AtomicU64::new(44100),
            total_samples: AtomicU64::new(0),
            samples_played: AtomicU64::new(0),
            channels: AtomicUsize::new(2),
            bits_per_sample: AtomicUsize::new(16),
            producer_done: AtomicBool::new(false),
            track_info_ready: AtomicBool::new(false),
            buffer_level: AtomicUsize::new(0),
            discard_samples: AtomicU64::new(0),
            reset_consumer_counter: AtomicBool::new(false),
            viz_mode: AtomicU8::new(VizMode::None as u8),
            peak_left: AtomicU32::new(0),
            peak_right: AtomicU32::new(0),
            spectrum: std::array::from_fn(|_| AtomicU32::new(0)),
            spectrum_r: std::array::from_fn(|_| AtomicU32::new(0)),
            peak_dots: std::array::from_fn(|_| AtomicU32::new(0)),
            vu_peak_dot_l: AtomicU32::new(0),
            vu_peak_dot_r: AtomicU32::new(0),
            volume: AtomicU32::new(100),
            eq_preset_index: AtomicUsize::new(0),
            eq_preset_count: AtomicUsize::new(0),
            eq_changed: AtomicBool::new(false),
            effects_preset_index: AtomicUsize::new(0),
            effects_preset_count: AtomicUsize::new(0),
            effects_changed: AtomicBool::new(false),
            pre_fader: AtomicBool::new(false),
            show_stats: AtomicBool::new(false),
            crossfade_secs: AtomicU32::new(0),
            viz_style: AtomicU8::new(VizStyle::Dots as u8),
            decode_error: Mutex::new(None),
            track_transition_count: AtomicUsize::new(0),
            producer_track_index: AtomicUsize::new(0),
            rg_mode: AtomicU8::new(RgMode::Track as u8),
            clipping: AtomicBool::new(false),
            crossfeed_preset_index: AtomicUsize::new(0),
            crossfeed_preset_count: AtomicUsize::new(0),
            crossfeed_changed: AtomicBool::new(false),
            balance: AtomicI32::new(0),
            exclusive: AtomicBool::new(false),
            rate_change_needed: AtomicBool::new(false),
            next_track_rate: AtomicU32::new(0),
            stream_error: AtomicBool::new(false),
            external_playback_active: AtomicBool::new(false),
            external_time_us: AtomicU64::new(0),
            external_duration_us: AtomicU64::new(0),
            video_zoom_steps: AtomicI32::new(0),
            video_pan_x_steps: AtomicI32::new(0),
            video_pan_y_steps: AtomicI32::new(0),
            video_step_adjust: AtomicI32::new(0),
            video_view_reset: AtomicBool::new(false),
        }
    }

    pub fn toggle_pause(&self) { self.paused.fetch_xor(true, Ordering::Relaxed); }

    /// 将暂停状态设为绝对值（用于 mpv IPC 回传或与终端操作对齐）。
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::Relaxed) }
    pub fn quit(&self) { self.quit.store(true, Ordering::Relaxed); }
    pub fn should_quit(&self) -> bool { self.quit.load(Ordering::Relaxed) }
    pub fn next(&self) { self.skip_next.store(true, Ordering::Relaxed); }
    pub fn prev(&self) { self.skip_prev.store(true, Ordering::Relaxed); }
    pub fn jump_to(&self, index: usize) {
        self.jump_to_track.store(index as i64, Ordering::Relaxed);
    }

    pub fn take_jump(&self) -> Option<usize> {
        let val = self.jump_to_track.swap(-1, Ordering::Relaxed);
        if val >= 0 { Some(val as usize) } else { None }
    }
    pub fn take_skip_next(&self) -> bool { self.skip_next.swap(false, Ordering::Relaxed) }
    pub fn take_skip_prev(&self) -> bool { self.skip_prev.swap(false, Ordering::Relaxed) }
    pub fn seek(&self, secs: i64) { self.seek_request.store(secs, Ordering::Relaxed); }
    pub fn take_seek(&self) -> i64 { self.seek_request.swap(0, Ordering::Relaxed) }

    pub fn volume_up(&self) {
        let cur = self.volume.load(Ordering::Relaxed);
        self.volume.store((cur + 5).min(150), Ordering::Relaxed);
    }
    pub fn volume_down(&self) {
        let cur = self.volume.load(Ordering::Relaxed);
        self.volume.store(cur.saturating_sub(5), Ordering::Relaxed);
    }
    pub fn volume_gain(&self) -> f32 {
        self.volume.load(Ordering::Relaxed) as f32 / 100.0
    }

    pub fn time_secs(&self) -> f64 {
        if self.external_playback_active.load(Ordering::Relaxed) {
            self.external_time_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
        } else {
            let s = self.samples_played.load(Ordering::Relaxed) as f64;
            let r = self.output_rate.load(Ordering::Relaxed) as f64;
            if r > 0.0 { s / r } else { 0.0 }
        }
    }

    pub fn total_secs(&self) -> f64 {
        if self.external_playback_active.load(Ordering::Relaxed) {
            self.external_duration_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
        } else {
            let s = self.total_samples.load(Ordering::Relaxed) as f64;
            let r = self.sample_rate.load(Ordering::Relaxed) as f64;
            if r > 0.0 { s / r } else { 0.0 }
        }
    }

    /// 启用外部播放时间轴（mpv）；与 [`Self::clear_external_playback`] 成对使用。
    pub fn set_external_playback_active(&self, active: bool) {
        self.external_playback_active.store(active, Ordering::Relaxed);
    }

    pub fn set_external_time_duration(&self, pos_secs: f64, dur_secs: f64) {
        self.external_time_us.store((pos_secs * 1_000_000.0) as u64, Ordering::Relaxed);
        self.external_duration_us.store((dur_secs * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    pub fn clear_external_playback(&self) {
        self.external_playback_active.store(false, Ordering::Relaxed);
        self.external_time_us.store(0, Ordering::Relaxed);
        self.external_duration_us.store(0, Ordering::Relaxed);
    }

    pub fn request_video_zoom_steps(&self, delta_steps: i32) {
        if delta_steps != 0 {
            let _ = self.video_zoom_steps.fetch_add(delta_steps, Ordering::Relaxed);
        }
    }

    pub fn take_video_zoom_steps(&self) -> i32 {
        self.video_zoom_steps.swap(0, Ordering::Relaxed)
    }

    pub fn request_video_pan_steps(&self, dx_steps: i32, dy_steps: i32) {
        if dx_steps != 0 {
            let _ = self.video_pan_x_steps.fetch_add(dx_steps, Ordering::Relaxed);
        }
        if dy_steps != 0 {
            let _ = self.video_pan_y_steps.fetch_add(dy_steps, Ordering::Relaxed);
        }
    }

    pub fn take_video_pan_steps(&self) -> (i32, i32) {
        let dx = self.video_pan_x_steps.swap(0, Ordering::Relaxed);
        let dy = self.video_pan_y_steps.swap(0, Ordering::Relaxed);
        (dx, dy)
    }

    pub fn request_video_step_adjust(&self, delta: i32) {
        if delta != 0 {
            let _ = self.video_step_adjust.fetch_add(delta, Ordering::Relaxed);
        }
    }

    pub fn take_video_step_adjust(&self) -> i32 {
        self.video_step_adjust.swap(0, Ordering::Relaxed)
    }

    pub fn request_video_view_reset(&self) {
        self.video_view_reset.store(true, Ordering::Relaxed);
    }

    pub fn take_video_view_reset(&self) -> bool {
        self.video_view_reset.swap(false, Ordering::Relaxed)
    }

    pub fn viz_mode(&self) -> VizMode {
        VizMode::from_u8(self.viz_mode.load(Ordering::Relaxed))
    }

    pub fn cycle_viz_mode(&self) {
        let current = self.viz_mode();
        self.viz_mode.store(current.next() as u8, Ordering::Relaxed);
    }

    pub fn cycle_eq(&self) {
        let count = self.eq_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.eq_preset_index.load(Ordering::Relaxed);
        self.eq_preset_index.store((cur + 1) % count, Ordering::Relaxed);
        self.eq_changed.store(true, Ordering::Relaxed);
    }

    pub fn eq_index(&self) -> usize {
        self.eq_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_eq_changed(&self) -> bool {
        self.eq_changed.swap(false, Ordering::Relaxed)
    }

    pub fn cycle_effects(&self) {
        let count = self.effects_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.effects_preset_index.load(Ordering::Relaxed);
        self.effects_preset_index.store((cur + 1) % count, Ordering::Relaxed);
        self.effects_changed.store(true, Ordering::Relaxed);
    }

    pub fn effects_index(&self) -> usize {
        self.effects_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_effects_changed(&self) -> bool {
        self.effects_changed.swap(false, Ordering::Relaxed)
    }

    pub fn toggle_pre_fader(&self) {
        self.pre_fader.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn is_pre_fader(&self) -> bool {
        self.pre_fader.load(Ordering::Relaxed)
    }

    pub fn toggle_stats(&self) {
        self.show_stats.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn show_stats(&self) -> bool {
        self.show_stats.load(Ordering::Relaxed)
    }

    pub fn viz_style(&self) -> VizStyle {
        VizStyle::from_u8(self.viz_style.load(Ordering::Relaxed))
    }

    pub fn toggle_viz_style(&self) {
        let cur = self.viz_style.load(Ordering::Relaxed);
        self.viz_style.store(if cur == 0 { 1 } else { 0 }, Ordering::Relaxed);
    }

    pub fn signal_next_track(&self, index: usize) {
        self.producer_track_index.store(index, Ordering::Relaxed);
        self.track_transition_count.fetch_add(1, Ordering::Release);
    }

    pub fn rg_mode(&self) -> RgMode {
        RgMode::from_u8(self.rg_mode.load(Ordering::Relaxed))
    }

    pub fn is_clipping(&self) -> bool {
        self.clipping.swap(false, Ordering::Relaxed)
    }

    pub fn cycle_crossfeed(&self) {
        let count = self.crossfeed_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.crossfeed_preset_index.load(Ordering::Relaxed);
        self.crossfeed_preset_index.store((cur + 1) % count, Ordering::Relaxed);
        self.crossfeed_changed.store(true, Ordering::Relaxed);
    }

    pub fn crossfeed_index(&self) -> usize {
        self.crossfeed_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_crossfeed_changed(&self) -> bool {
        self.crossfeed_changed.swap(false, Ordering::Relaxed)
    }

    pub fn balance_left(&self) {
        let cur = self.balance.load(Ordering::Relaxed);
        self.balance.store((cur - 5).max(-100), Ordering::Relaxed);
    }

    pub fn balance_right(&self) {
        let cur = self.balance.load(Ordering::Relaxed);
        self.balance.store((cur + 5).min(100), Ordering::Relaxed);
    }

    pub fn balance_value(&self) -> i32 {
        self.balance.load(Ordering::Relaxed)
    }

    pub fn set_peaks(&self, left: f32, right: f32) {
        self.peak_left.store(left.to_bits(), Ordering::Relaxed);
        self.peak_right.store(right.to_bits(), Ordering::Relaxed);
    }

    pub fn get_peaks(&self) -> (f32, f32) {
        let left = f32::from_bits(self.peak_left.load(Ordering::Relaxed));
        let right = f32::from_bits(self.peak_right.load(Ordering::Relaxed));
        (left, right)
    }

    pub fn set_spectrum(&self, bands: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in bands.iter().enumerate() {
            self.spectrum[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.spectrum[i].load(Ordering::Relaxed)) )
    }

    pub fn set_spectrum_r(&self, bands: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in bands.iter().enumerate() {
            self.spectrum_r[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_spectrum_r(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.spectrum_r[i].load(Ordering::Relaxed)))
    }

    pub fn set_dots(&self, dots: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in dots.iter().enumerate() {
            self.peak_dots[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_dots(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.peak_dots[i].load(Ordering::Relaxed)))
    }

    pub fn set_vu_dots(&self, left: f32, right: f32) {
        self.vu_peak_dot_l.store(left.to_bits(), Ordering::Relaxed);
        self.vu_peak_dot_r.store(right.to_bits(), Ordering::Relaxed);
    }

    pub fn get_vu_dots(&self) -> (f32, f32) {
        let left = f32::from_bits(self.vu_peak_dot_l.load(Ordering::Relaxed));
        let right = f32::from_bits(self.vu_peak_dot_r.load(Ordering::Relaxed));
        (left, right)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Player,
    Playlist,
    Lyrics,
}

/// 顶部 banner 第二行可点击热键（与键盘 E/X/C/… 一一对应，用于鼠标命中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerHotkey {
    Eq,
    Fx,
    Crossfeed,
    Fader,
    VizMode,
    VizStyle,
    Info,
    List,
    Lyrics,
    Open,
    Pick,
    Shuffle,
    LoopToggle,
}

/// 终端单元格轴对齐矩形（与 crossterm / ratatui 坐标系一致，含 x/y 左上角）。
#[derive(Clone, Copy, Debug)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// 主界面进度行（▶/⏸ 与进度条）的鼠标动作，与键盘 Space / ← / → 语义对齐。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMouseAction {
    TogglePause,
    /// 与左方向键一致：快退约 10 秒。
    SeekBack,
    /// 与右方向键一致：快进约 10 秒。
    SeekForward,
}

#[derive(Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search(String),
    SavePlaylist(String),
}

pub struct UiState {
    pub view_mode: ViewMode,
    pub input_mode: InputMode,
    pub scroll_offset: usize,
    pub cursor: usize,
    pub filtered_indices: Vec<usize>,
    pub current: usize,
    pub source_paths: Vec<PathBuf>,
    pub status_message: Option<(String, Instant)>,
    pub metadata_cache: std::sync::Arc<crate::metadata::MetadataCache>,
    pub scan_handle: Option<JoinHandle<()>>,
    pub removed_paths: std::collections::HashSet<PathBuf>,
    pub banner_lines: usize,
    pub banner_text: String,
    /// 每帧由 [`crate::player_ratatui`] 重算：banner 底部热键的点击区域。
    pub banner_hotkey_regions: Vec<(CellRect, BannerHotkey)>,
    /// 每帧重算：播放图标与进度条左/右半块的命中区。
    pub transport_click_regions: Vec<(CellRect, TransportMouseAction)>,
    /// 列表视图：可见行对应的列表逻辑下标 `list_pos`（与 `scroll_offset + 行号` 一致，用于鼠标选曲）。
    pub playlist_mouse_rows: Vec<(CellRect, usize)>,
    pub playlist_dirty: bool,
    pub current_track_removed: bool,
    pub terminal_resized: bool,
    pub lyrics: Option<crate::lyrics::Lyrics>,
    pub lyrics_receiver: Option<std::sync::mpsc::Receiver<Option<crate::lyrics::Lyrics>>>,
    pub lyrics_scroll: usize,
    pub lyrics_auto_scroll: bool,
    pub lyrics_offset: f64, // seconds, positive = lyrics later, negative = lyrics earlier

    /// Random order (re-shuffle on each repeat cycle when enabled).
    pub shuffle: bool,
    /// Loop playlist when the end is reached (rescans directories each cycle).
    pub repeat: bool,
    /// Playlist finished with `repeat == false`: stay in TUI until user opens new source or quits.
    pub session_idle: bool,
    /// After toggling shuffle/repeat from keyboard, persist resume state on next main-loop tick.
    pub pending_resume_save: bool,
}

impl UiState {
    pub fn new(
        source_paths: Vec<PathBuf>,
        metadata_cache: std::sync::Arc<crate::metadata::MetadataCache>,
        shuffle: bool,
        repeat: bool,
    ) -> Self {
        Self {
            view_mode: ViewMode::Player,
            input_mode: InputMode::Normal,
            scroll_offset: 0,
            cursor: 0,
            filtered_indices: Vec::new(),
            current: 0,
            source_paths,
            status_message: None,
            metadata_cache,
            scan_handle: None,
            removed_paths: std::collections::HashSet::new(),
            banner_lines: 0,
            banner_text: String::new(),
            banner_hotkey_regions: Vec::new(),
            transport_click_regions: Vec::new(),
            playlist_mouse_rows: Vec::new(),
            playlist_dirty: false,
            current_track_removed: false,
            terminal_resized: false,
            lyrics: None,
            lyrics_receiver: None,
            lyrics_scroll: 0,
            lyrics_auto_scroll: true,
            lyrics_offset: 0.0,
            shuffle,
            repeat,
            session_idle: false,
            pending_resume_save: false,
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    pub fn take_status(&mut self) -> Option<String> {
        if let Some((ref msg, when)) = self.status_message {
            if when.elapsed() < std::time::Duration::from_secs(2) {
                return Some(msg.clone());
            }
            self.status_message = None;
        }
        None
    }
}
