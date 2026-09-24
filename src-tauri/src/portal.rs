use crate::model::AppConfig;
use crate::probe::ProbeResult;
use url::Url;

const MAX_VALUE_LEN: usize = 512;

#[derive(Clone, Debug, Default)]
pub struct PortalParams {
    pub wlan_user_ip: String,
    pub wlan_ac_name: String,
    pub nas_ip: String,
    pub mac: String,
    pub original_url: String,
    pub nas_id: String,
    pub vid: String,
    pub port: String,
    pub nas_port_id: String,
}

#[derive(Clone, Debug)]
pub struct CaptivePortal {
    pub params: PortalParams,
    pub status: u16,
    /// Safe for diagnostics: scheme/host/path only, with query and fragment
    /// removed. Never retain a complete Location URL in a log record.
    pub location: String,
}

/// Return whether an absolute URL belongs to the configured campus gateway.
/// This helper is also used by the capture window so arbitrary external URLs
/// containing apartmentId/roomId cannot write configuration.
pub fn is_portal_url(config: &AppConfig, value: &str) -> bool {
    let Some(expected) = configured_portal_host(config) else {
        return false;
    };
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.host_str()
        .map(|host| host.eq_ignore_ascii_case(&expected))
        .unwrap_or(false)
}

/// Classify a probe response as the configured campus portal. A redirect is
/// accepted only when its URL host matches the configured gateway and includes
/// usable dynamic parameters, or when the body carries protocol markers.
pub fn classify(config: &AppConfig, result: &ProbeResult) -> Option<CaptivePortal> {
    let ProbeResult::Redirect {
        status,
        location,
        body,
        ..
    } = result
    else {
        return None;
    };
    let candidates = candidate_urls(location, body);
    let expected_host = configured_portal_host(config);
    for candidate in &candidates {
        if !host_allowed(candidate, expected_host.as_deref()) {
            continue;
        }
        let params = extract_params(candidate);
        if !params.wlan_user_ip.is_empty() {
            return Some(CaptivePortal {
                params,
                status: *status,
                location: safe_location(candidate),
            });
        }
    }

    // Some portals put key=value pairs directly in an HTML/JavaScript body
    // rather than inside an absolute URL. Parse that body as a final source,
    // using the configured gateway host for host validation.
    if body_has_allowed_host(body, expected_host.as_deref()) {
        let params = extract_params(body);
        if !params.wlan_user_ip.is_empty() {
            return Some(CaptivePortal {
                params,
                status: *status,
                location: expected_host
                    .map(|host| format!("http://{host}"))
                    .unwrap_or_default(),
            });
        }
    }
    None
}

/// Extract the two required room fields from any request URL. The path is not
/// inspected, so names such as `login_sso.jsp` remain implementation details of
/// one campus deployment rather than protocol requirements.
pub fn extract_room_binding(value: &str) -> Option<(String, String)> {
    let parsed = Url::parse(value).ok()?;
    let mut apartment: Option<String> = None;
    let mut room: Option<String> = None;
    for (key, val) in parsed.query_pairs() {
        let val = val.into_owned();
        if !valid_id(&val) {
            continue;
        }
        match key.as_ref() {
            "apartmentId" => {
                if !set_unique(&mut apartment, val) {
                    return None;
                }
            }
            "roomId" => {
                if !set_unique(&mut room, val) {
                    return None;
                }
            }
            _ => {}
        }
    }
    match (apartment, room) {
        (Some(apartment_id), Some(room_id)) => Some((apartment_id, room_id)),
        _ => None,
    }
}

/// Same extraction with an optional host allow-list for the capture window.
/// The path is intentionally ignored; only the campus gateway host is trusted.
pub fn extract_room_binding_for_host(value: &str, expected_host: &str) -> Option<(String, String)> {
    let parsed = Url::parse(value).ok()?;
    if !expected_host.trim().is_empty()
        && !parsed
            .host_str()
            .map(|host| host.eq_ignore_ascii_case(expected_host.trim()))
            .unwrap_or(false)
    {
        return None;
    }
    extract_room_binding(value)
}

fn set_unique(slot: &mut Option<String>, value: String) -> bool {
    match slot {
        Some(existing) => existing == &value,
        None => {
            *slot = Some(value);
            true
        }
    }
}

