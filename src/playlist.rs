use std::fs;
use std::path::{Path, PathBuf};

use crate::track::{is_supported_media_extension, Track};

pub fn shuffle_tracks(list: &mut [Track]) {
    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(12345);
    for i in (1..list.len()).rev() {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        list.swap(i, rng as usize % (i + 1));
    }
}

pub fn build_playlist(path: &Path, shuffle: bool) -> Result<Vec<Track>, Box<dyn std::error::Error>> {
    // Check for M3U playlist file
    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if ext_lower == "m3u" || ext_lower == "m3u8" {
            let mut list = parse_m3u(path)?;
            if shuffle {
                shuffle_tracks(&mut list);
            }
            return Ok(list);
        }
    }

    let mut list = Vec::new();

    if path.is_file() {
        list.push(Track::new(path.to_path_buf()));
    } else if path.is_dir() {
        fn scan_dir(dir: &Path, list: &mut Vec<Track>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        scan_dir(&p, list);
                    } else if p.is_file() {
                        if let Some(ext) = p.extension() {
                            if is_supported_media_extension(&ext.to_string_lossy()) {
                                list.push(Track::new(p));
                            }
                        }
                    }
                }
            }
        }
        scan_dir(path, &mut list);
        list.sort_by(|a, b| a.path.cmp(&b.path));

        if shuffle {
            shuffle_tracks(&mut list);
        }
    }

    if list.is_empty() {
        return Err("No audio or video files found".into());
    }
    Ok(list)
}

/// Returns the platform-aware Keet config directory.
pub fn keet_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("APPDATA").ok().map(|p| PathBuf::from(p).join("keet"))
    } else {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config").join("keet"))
    }
}

/// Parse an M3U/M3U8 playlist file into a list of tracks.
pub fn parse_m3u(path: &Path) -> Result<Vec<Track>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut list = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let track_path = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            parent.join(line)
        };
        if track_path.is_file() {
            if let Some(ext) = track_path.extension() {
                if is_supported_media_extension(&ext.to_string_lossy()) {
                    list.push(Track::new(track_path));
                }
            }
        }
    }

    if list.is_empty() {
        return Err("No audio or video files found in playlist".into());
    }
    Ok(list)
}

/// Save a playlist as an M3U file.
/// If `name` contains a path separator, treat it as a full path.
/// Otherwise, save to ~/.config/keet/playlists/<name>.m3u.
pub fn save_m3u(playlist: &[Track], name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = if name.contains('/') || name.contains('\\') {
        let p = PathBuf::from(name);
        if !p.to_string_lossy().ends_with(".m3u") && !p.to_string_lossy().ends_with(".m3u8") {
            p.with_extension("m3u")
        } else {
            p
        }
    } else {
        let dir = keet_config_dir()
            .ok_or("Could not determine config directory")?
            .join("playlists");
        fs::create_dir_all(&dir)?;
        let filename = if name.ends_with(".m3u") || name.ends_with(".m3u8") {
            name.to_string()
        } else {
            format!("{}.m3u", name)
        };
        dir.join(&filename)
    };

    // Ensure parent directory exists for arbitrary paths
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut content = String::from("#EXTM3U\n");
    for track in playlist {
        content.push_str(&track.path.to_string_lossy());
        content.push('\n');
    }
    fs::write(&path, &content)?;
    Ok(path)
}

/// Rescan source path and diff against current playlist.
/// Returns (added_count, removed_count).
pub fn rescan_playlist(
    source_path: &Path,
    playlist: &mut Vec<Track>,
    current_track_path: Option<&Path>,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let fresh = if source_path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .map(|e| e == "m3u" || e == "m3u8")
        .unwrap_or(false)
    {
        parse_m3u(source_path)?
    } else {
        build_playlist(source_path, false)?
    };

    let current_set: std::collections::HashSet<&std::path::Path> =
        playlist.iter().map(|t| t.path.as_path()).collect();
    let fresh_set: std::collections::HashSet<&std::path::Path> =
        fresh.iter().map(|t| t.path.as_path()).collect();

    // Find new files (in fresh but not in current)
    let mut added: Vec<Track> = fresh
        .iter()
        .filter(|t| !current_set.contains(t.path.as_path()))
        .cloned()
        .collect();
    let added_count = added.len();

    // Find removed files (in current but not in fresh)
    let removed_count = current_set.difference(&fresh_set).count();

    // Remove missing files (preserve order, skip currently playing track)
    playlist.retain(|t| {
        if !fresh_set.contains(t.path.as_path()) {
            current_track_path.map(|c| c == t.path.as_path()).unwrap_or(false)
        } else {
            true
        }
    });

    // Append new files to tail
    playlist.append(&mut added);

    Ok((added_count, removed_count))
}
