use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ids;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub v: u32,
    pub in_reply_to: String,
    pub ok: bool,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

impl Reply {
    pub fn ok(id: &str, revision: u64, result: serde_json::Value) -> Self {
        Reply {
            v: PROTOCOL_VERSION,
            in_reply_to: id.to_owned(),
            ok: true,
            revision,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: &str, revision: u64, code: &'static str, message: String) -> Self {
        Reply {
            v: PROTOCOL_VERSION,
            in_reply_to: id.to_owned(),
            ok: false,
            revision,
            result: None,
            error: Some(ControlError {
                code: code.to_owned(),
                message,
            }),
        }
    }
}

pub struct ControlServer;

pub enum Incoming {
    Request {
        envelope: Envelope,
        reply: Sender<Reply>,
        follow: Option<Sender<String>>,
    },
}

pub fn serve(socket_path: &Path, tx: Sender<Incoming>) -> Result<()> {
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            anyhow::bail!(
                "another daemon already listens on {}",
                socket_path.display()
            );
        }
        std::fs::remove_file(socket_path).ok();
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            if let Err(err) = serve_conn(stream, tx) {
                let _ = writeln!(std::io::stderr(), "aif: control conn ended: {err}");
            }
        });
    }
    Ok(())
}

fn serve_conn(stream: UnixStream, tx: Sender<Incoming>) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let envelope: Envelope = match serde_json::from_str(&line) {
            Ok(env) => env,
            Err(err) => {
                let reply = Reply::err(&ids::new_id(), 0, "bad_request", format!("{err}"));
                write_reply(&mut writer, &reply)?;
                continue;
            }
        };
        if envelope.v != PROTOCOL_VERSION {
            let reply = Reply::err(
                &envelope.id,
                0,
                "version_mismatch",
                format!("server speaks v{PROTOCOL_VERSION}"),
            );
            write_reply(&mut writer, &reply)?;
            continue;
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
        let follow_pair =
            (envelope.method == "events.follow").then(std::sync::mpsc::channel::<String>);
        if tx
            .send(Incoming::Request {
                envelope,
                reply: reply_tx,
                follow: follow_pair.as_ref().map(|(tx, _)| tx.clone()),
            })
            .is_err()
        {
            anyhow::bail!("daemon channel closed");
        }
        match reply_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(reply) => write_reply(&mut writer, &reply)?,
            Err(_) => anyhow::bail!("daemon did not reply in time"),
        }
        if let Some((_, follow_rx)) = follow_pair {
            for record in follow_rx {
                let mut line = record;
                line.push('\n');
                if writer.write_all(line.as_bytes()).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        }
    }
    Ok(())
}

fn write_reply(writer: &mut UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

pub fn request(socket_path: &Path, method: &str, params: serde_json::Value) -> Result<Reply> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("cannot reach daemon at {}", socket_path.display()))?;
    let envelope = Envelope {
        v: PROTOCOL_VERSION,
        id: ids::new_id(),
        method: method.to_owned(),
        params,
    };
    let mut line = serde_json::to_string(&envelope)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    let reply: Reply = serde_json::from_str(response.trim())
        .with_context(|| format!("bad daemon reply: {response:?}"))?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let raw = r#"{"v":1,"id":"r1","method":"task.cancel","params":{"task_id":"t"}}"#;
        let env: Envelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.method, "task.cancel");
        assert_eq!(env.params["task_id"], "t");
    }

    #[test]
    fn reply_serializes_error_only_when_present() {
        let ok = Reply::ok("r1", 3, serde_json::json!({"a":1}));
        let text = serde_json::to_string(&ok).unwrap();
        assert!(!text.contains("error"));
        let err = Reply::err("r2", 3, "not_found", "no task".into());
        let text = serde_json::to_string(&err).unwrap();
        assert!(text.contains("not_found"));
    }
}
