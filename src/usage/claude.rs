//! The claude usage probe: auth status, credentials read, OAuth usage parse.

use std::path::Path;

use anyhow::Result;

use crate::exec::Exec;

use super::UsageRecord;

/// Probe the claude identity.
///
/// The probe runs `<program> auth status` for the billing mode, re-reads the
/// OAuth access token from `credentials_path` on every call, and reads the
/// quota windows from the OAuth usage endpoint through `curl`.
pub(crate) fn probe_claude(
    _exec: &dyn Exec,
    _program: &str,
    _credentials_path: &Path,
    _now_ms: u64,
) -> Result<UsageRecord> {
    Err(anyhow::anyhow!("not implemented"))
}
