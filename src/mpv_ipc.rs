//! mpv JSON IPC：在 Unix domain socket 上与 mpv 同步进度、暂停、音量与相对跳转。
//!
//! **协议**：`DOCS/man/ipc.rst`；非 Unix 平台不提供实现（调用方回退为无 IPC）。

#[cfg(unix)]
mod unix {
    use std::io::{self, BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    use serde_json::Value;

    /// 与 mpv 的一次快照（TUI 进度条、暂停图标）。
    #[derive(Clone, Debug)]
    pub struct MpvPoll {
        pub time_pos: f64,
        pub duration: f64,
        pub paused: bool,
    }

    pub struct MpvIpc {
        writer: std::os::unix::net::UnixStream,
        reader: BufReader<std::os::unix::net::UnixStream>,
        socket_path: PathBuf,
        child: Child,
        next_rid: u64,
    }

    fn parse_data_f64(v: &Value) -> Option<f64> {
        match v {
            Value::Null => Some(0.0),
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
    }

    fn rid_match(v: &Value, want: u64) -> bool {
        v.get("request_id")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("request_id").and_then(|x| x.as_i64()).map(|i| i as u64))
            == Some(want)
    }

    impl MpvIpc {
        pub fn spawn(path: &Path, socket_path: PathBuf) -> io::Result<Self> {
            let _ = std::fs::remove_file(&socket_path);
            let ipc_arg = format!("--input-ipc-server={}", socket_path.display());

            let mut child = Command::new("mpv")
                .arg(&ipc_arg)
                .args([
                    "--really-quiet",
                    "--no-terminal",
                    "--force-window=yes",
                    "--keep-open=no",
                ])
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;

            let stream = match Self::connect_with_retry(&socket_path) {
                Ok(s) => s,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
            let writer = stream.try_clone()?;
            let _ = writer.set_write_timeout(Some(std::time::Duration::from_secs(3)));
            let reader = BufReader::new(stream);

            Ok(Self {
                writer,
                reader,
                socket_path,
                child,
                next_rid: 1000,
            })
        }

        fn connect_with_retry(socket_path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
            for _ in 0..80 {
                match std::os::unix::net::UnixStream::connect(socket_path) {
                    Ok(s) => return Ok(s),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                }
            }
            std::os::unix::net::UnixStream::connect(socket_path)
        }

        fn next_id(&mut self) -> u64 {
            self.next_rid += 1;
            self.next_rid
        }

        pub fn poll_snapshot(&mut self) -> io::Result<MpvPoll> {
            let id_t = self.next_id();
            let id_d = self.next_id();
            let id_p = self.next_id();

            writeln!(
                self.writer,
                "{{\"command\":[\"get_property\",\"time-pos\"],\"request_id\":{}}}",
                id_t
            )?;
            writeln!(
                self.writer,
                "{{\"command\":[\"get_property\",\"duration\"],\"request_id\":{}}}",
                id_d
            )?;
            writeln!(
                self.writer,
                "{{\"command\":[\"get_property\",\"pause\"],\"request_id\":{}}}",
                id_p
            )?;
            self.writer.flush()?;

            let mut t = None::<f64>;
            let mut d = None::<f64>;
            let mut p = None::<bool>;
            let mut line = String::new();
            let mut guard = 0usize;

            while t.is_none() || d.is_none() || p.is_none() {
                guard += 1;
                if guard > 4096 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mpv IPC: too many lines while waiting for replies",
                    ));
                }
                line.clear();
                let n = self.reader.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let v: Value = serde_json::from_str(trimmed).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("mpv IPC JSON: {e}"))
                })?;

                if rid_match(&v, id_t) {
                    if v.get("error").and_then(|e| e.as_str()) == Some("success") {
                        t = v.get("data").and_then(parse_data_f64);
                    }
                } else if rid_match(&v, id_d) {
                    if v.get("error").and_then(|e| e.as_str()) == Some("success") {
                        d = v.get("data").and_then(parse_data_f64);
                    }
                } else if rid_match(&v, id_p) {
                    if v.get("error").and_then(|e| e.as_str()) == Some("success") {
                        p = v.get("data").and_then(|x| x.as_bool());
                    }
                }
            }

            Ok(MpvPoll {
                time_pos: t.unwrap_or(0.0).max(0.0),
                duration: d.unwrap_or(0.0).max(0.0),
                paused: p.unwrap_or(false),
            })
        }

        pub fn set_pause(&mut self, paused: bool) -> io::Result<()> {
            let id = self.next_id();
            writeln!(
                self.writer,
                "{{\"command\":[\"set_property\",\"pause\",{}],\"request_id\":{}}}",
                if paused { "true" } else { "false" },
                id
            )?;
            self.writer.flush()?;
            self.drain_until_id(id)
        }

        pub fn set_volume_keet(&mut self, keet_vol: u32) -> io::Result<()> {
            let v = (keet_vol as f64 * 100.0 / 150.0).clamp(0.0, 100.0);
            let id = self.next_id();
            writeln!(
                self.writer,
                "{{\"command\":[\"set_property\",\"volume\",{}],\"request_id\":{}}}",
                v, id
            )?;
            self.writer.flush()?;
            self.drain_until_id(id)
        }

        pub fn seek_relative(&mut self, delta_secs: i64) -> io::Result<()> {
            let id = self.next_id();
            let d = delta_secs as f64;
            writeln!(
                self.writer,
                "{{\"command\":[\"seek\",{},\"relative\"],\"request_id\":{}}}",
                d, id
            )?;
            self.writer.flush()?;
            self.drain_until_id(id)
        }

        fn drain_until_id(&mut self, want: u64) -> io::Result<()> {
            let mut line = String::new();
            let mut guard = 0usize;
            loop {
                guard += 1;
                if guard > 2048 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "mpv IPC: drain reply overflow",
                    ));
                }
                line.clear();
                let n = self.reader.read_line(&mut line)?;
                if n == 0 {
                    return Ok(());
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if rid_match(&v, want) {
                    return Ok(());
                }
            }
        }

        pub fn try_wait_child(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.child.try_wait()
        }

        pub fn kill_child(&mut self) -> io::Result<()> {
            let _ = self.child.kill();
            let _ = self.child.wait();
            Ok(())
        }
    }

    impl Drop for MpvIpc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

#[cfg(unix)]
pub use unix::MpvIpc;

#[cfg(not(unix))]
#[derive(Clone, Debug)]
pub struct MpvPoll {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
}
