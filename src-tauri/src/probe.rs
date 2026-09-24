use crate::model::AppConfig;
use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use ureq::{Agent, OrAnyStatus};

const FALLBACK_PROBE_URL: &str = "https://www.baidu.com/";
const MAX_BODY_BYTES: u64 = 128 * 1024;
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum ProbeResult {
    Online {
        elapsed_ms: u128,
    },
    /// Raw values stay in memory for parsing only and must never be logged.
    Redirect {
        status: u16,
        location: String,
        body: String,
        elapsed_ms: u128,
    },
    Http {
        status: u16,
        elapsed_ms: u128,
    },
    /// `kind` is a stable ureq error category, never its display string (which
    /// can contain the full request URL).
    Transport {
        kind: String,
        elapsed_ms: u128,
    },
}

/// Probe without following redirects. An ordinary public redirect is checked
/// against a second HTTPS endpoint before being treated as a captive portal.
pub fn probe(config: &AppConfig) -> ProbeResult {
    probe_with_timeout(config, DEFAULT_PROBE_TIMEOUT)
}

/// Probe with a caller-selected upper bound for each HTTP request. The normal
/// monitor uses a short bound, while recovery verification can choose an even
/// shorter one so a stale route does not block the next confirmation attempt.
pub fn probe_with_timeout(config: &AppConfig, timeout: Duration) -> ProbeResult {
    let primary = probe_once(&config.check_url, false, timeout);
    match &primary {
        ProbeResult::Online { .. } => primary,
        ProbeResult::Redirect { location, body, .. } if looks_like_portal(location, body) => {
            primary
        }
        _ => {
            // The fallback is only used after the primary endpoint did not
            // establish connectivity. Keep its timeout bounded by the same
            // caller budget so a failed confirmation cannot add a long stall.
            let fallback = probe_once(FALLBACK_PROBE_URL, true, timeout);
            match fallback {
                ProbeResult::Online { .. } => fallback,
                ProbeResult::Redirect {
                    ref location,
                    ref body,
                    ..
                } if looks_like_portal(location, body) => fallback,
                _ => primary,
            }
        }
    }
}

/// `accept_any_success` is used only for the fallback probe, where a normal
/// public page commonly returns 200 rather than 204.
fn probe_once(url: &str, accept_any_success: bool, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let request = probe_agent()
        .get(url)
        .set("Cache-Control", "no-cache")
        .set("User-Agent", "CampusAutoLogin/0.1")
        .timeout(timeout);
    let response = match request.call().or_any_status() {
        Ok(response) => response,
        Err(error) => {
            return ProbeResult::Transport {
                kind: format!("{:?}", error.kind()),
                elapsed_ms: started.elapsed().as_millis(),
            }
        }
    };
    let status = response.status() as u16;
    let location = response.header("Location").unwrap_or_default().to_string();
    let body = read_limited_body(response);
    let elapsed_ms = started.elapsed().as_millis();
    if status == 204 || (accept_any_success && (200..300).contains(&status)) {
        ProbeResult::Online { elapsed_ms }
    } else if (300..400).contains(&status) || looks_like_portal(&location, &body) {
        ProbeResult::Redirect {
            status,
            location,
            body,
            elapsed_ms,
        }
    } else {
        ProbeResult::Http { status, elapsed_ms }
    }
}

fn probe_agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| ureq::AgentBuilder::new().redirects(0).build())
}

fn read_limited_body(response: ureq::Response) -> String {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut bytes)
        .ok();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A captive candidate must contain protocol markers. This prevents a normal
/// public-web redirect from causing campus credentials to be submitted.
pub(crate) fn looks_like_portal(location: &str, body: &str) -> bool {
    let text = format!(
        "{}\n{}",
        location.to_ascii_lowercase(),
        body.to_ascii_lowercase()
    );
    [
        "wlanuserip=",
        "wlanacname=",
        "apartmentid=",
        "roomid=",
        "eportal",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::looks_like_portal;

    #[test]
    fn only_protocol_markers_make_a_redirect_captive() {
        assert!(looks_like_portal(
            "http://10.10.16.101/eportal/login?wlanuserip=10.0.0.2",
            ""
        ));
        assert!(looks_like_portal(
            "",
            "<meta http-equiv=refresh content=0;url=http://gw/eportal?roomId=abc>"
        ));
        assert!(!looks_like_portal("https://www.baidu.com/", ""));
        assert!(!looks_like_portal("", "temporary network error"));
    }
}
