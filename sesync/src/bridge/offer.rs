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

    /// Read a pairing link (`agenthub://pair?code=…`, or the older `orca://` form) or the offer's JSON, as
    /// `sesync bridge pair` prints them. The endpoint must be one a client can dial, and the key 32 bytes.
    pub fn parse(text: &str) -> Result<Offer, String> {
        let text = text.trim();
        let json = match text.split_once("://pair?code=") {
            Some((_, code)) => {
                let bytes = URL_SAFE_NO_PAD.decode(code.trim_end_matches('='))
                    .map_err(|_| "the pairing link's code is not base64url".to_string())?;
                String::from_utf8(bytes).map_err(|_| "the pairing link's code is not text".to_string())?
            }
            None => text.to_string(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&json).map_err(|_| "not a pairing link or offer".to_string())?;
        let field = |name: &str| {
            value[name].as_str().filter(|s| !s.is_empty()).map(str::to_string)
                .ok_or_else(|| format!("the offer has no {name}"))
        };
        let offer = Offer {
            endpoint: field("endpoint")?,
            device_token: field("deviceToken")?,
            public_key_b64: field("publicKeyB64")?,
        };
        check_endpoint(&offer.endpoint)?;
        if super::e2ee::parse_client_key(&offer.public_key_b64).is_none() {
            return Err("the offer's public key is not 32 bytes of base64".into());
        }
        Ok(offer)
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
            endpoint: "ws://10.0.0.3:6768".into(),
            device_token: "t".repeat(43),
            public_key_b64: "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=".into(),
        }
    }

    #[test]
    fn the_json_keeps_the_schema_key_order_and_version() {
        assert_eq!(
            sample().to_json(),
            format!(
                r#"{{"v":2,"endpoint":"ws://10.0.0.3:6768","deviceToken":"{}","publicKeyB64":"zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk="}}"#,
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
        assert!(check_endpoint("ws://10.0.0.3:6768").is_ok());
        assert!(check_endpoint("wss://bridge.example:443/path").is_ok());
        assert!(check_endpoint("http://10.0.0.3:6768").is_err());
        assert!(check_endpoint("ws://").is_err());
        assert!(check_endpoint("ws:///path").is_err());
        assert!(check_endpoint("ws://a b:1").is_err());
    }

    #[test]
    fn a_printed_link_or_offer_parses_back_and_a_damaged_one_is_refused() {
        let offer = sample();
        assert_eq!(Offer::parse(&offer.url()).unwrap(), offer);
        assert_eq!(Offer::parse(&format!("  {}\n", offer.to_json())).unwrap(), offer);
        let orca = offer.url().replace("agenthub://", "orca://");
        assert_eq!(Offer::parse(&orca).unwrap(), offer, "links printed before the rename still read");
        assert!(Offer::parse("agenthub://pair?code=!!").is_err());
        assert!(Offer::parse(&offer.to_json().replace("ws://", "http://")).is_err());
        assert!(Offer::parse(&offer.to_json().replace("zo060cy2M+x7", "zo0")).is_err());
        assert!(Offer::parse(r#"{"v":2,"endpoint":"ws://h:1"}"#).is_err());
    }
}
