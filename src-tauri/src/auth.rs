use crate::logger::Logger;
use crate::model::{AppConfig, Credentials};
use crate::portal::PortalParams;
use crate::probe;
use std::time::{Duration, Instant};
use url::Url;

const API_LOGIN: &str = "https://api.215123.cn/ac/auth/loginByPhoneAndUid";
const API_OAUTH: &str = "https://api.215123.cn/ac/auth/oauthRedirect";
const CLIENT_ID: &str = "6d6bc6f3b5f04107a5fc1c62e39dd5f4";
const VERIFY_BUDGET: Duration = Duration::from_secs(3);
const VERIFY_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug)]
pub enum AuthError {
    Transport(String),
    Protocol(String),
    VerifyFailed,
}

/// Execute the legacy authentication protocol as an independent Windows
/// implementation. The Python script remains untouched and is not invoked.
pub fn authenticate(
    config: &AppConfig,
    credentials: &Credentials,
    params: &PortalParams,
) -> Result<(), AuthError> {
    authenticate_inner(config, credentials, params, None)
}

/// Variant used by the tray runtime for per-stage diagnostics. Sensitive
/// values are never passed to the logger.
pub fn authenticate_with_logger(
    config: &AppConfig,
    credentials: &Credentials,
    params: &PortalParams,
    logger: &Logger,
) -> Result<(), AuthError> {
    authenticate_inner(config, credentials, params, Some(logger))
}

