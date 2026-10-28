//! The pairing offer the phone scans, version 2 (`src/shared/mobile-relay-pairing-offer.ts`). The
//! JSON is encoded as unpadded base64url and carried as `agenthub://pair?code=…`, the scheme the
//! phone app registers. `encodePairingOffer` in `src/shared/pairing.ts` writes the same code under
//! `orca://`; the app still reads that form for offers printed before the rename. Only direct offers
//! exist here: no relay block, so no relay credentials.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;

/// Orca's `PAIRING_ENDPOINT_MAX_CHARACTERS`.
const ENDPOINT_MAX: usize = 16 * 1024;

/// The start of the pairing link. The phone app registers the `agenthub` scheme with the OS.
const PAIR_LINK: &str = "agenthub://pair?code=";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// A `ws://` or `wss://` URL the phone dials.
    pub endpoint: String,
    pub device_token: String,
    /// The bridge's Curve25519 public key, standard base64 of 32 bytes.
    pub public_key_b64: String,
}

impl Offer {
    /// The offer as the app's schema reads it. Key order is kept as written.
    pub fn json(&self) -> serde_json::Value {
        json!({
            "v": 2,
            "endpoint": self.endpoint,
            "deviceToken": self.device_token,
            "publicKeyB64": self.public_key_b64,
        })
    }

    pub fn to_json(&self) -> String {
        self.json().to_string()
    }

    /// The `agenthub://pair?code=` link, the same text the QR code carries.
    pub fn url(&self) -> String {
        format!("{PAIR_LINK}{}", URL_SAFE_NO_PAD.encode(self.to_json()))
    }
}

/// Refuse an endpoint the phone could not dial. A malformed endpoint would otherwise print an offer
/// that pairs and then never connects.
pub fn check_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.len() > ENDPOINT_MAX {
        return Err("the endpoint is too long".into());
    }
    let rest = endpoint.strip_prefix("ws://")
        .or_else(|| endpoint.strip_prefix("wss://"))
        .ok_or_else(|| format!("{endpoint}: the endpoint must start with ws:// or wss://"))?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(format!("{endpoint}: the endpoint has no host"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Offer {
        Offer {
            endpoint: "ws://10.10.20.3:6768".into(),
            device_token: "t".repeat(43),
            public_key_b64: "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=".into(),
        }
    }

    #[test]
    fn the_json_keeps_the_schema_key_order_and_version() {
        assert_eq!(
            sample().to_json(),
            format!(
                r#"{{"v":2,"endpoint":"ws://10.10.20.3:6768","deviceToken":"{}","publicKeyB64":"zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk="}}"#,
                "t".repeat(43)
            )
        );
    }

    #[test]
    fn the_url_decodes_back_to_the_same_json_without_padding() {
        let offer = sample();
        let url = offer.url();
        let code = url.strip_prefix("agenthub://pair?code=").expect("scheme and query");
        assert!(!code.contains('='), "padding is stripped, as the desktop does");
        assert!(!code.contains('+') && !code.contains('/'), "base64url alphabet");
        let json = String::from_utf8(URL_SAFE_NO_PAD.decode(code).unwrap()).unwrap();
        assert_eq!(json, offer.to_json());
    }

    #[test]
    fn the_link_is_agenthub_not_orca() {
        let url = sample().url();
        assert!(url.starts_with("agenthub://pair?code="), "the phone app registers agenthub");
        assert!(!url.starts_with("orca:"), "the bridge prints no orca links");
    }

    #[test]
    fn only_websocket_endpoints_with_a_host_are_accepted() {
        assert!(check_endpoint("ws://10.10.20.3:6768").is_ok());
        assert!(check_endpoint("wss://bridge.example:443/path").is_ok());
        assert!(check_endpoint("http://10.10.20.3:6768").is_err());
        assert!(check_endpoint("ws://").is_err());
        assert!(check_endpoint("ws:///path").is_err());
        assert!(check_endpoint("ws://a b:1").is_err());
    }
}
