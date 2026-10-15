//! API keys in the macOS keychain (service `quorum`), managed by `quorum secrets`.
//!
//! A key passes between processes only through `security`'s own prompt and stdout — never as a
//! command-line argument (visible in `ps`) — and is never logged or printed.

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// Keychain service name for every quorum secret.
const SERVICE: &str = "quorum";
/// Tiingo API key: US stock and ETF end-of-day bars, read only.
pub(crate) const TIINGO: &str = "tiingo";
/// Every secret `quorum secrets` manages.
pub(crate) const KNOWN: [&str; 1] = [TIINGO];

/// The secret for `account`, if stored.
pub(crate) fn get(account: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!key.is_empty()).then_some(key)
}

/// Stores the secret for `account`, prompting on the terminal (`security` reads it; it is never
/// on a command line). Replaces an existing one.
pub(crate) fn store_interactive(account: &str) -> Result<()> {
    if !KNOWN.contains(&account) {
        bail!("unknown secret `{account}` (known: {})", KNOWN.join(", "));
    }
    // `-w` as the last option with no value makes `security` prompt for the secret.
    let status = Command::new("security")
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", account])
        .args(["-l", &format!("quorum {account} API key"), "-w"])
        .status()?;
    if !status.success() {
        bail!("the keychain tool exited with {status}; nothing stored");
    }
    Ok(())
}
