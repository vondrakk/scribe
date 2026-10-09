//! Interactive pseudo-terminal sessions for the Terminal tab.
//!
//! Each terminal tab is a real PTY running the engineer's login shell, so
//! interactive programs — including `claude` (Claude Code) — run live. The
//! browser (xterm.js) talks to it over simple JSON polling: output is read
//! from a per-session buffer by byte offset, input and resizes are posted in.
//!
//! The shell is started as a login+interactive bash, so ~/.bashrc runs and the
//! Scribe capture snippet (if sourced there) records commands typed here too.
//!
//! Loopback-only, like the rest of the panel: these sessions are reachable only
//! from this machine, by this user — the same boundary as their own terminal.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct Session {
    #[allow(dead_code)]
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    #[allow(dead_code)]
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

fn registry() -> &'static Mutex<HashMap<String, Session>> {
    static R: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open a new PTY running a login+interactive bash. Returns a session id.
pub fn open(cols: u16, rows: u16, cwd: &str) -> Result<String, String> {
    let sys = native_pty_system();
    let pair = sys
        .openpty(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.arg("-l"); // login shell → sources profile/bashrc (and Scribe capture)
    cmd.arg("-i");
    cmd.env("TERM", "xterm-256color");
    let dir = if cwd.trim().is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    } else {
        cwd.to_string()
    };
    cmd.cwd(dir);

    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn: {e}"))?;
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().map_err(|e| format!("reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("writer: {e}"))?;

    let buf = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let buf = buf.clone();
        let mut reader = reader;
        std::thread::spawn(move || {
            let mut tmp = [0u8; 8192];
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut b) = buf.lock() {
                            b.extend_from_slice(&tmp[..n]);
                        }
                    }
                }
            }
        });
    }

    let id = format!("pty{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    registry().lock().unwrap().insert(
        id.clone(),
        Session { master: pair.master, writer, buf, child },
    );
    Ok(id)
}

/// Read output from `from` byte offset. Returns (base64 data, new offset, alive).
pub fn read(id: &str, from: usize) -> (String, usize, bool) {
    let reg = registry().lock().unwrap();
    match reg.get(id) {
        None => (String::new(), from, false),
        Some(s) => {
            let b = s.buf.lock().unwrap();
            if from >= b.len() {
                (String::new(), b.len(), true)
            } else {
                (b64(&b[from..]), b.len(), true)
            }
        }
    }
}

/// Write input bytes to the PTY.
pub fn write(id: &str, data: &str) -> bool {
    let mut reg = registry().lock().unwrap();
    if let Some(s) = reg.get_mut(id) {
        let _ = s.writer.write_all(data.as_bytes());
        let _ = s.writer.flush();
        true
    } else {
        false
    }
}

pub fn resize(id: &str, cols: u16, rows: u16) {
    let reg = registry().lock().unwrap();
    if let Some(s) = reg.get(id) {
        let _ = s.master.resize(PtySize {
            rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0,
        });
    }
}

pub fn close(id: &str) {
    // Dropping the Session kills the child and closes the PTY.
    let _ = registry().lock().unwrap().remove(id);
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}