fn authenticate_inner(
    config: &AppConfig,
    credentials: &Credentials,
    params: &PortalParams,
    logger: Option<&Logger>,
) -> Result<(), AuthError> {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    stage_log(logger, "auth_login_start", "credentials_present=true");
    let login_body = serde_json::json!({
        "phone": credentials.phone,
        "uid": credentials.uid,
        "captchaKey": ""
    });
    let login = agent
        .post(API_LOGIN)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .send_json(login_body)
        .map_err(|e| AuthError::Transport(stable_error(&e)))?;
    // ureq's default agent is stateless. Carry the API session cookies to the
    // OAuth request explicitly, matching requests.Session in auto_login.py.
    let login_cookie_header = cookie_header(login.all("Set-Cookie"));
    let login_json: serde_json::Value = login
        .into_json()
        .map_err(|e| AuthError::Protocol(e.to_string()))?;
    let token = login_json
        .pointer("/data/token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::Protocol("登录接口未返回 token".into()))?;
    stage_log(logger, "auth_token_received", "token_present=true");

    let inner_url = build_inner_url(config, params)?;
    let redirect_uri = encode_legacy_redirect(&inner_url);
    // `redirect_uri` is already form-encoded by `encode_legacy_redirect`.
    // Append it verbatim here; passing it through `query_pairs_mut` again
    // would turn `%253F` into `%25253F` and diverge from the Python client.
    let oauth_url = format!(
        "{API_OAUTH}?response_type={}&client_id={}&redirect_uri={}&serviceName={}",
        encode_component("code"),
        encode_component(CLIENT_ID),
        redirect_uri,
        encode_component(&config.service_name),
    );
    let mut oauth_request = agent
        .get(&oauth_url)
        .set("satoken", token)
        .timeout(Duration::from_secs(15));
    if !login_cookie_header.is_empty() {
        oauth_request = oauth_request.set("Cookie", &login_cookie_header);
    }
    let oauth = oauth_request
        .call()
        .map_err(|e| AuthError::Transport(stable_error(&e)))?;
    stage_log(logger, "auth_oauth_response", "status_received=true");
    let session_id = oauth
        .all("Set-Cookie")
        .into_iter()
        .find_map(parse_session_cookie)
        .unwrap_or_default();
    let location = oauth.header("Location").map(str::to_owned);
    let body = oauth.into_string().unwrap_or_default();
    let code = location
        .as_deref()
        .and_then(extract_code)
        .or_else(|| extract_code(&body))
        .ok_or_else(|| AuthError::Protocol("OAuth 响应未返回 code".into()))?;
    stage_log(logger, "auth_code_received", "code_present=true");

    let final_url = build_final_url(config, params, &code)?;
    // The legacy client follows the gateway's final redirect. Keep redirects
    // disabled for probe/OAuth, but allow this bounded gateway hop.
    let final_agent = ureq::AgentBuilder::new().redirects(5).build();
    let mut final_request = final_agent
        .get(final_url.as_str())
        .timeout(Duration::from_secs(20));
    if !session_id.is_empty() {
        final_request = final_request.set("Cookie", &format!("JSESSIONID={session_id}"));
    }
    final_request
        .call()
        .map_err(|e| AuthError::Transport(stable_error(&e)))?;
    stage_log(logger, "auth_final_request", "request_sent=true");

    if verify_online(config) {
        stage_log(logger, "auth_verify", "verified=true");
        Ok(())
    } else {
        stage_log(logger, "auth_verify", "verified=false");
        Err(AuthError::VerifyFailed)
    }
}

/// Verify as soon as the gateway takes effect. The old fixed three-second
/// sleep made every successful login wait the same amount of time even when
/// the route was already usable. Short probes with exponential spacing return
/// immediately on success and stay bounded during a slow route change.
fn verify_online(config: &AppConfig) -> bool {
    let deadline = Instant::now() + VERIFY_BUDGET;
    let mut delay = Duration::from_millis(100);
    loop {
        if matches!(
            probe::probe_with_timeout(config, VERIFY_PROBE_TIMEOUT),
            probe::ProbeResult::Online { .. }
        ) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(delay.min(remaining));
        delay = (delay + delay).min(Duration::from_millis(800));
    }
}

fn stage_log(logger: Option<&Logger>, event: &str, detail: &str) {
    if let Some(logger) = logger {
        logger.event("INFO", event, detail);
    }
}

fn build_inner_url(config: &AppConfig, params: &PortalParams) -> Result<String, AuthError> {
    let mut url = Url::parse(&config.gateway).map_err(|e| AuthError::Protocol(e.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("wlanuserip", &params.wlan_user_ip);
        query.append_pair("wlanacname", &params.wlan_ac_name);
        query.append_pair("ssid", "");
        query.append_pair("nasip", &params.nas_ip);
        query.append_pair("snmpagentip", "");
        query.append_pair("mac", &params.mac);
        query.append_pair("t", "wireless-v2");
        query.append_pair("url", &params.original_url);
        query.append_pair("apmac", "");
        query.append_pair("nasid", &params.nas_id);
        query.append_pair("vid", &params.vid);
        query.append_pair("port", &params.port);
        query.append_pair("nasportid", &params.nas_port_id);
    }
    Ok(url.into())
}

fn build_final_url(
    config: &AppConfig,
    params: &PortalParams,
    code: &str,
) -> Result<Url, AuthError> {
    let mut url = Url::parse(&build_inner_url(config, params)?)
        .map_err(|e| AuthError::Protocol(e.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("code", code);
        query.append_pair("serviceName", &config.service_name);
        query.append_pair("apartmentId", &config.apartment_id);
        query.append_pair("roomId", &config.room_id);
    }
    Ok(url)
}

/// Preserve the existing script's historical transform: replace `?` by the
/// literal `%3F`, then form-encode the complete redirect URI. Real campus
/// fixtures should be used before changing this compatibility behavior.
fn encode_legacy_redirect(inner_url: &str) -> String {
    url::form_urlencoded::byte_serialize(inner_url.replace('?', "%3F").as_bytes()).collect()
}

fn encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn parse_session_cookie(header: &str) -> Option<String> {
    header
        .split(';')
        .find_map(|part| part.trim().strip_prefix("JSESSIONID="))
        .map(str::to_string)
}

fn cookie_header(headers: Vec<&str>) -> String {
    headers
        .into_iter()
        .filter_map(|header| header.split(';').next())
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            if name.trim().is_empty() || value.trim().is_empty() {
                return None;
            }
            Some(format!("{}={}", name.trim(), value.trim()))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn extract_code(value: &str) -> Option<String> {
    if let Ok(url) = Url::parse(value) {
        if let Some(code) = url.query_pairs().find_map(|(key, value)| {
            (key == "code" && !value.is_empty()).then(|| value.into_owned())
        }) {
            return Some(code);
        }
    }
    // OAuth servers sometimes return a relative Location (`/callback?code=`)
    // rather than an absolute URL. Parse that query without requiring a host.
    let query = value
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or(value);
    let query = query
        .split_once('#')
        .map(|(query, _)| query)
        .unwrap_or(query);
    if let Some(code) = url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == "code" && !value.is_empty()).then(|| value.into_owned()))
    {
        return Some(code);
    }
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    find_code_json(&json)
}

fn find_code_json(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key.eq_ignore_ascii_case("code") {
                    if let Some(code) = value.as_str().filter(|s| !s.is_empty()) {
                        return Some(code.to_string());
                    }
                }
                if let Some(code) = find_code_json(value) {
                    return Some(code);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_code_json),
        _ => None,
    }
}

fn stable_error(error: &ureq::Error) -> String {
    format!("{:?}", error.kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_redirect_encoding_matches_python_transform_shape() {
        let encoded = encode_legacy_redirect("http://gw/login?a=b");
        assert!(encoded.contains("http%3A%2F%2Fgw%2Flogin%253Fa%3Db"));
    }

    #[test]
    fn oauth_url_does_not_double_encode_legacy_redirect() {
        let redirect = encode_legacy_redirect("http://gw/login?a=b");
        let url = format!("{API_OAUTH}?redirect_uri={redirect}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Fgw%2Flogin%253Fa%3Db"));
        assert!(!url.contains("%25253F"));
    }

    #[test]
    fn extracts_code_from_location_and_json_body() {
        assert_eq!(
            extract_code("http://gw/cb?code=abc%20123"),
            Some("abc 123".into())
        );
        assert_eq!(
            extract_code("/callback?code=relative"),
            Some("relative".into())
        );
        assert_eq!(
            extract_code(r#"{"data":{"code":"json-code"}}"#),
            Some("json-code".into())
        );
        assert_eq!(extract_code("not-a-code"), None);
    }

    #[test]
    fn parses_only_jsessionid_cookie_value() {
        assert_eq!(
            parse_session_cookie("JSESSIONID=abc123; Path=/"),
            Some("abc123".into())
        );
        assert_eq!(parse_session_cookie("foo=bar; Path=/"), None);
        assert_eq!(
            cookie_header(vec!["A=1; Path=/", "B=2; HttpOnly"]),
            "A=1; B=2"
        );
    }
}
