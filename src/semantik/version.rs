/// Semantic version of this SDK release. Included in every outgoing
/// `User-Agent` header and available to callers that want to log or
/// report the running SDK revision.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the `User-Agent` header value emitted by every request.
///
/// The format is stable: callers are free to grep server logs for it.
/// Tooling that talks to Semantik directly (debug clients, load
/// generators) should use this so server logs see one wire-level
/// identity for "the Rust client family".
///
/// ```text
/// noetive-sdk-rust/<VERSION> (rust; <os>/<arch>)
/// ```
///
/// The Rust toolchain version is omitted intentionally — it is not
/// available at runtime without an extra build-script dependency, and
/// the server-side use case (grouping by SDK family) does not need it.
pub fn user_agent() -> String {
    format!(
        "noetive-sdk-rust/{} (rust; {}/{})",
        VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_has_stable_prefix() {
        let ua = user_agent();
        assert!(ua.starts_with("noetive-sdk-rust/"));
        assert!(ua.contains(VERSION));
        assert!(ua.contains(std::env::consts::OS));
        assert!(ua.contains(std::env::consts::ARCH));
    }

    #[test]
    fn version_matches_cargo_pkg_version() {
        // Cheap sanity: the constant is wired to the package version.
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
