use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub apartment_id: String,
    pub room_id: String,
    /// Internal carrier identifier expected by the campus gateway.
    ///
    /// Older configuration files predate the carrier selector, so keep a
    /// serde default here as well as the application default below.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    pub gateway: String,
    pub check_url: String,
    pub portal_host: String,
    pub auto_start: bool,
}

pub const SERVICE_NAMES: [&str; 3] = ["chinaTelecom", "chinaUnicom", "chinaMobile"];

fn default_service_name() -> String {
    "chinaTelecom".into()
}

pub fn is_valid_service_name(value: &str) -> bool {
    SERVICE_NAMES.contains(&value)
}

impl AppConfig {
    pub fn defaults() -> Self {
        Self {
            apartment_id: String::new(),
            room_id: String::new(),
            service_name: default_service_name(),
            gateway: "http://10.10.16.101:8080/eportal/login_sso.jsp".into(),
            check_url: "http://connect.rom.miui.com/generate_204".into(),
            portal_host: "10.10.16.101".into(),
            auto_start: false,
        }
    }

    pub fn has_room_binding(&self) -> bool {
        !self.apartment_id.trim().is_empty() && !self.room_id.trim().is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub phone: String,
    pub uid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigView {
    pub config: AppConfig,
    pub has_credentials: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    SetupRequired,
    Online,
    Checking,
    ProbeFailed { consecutive: u8 },
    PortalDetected,
    Authenticating { attempt: u8 },
    WaitingToRetry { seconds: u64 },
    Offline,
    Paused,
    NeedsAttention,
}

impl Default for AppStatus {
    fn default() -> Self {
        Self::SetupRequired
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusView {
    pub status: AppStatus,
    pub detail: String,
    /// Whether the last known-good connection is being checked again. This is
    /// separate from `status` so the UI can show progress without turning a
    /// still-connected green state into an outage warning.
    pub checking: bool,
    pub last_check: Option<String>,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureResult {
    pub apartment_id: String,
    pub room_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_defaults_for_legacy_config() {
        let value: AppConfig = serde_json::from_str(
            r#"{
                "apartment_id":"apt",
                "room_id":"room",
                "gateway":"http://gw/login",
                "check_url":"http://check/",
                "portal_host":"gw",
                "auto_start":false
            }"#,
        )
        .expect("legacy config should deserialize");
        assert_eq!(value.service_name, "chinaTelecom");
    }

    #[test]
    fn service_name_whitelist_matches_campus_values() {
        assert!(is_valid_service_name("chinaTelecom"));
        assert!(is_valid_service_name("chinaUnicom"));
        assert!(is_valid_service_name("chinaMobile"));
        assert!(!is_valid_service_name("telecom"));
        assert!(!is_valid_service_name(""));
    }
}
