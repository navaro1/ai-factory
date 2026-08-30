use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::ids;
use crate::snapshot::ItemState;
use crate::task::ExtIds;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rec {
    FactoryMeta {
        root: String,
        repo: String,
    },
    Trust {
        granted: bool,
    },
    SourceBatch {
        items: Vec<ItemState>,
        #[serde(default)]
        forced: bool,
    },
    TaskCreated {
        id: String,
        node: String,
        item_kind: String,
        number: u64,
        item_node_id: String,
        title: String,
        revision: u64,
        attempt: u32,
    },
    TaskTransition {
        id: String,
        from: String,
        to: String,
        #[serde(default)]
        detail: String,
    },
    DispatchBegin {
        id: String,
        target: String,
    },
    External {
        id: String,
        #[serde(flatten)]
        ext: ExtIds,
    },
    Paused {
        node: Option<String>,
        paused: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub seq: u64,
    pub ts: String,
    pub v: u32,
    #[serde(flatten)]
    pub rec: Rec,
}

pub struct Journal {
    path: PathBuf,
    file: File,
    seq: u64,
    size: u64,
}

impl Journal {
    pub fn open(path: &Path) -> Result<(Self, Vec<Record>)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let records = Self::replay(path)?;
        let seq = records.last().map(|r| r.seq).unwrap_or(0);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok((
            Journal {
                path: path.to_path_buf(),
                file,
                seq,
                size,
            },
            records,
        ))
    }

    pub fn replay(path: &Path) -> Result<Vec<Record>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(path).context("journal open")?;
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();
        let mut buf = Vec::new();
        let mut partial_tail = false;
        loop {
            buf.clear();
            let read = reader
                .read_until(b'\n', &mut buf)
                .with_context(|| "journal read")?;
            if read == 0 {
                break;
            }
            if !buf.ends_with(b"\n") {
                partial_tail = true;
                break;
            }
            let line = String::from_utf8_lossy(&buf).trim().to_owned();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(&line) {
                Ok(rec) => records.push(rec),
                Err(err) => {
                    if partial_tail {
                        break;
                    }
                    bail!("journal corruption at record {}: {err}", records.len() + 1);
                }
            }
        }
        if partial_tail {
            eprintln!("aif: journal has a torn final record; ignoring it");
        }
        Ok(records)
    }

    pub fn append(&mut self, rec: Rec) -> Result<Record> {
        self.seq += 1;
        let record = Record {
            seq: self.seq,
            ts: ids::now_iso(),
            v: SCHEMA_VERSION,
            rec,
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.file.sync_all()?;
        self.size += line.len() as u64;
        Ok(record)
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::ItemKind;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-journal-{}", ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item(node_id: &str) -> ItemState {
        ItemState {
            repo_id: 1,
            node_id: node_id.into(),
            kind: ItemKind::Issue,
            number: 1,
            title: "t".into(),
            open: true,
            draft: false,
            labels: vec![],
            blocked_by: vec![],
            head: None,
        }
    }

    #[test]
    fn append_and_replay_round_trip() {
        let dir = dir();
        let path = dir.join("journal.jsonl");
        let (mut journal, _) = Journal::open(&path).unwrap();
        journal.append(Rec::SourceBatch { items: vec![item("I_1")], forced: false }).unwrap();
        journal
            .append(Rec::TaskTransition {
                id: "t1".into(),
                from: "queued".into(),
                to: "reserved".into(),
                detail: String::new(),
            })
            .unwrap();
        let records = Journal::replay(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);
        assert_eq!(records[1].v, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn torn_tail_is_ignored_but_midfile_corruption_fails() {
        let dir = dir();
        let path = dir.join("journal.jsonl");
        let (mut journal, _) = Journal::open(&path).unwrap();
        journal.append(Rec::Trust { granted: true }).unwrap();
        std::fs::write(&path, "{\"seq\":1,\"ts\":\"x\",\"v\":1,\"kind\":\"trust\",\"granted\":true}\n{torn").unwrap();
        let records = Journal::replay(&path).unwrap();
        assert_eq!(records.len(), 1);

        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "not json\n").unwrap();
        assert!(Journal::replay(&bad).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seq_continues_after_reopen() {
        let dir = dir();
        let path = dir.join("journal.jsonl");
        {
            let (mut journal, _) = Journal::open(&path).unwrap();
            journal.append(Rec::Trust { granted: true }).unwrap();
        }
        let (mut journal, replayed) = Journal::open(&path).unwrap();
        assert_eq!(replayed.len(), 1);
        let rec = journal.append(Rec::Trust { granted: false }).unwrap();
        assert_eq!(rec.seq, 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
