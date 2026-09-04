//! The OpenCode usage probes: z.ai plan, Zen/Go plan, and other providers.

use std::path::Path;

use anyhow::Result;

use crate::exec::Exec;

use super::UsageRecord;

/// Read the OpenCode auth token for one provider.
///
/// `OPENCODE_AUTH_CONTENT` wins over the auth file, so a test or a sandboxed
/// operator can inject the token. `None` means no entry exists.
pub(crate) fn read_opencode_auth(_auth_path: &Path) -> Result<Option<String>> {
    Err(anyhow::anyhow!("not implemented"))
}

/// Probe the `zai-coding-plan` identity through the z.ai monitor endpoint.
pub(crate) fn probe_zai(_exec: &dyn Exec, _token: &str, _now_ms: u64) -> Result<UsageRecord> {
    Err(anyhow::anyhow!("not implemented"))
}

/// Probe the `opencode` identity through the Zen/Go usage endpoint.
pub(crate) fn probe_zen(_exec: &dyn Exec, _token: &str, _now_ms: u64) -> Result<UsageRecord> {
    Err(anyhow::anyhow!("not implemented"))
}

/// Build the row of any other OpenCode provider.
///
/// A provider without a quota endpoint still gets a record: the factory
/// spend always shows, and an admin environment variable may add the
/// organization costs.
pub(crate) fn probe_other_provider(
    _exec: &dyn Exec,
    _provider: &str,
    _now_ms: u64,
) -> Result<UsageRecord> {
    Err(anyhow::anyhow!("not implemented"))
}
