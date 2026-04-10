#!/bin/bash
# Create a macOS .app bundle for Keet
set -e

APP="Keet.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

# Build release binary
cargo build --release

# Create bundle structure
rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Copy binary and icon
cp target/release/keet "$MACOS/keet-bin"
cp assets/icon.icns "$RESOURCES/keet.icns"

# Launcher:
# - If launched from an existing terminal, run the real binary directly (full TUI).
# - If launched from Finder (no tty), open Terminal and start the TUI.
#   If no previous session exists, prompt the user to pick a folder first.
cat > "$MACOS/keet" << 'EOF'
#!/bin/bash
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/keet-bin"

if [ -t 1 ]; then
  exec "$BIN" "$@"
fi

osascript - "$BIN" <<'APPLESCRIPT'
on run argv
  set binPath to item 1 of argv

  -- If no saved session exists, pick a folder to play.
  set cfgDir to (POSIX path of (path to home folder)) & ".config/keet/"
  set resumeFile to cfgDir & "resume.json"
  set hasResume to false
  try
    set f to open for access POSIX file resumeFile
    close access f
    set hasResume to true
  on error
    set hasResume to false
  end try

  set cmd to "clear; "
  if hasResume then
    set cmd to cmd & quoted form of binPath
  else
    try
      set p to choose folder with prompt "选择要播放的目录（或包含音频的文件夹）"
      set cmd to cmd & quoted form of binPath & " " & quoted form of (POSIX path of p)
    on error number -128
      set cmd to cmd & quoted form of binPath & " --help"
    end try
  end if

  tell application "Terminal"
    activate
    do script cmd
  end tell
end run
APPLESCRIPT
EOF

chmod +x "$MACOS/keet" "$MACOS/keet-bin"

# Create Info.plist
cat > "$CONTENTS/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Keet</string>
    <key>CFBundleDisplayName</key>
    <string>Keet</string>
    <key>CFBundleIdentifier</key>
    <string>com.keet.audio-player</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleExecutable</key>
    <string>keet</string>
    <key>CFBundleIconFile</key>
    <string>keet</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Keet needs audio device access for playback.</string>
</dict>
</plist>
EOF

echo "Created $APP"
