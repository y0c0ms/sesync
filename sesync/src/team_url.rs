//! The one check every team request passes before a token leaves this machine.
//!
//! Tokens are bearer credentials. Plain `http://` sends them in clear text, so it is refused
//! unless the host is loopback or the operator opts in with `SESYNC_TEAM_ALLOW_HTTP=1` (for a
//! deliberately private network such as a WireGuard mesh).

/// Environment variable that accepts plain HTTP to a non-loopback team server.
pub const ALLOW_HTTP_ENV: &str = "SESYNC_TEAM_ALLOW_HTTP";

/// `Ok` if a request carrying a token may be sent to `base`. Errors name the fix, never the token.
pub fn check_scheme(base: &str) -> Result<(), String> {
    let allow = std::env::var(ALLOW_HTTP_ENV).map(|v| v == "1").unwrap_or(false);
    check_scheme_with(base, allow)
}

/// The decision, separated from the environment so it can be tested directly.
pub fn check_scheme_with(base: &str, allow_http: bool) -> Result<(), String> {
    let base = base.trim();
    if base.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = base.strip_prefix("http://") else {
        return Err(format!("team server URL must start with https:// (got {base:?})"));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    // Parse the address: a name that merely starts with "127." (127.attacker.example) is not loopback.
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if loopback || allow_http {
        return Ok(());
    }
    Err(format!(
        "refusing to send a team token over plain http to {host}; use https://, or set {ALLOW_HTTP_ENV}=1 \
         if this host is on a private network you trust"
    ))
}

#[cfg(test)]
mod tests {
    use super::check_scheme_with;

    #[test]
    fn https_is_always_accepted() {
        assert!(check_scheme_with("https://team.example.com", false).is_ok());
    }

    #[test]
    fn plain_http_to_a_remote_host_is_refused_without_opt_in() {
        let err = check_scheme_with("http://10.10.20.6:30500", false).unwrap_err();
        assert!(err.contains("10.10.20.6"), "{err}");
        assert!(!err.contains("token="), "the message must not echo credentials: {err}");
    }

    #[test]
    fn plain_http_to_a_remote_host_is_accepted_with_opt_in() {
        assert!(check_scheme_with("http://10.10.20.6:30500", true).is_ok());
    }

    #[test]
    fn plain_http_to_loopback_is_accepted() {
        assert!(check_scheme_with("http://127.0.0.1:30500", false).is_ok());
        assert!(check_scheme_with("http://localhost:30500/", false).is_ok());
        assert!(check_scheme_with("http://[::1]:30500", false).is_ok());
    }

    #[test]
    fn userinfo_cannot_make_a_remote_host_look_local() {
        assert!(check_scheme_with("http://127.0.0.1@10.10.20.6:30500", false).is_err());
    }

    #[test]
    fn a_name_that_starts_with_127_is_not_loopback() {
        assert!(check_scheme_with("http://127.attacker.example:30500", false).is_err());
        assert!(check_scheme_with("http://127.0.0.1.attacker.example", false).is_err());
        assert!(check_scheme_with("http://127.255.0.9:30500", false).is_ok());
    }

    #[test]
    fn other_schemes_are_refused() {
        assert!(check_scheme_with("ftp://127.0.0.1", true).is_err());
        assert!(check_scheme_with("10.10.20.6:30500", true).is_err());
    }
}
