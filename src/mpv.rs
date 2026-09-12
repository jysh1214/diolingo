//! mpv as a headless audio player, driven over its JSON IPC socket.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// One player per session: `$XDG_RUNTIME_DIR/diolingo-mpv.sock`.
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("diolingo-mpv.sock")
}

/// Write one IPC command line (`{"command": [...]}`) to `stream`.
pub fn send(stream: &mut UnixStream, command: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(command).expect("serialisable command");
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Blocking request/reply client for one-shot commands (`diolingo ctl`).
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).with_context(|| format!("no player running ({} is not reachable)", path.display()))?;
        Ok(Self { writer: stream.try_clone()?, reader: BufReader::new(stream) })
    }

    /// Send `args` as an mpv command and return the reply's `data`, or the
    /// error mpv reported. Events arriving meanwhile are skipped.
    pub fn command(&mut self, args: Vec<Value>) -> Result<Value> {
        const REQUEST_ID: u64 = 1;
        send(&mut self.writer, &json!({ "command": args, "request_id": REQUEST_ID }))?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("mpv closed the connection");
            }
            let reply: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if reply.get("request_id").and_then(Value::as_u64) != Some(REQUEST_ID) {
                continue;
            }
            return match reply.get("error").and_then(Value::as_str) {
                Some("success") => Ok(reply.get("data").cloned().unwrap_or(Value::Null)),
                Some(e) => bail!("mpv: {e}"),
                None => Ok(Value::Null),
            };
        }
    }
}

/// Turn `diolingo ctl` words into mpv command arguments: integers and floats
/// become JSON numbers, everything else stays a string.
pub fn parse_ctl_args(words: &[String]) -> Vec<Value> {
    words
        .iter()
        .map(|w| {
            if let Ok(i) = w.parse::<i64>() {
                json!(i)
            } else if let Ok(f) = w.parse::<f64>() {
                json!(f)
            } else {
                json!(w)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_args_numbers() {
        let v = parse_ctl_args(&["sub-seek".into(), "-1".into(), "0.5".into()]);
        assert_eq!(v, vec![json!("sub-seek"), json!(-1), json!(0.5)]);
    }
}
