//! The codex usage probe: auth mode, app-server rate limits, admin costs.

use std::path::Path;

use anyhow::Result;

use crate::exec::Exec;

use super::UsageRecord;

/// Probe the codex identity.
///
/// The auth file decides the mode: a `tokens` object means a ChatGPT plan
/// and a JSON-RPC conversation with `<program> app-server`; an
/// `OPENAI_API_KEY` means the direct API mode with spend only.
pub(crate) fn probe_codex(
    _exec: &dyn Exec,
    _program: &str,
    _auth_path: &Path,
    _now_ms: u64,
) -> Result<UsageRecord> {
    Err(anyhow::anyhow!("not implemented"))
}
