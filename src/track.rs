//! 统一曲目类型：路径 + 媒体种类（音频走 symphonia/cpal，视频走外部 mpv 窗口）。
//!
//! **设计**：列表与导航共用一套结构；播放后端在 `main` 中按 [`MediaKind`] 分支，避免在解码线程内探测视频容器。

use std::path::{Path, PathBuf};

/// 音频扩展名（小写，不含点）；与历史 `state::SUPPORTED_EXTENSIONS` 一致。
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "m4a", "aiff", "aif"];

/// 常见视频容器扩展名（小写，不含点）；用于目录扫描与 M3U。
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// 播放列表中的一项：路径与种类（由扩展名决定，可在构建后保持不可变）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    pub kind: MediaKind,
}

impl Track {
    pub fn new(path: PathBuf) -> Self {
        let kind = media_kind_for_path(&path);
        Self { path, kind }
    }
}

pub fn media_kind_for_path(path: &Path) -> MediaKind {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .map(|ext| {
            if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
                MediaKind::Video
            } else {
                MediaKind::Audio
            }
        })
        .unwrap_or(MediaKind::Audio)
}

/// 目录 / M3U 是否应收录该扩展名。
pub fn is_supported_media_extension(ext: &str) -> bool {
    let e = ext.to_lowercase();
    AUDIO_EXTENSIONS.contains(&e.as_str()) || VIDEO_EXTENSIONS.contains(&e.as_str())
}