fn configured_portal_host(config: &AppConfig) -> Option<String> {
    let configured = config.portal_host.trim();
    if !configured.is_empty() {
        return Some(configured.trim_matches('/').to_ascii_lowercase());
    }
    Url::parse(&config.gateway)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

fn host_allowed(candidate: &str, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Ok(url) = Url::parse(candidate) else {
        return true;
    };
    url.host_str()
        .map(|host| host.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn body_has_allowed_host(body: &str, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    candidate_urls("", body)
        .iter()
        .any(|url| host_allowed(url, Some(expected)))
        || !body.contains("http://") && !body.contains("https://")
}

/// Find absolute URLs in a Location header or an HTML meta/JavaScript body.
/// This deliberately does not require any particular pathname.
fn candidate_urls(location: &str, body: &str) -> Vec<String> {
    let mut result = Vec::new();
    if Url::parse(location).is_ok() {
        result.push(location.to_string());
    }
    let lower = body.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative) = lower[offset..]
        .find("http://")
        .or_else(|| lower[offset..].find("https://"))
    {
        let start = offset + relative;
        let tail = &body[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .unwrap_or(tail.len());
        let candidate = tail[..end].replace("&amp;", "&");
        if Url::parse(&candidate).is_ok() && !result.iter().any(|old| old == &candidate) {
            result.push(candidate);
        }
        offset = start.saturating_add(end.max(1));
        if offset >= body.len() {
            break;
        }
    }
    result
}

fn safe_location(value: &str) -> String {
    let Ok(url) = Url::parse(value) else {
        return String::new();
    };
    let host = url.host_str().unwrap_or_default();
    if host.is_empty() {
        return url.path().to_string();
    }
    format!("{}://{}{}", url.scheme(), host, url.path())
}

fn extract_params(source: &str) -> PortalParams {
    let mut params = PortalParams::default();
    for (key, value) in query_pairs(source) {
        match key.as_str() {
            "wlanuserip" => params.wlan_user_ip = value,
            "wlanacname" => params.wlan_ac_name = value,
            "nasip" => params.nas_ip = value,
            "mac" => params.mac = value,
            "url" => params.original_url = value,
            "nasid" => params.nas_id = value,
            "vid" => params.vid = value,
            "port" => params.port = value,
            "nasportid" => params.nas_port_id = value,
            _ => {}
        }
    }
    params
}

fn query_pairs(source: &str) -> Vec<(String, String)> {
    if let Ok(url) = Url::parse(source) {
        return url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    }
    let source = source.replace("&amp;", "&");
    let mut pairs = Vec::new();
    for key in [
        "wlanuserip",
        "wlanacname",
        "nasip",
        "mac",
        "url",
        "nasid",
        "vid",
        "port",
        "nasportid",
    ] {
        let marker = format!("{key}=");
        let mut start = 0;
        while let Some(found) = source[start..].find(&marker) {
            let at = start + found;
            if at > 0
                && (source.as_bytes()[at - 1].is_ascii_alphanumeric()
                    || source.as_bytes()[at - 1] == b'_')
            {
                start = at + marker.len();
                continue;
            }
            let tail = &source[at + marker.len()..];
            let end = tail
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | '"' | '\'' | '<' | '>'))
                .unwrap_or(tail.len());
            let raw = &tail[..end];
            let decoded = url::form_urlencoded::parse(format!("{key}={raw}").as_bytes())
                .next()
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            if decoded.len() <= MAX_VALUE_LEN {
                pairs.push((key.to_string(), decoded));
            }
            start = at + marker.len() + end.max(1);
            if start >= source.len() {
                break;
            }
        }
    }
    pairs
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_binding_does_not_depend_on_path_name() {
        let url = "http://gw.example/anything?roomId=room-7&apartmentId=apt.3&ignored=x";
        assert_eq!(
            extract_room_binding(url),
            Some(("apt.3".into(), "room-7".into()))
        );
    }

    #[test]
    fn room_binding_rejects_missing_or_conflicting_values() {
        assert_eq!(extract_room_binding("http://gw/path?roomId=r"), None);
        assert_eq!(
            extract_room_binding("http://gw/path?apartmentId=a&apartmentId=b&roomId=r"),
            None
        );
        assert_eq!(
            extract_room_binding("http://gw/path?apartmentId=a%2E1&roomId=r_2"),
            Some(("a.1".into(), "r_2".into()))
        );
    }

    #[test]
    fn parses_js_or_meta_redirect_body_without_fixed_path() {
        let config = AppConfig {
            portal_host: "10.10.16.101".into(),
            ..AppConfig::defaults()
        };
        let response = ProbeResult::Redirect {
            status: 200,
            location: String::new(),
            body: "<script>window.location='http://10.10.16.101/auth/go?wlanuserip=10.0.0.5&nasip=n';</script>".into(),
            elapsed_ms: 1,
        };
        let portal = classify(&config, &response).expect("portal");
        assert_eq!(portal.params.wlan_user_ip, "10.0.0.5");
        assert_eq!(portal.params.nas_ip, "n");
        assert_eq!(portal.location, "http://10.10.16.101/auth/go");
    }

    #[test]
    fn portal_host_guard_rejects_external_capture_urls() {
        let config = AppConfig {
            portal_host: "10.10.16.101".into(),
            ..AppConfig::defaults()
        };
        assert!(is_portal_url(&config, "http://10.10.16.101/auth"));
        assert!(!is_portal_url(
            &config,
            "https://evil.example/auth?apartmentId=a&roomId=r"
        ));
    }

    #[test]
    fn query_values_are_percent_decoded() {
        let params =
            extract_params("http://gw/path?wlanuserip=10.0.0.1&url=http%3A%2F%2Fexample.test%2F");
        assert_eq!(params.original_url, "http://example.test/");
    }
}
