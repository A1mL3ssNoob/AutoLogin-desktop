use crate::model::{is_valid_service_name, AppConfig, ConfigView, Credentials};
use crate::{paths, secure_store};
use std::fs;

pub fn load_config() -> Result<AppConfig, String> {
    let path = paths::config_path();
    if !path.exists() {
        return Ok(AppConfig::defaults());
    }
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut config: AppConfig = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    // Treat an empty value from an old or hand-edited file the same as a
    // missing field. This keeps existing installations usable while the
    // carrier selector is introduced.
    if config.service_name.trim().is_empty() {
        config.service_name = AppConfig::defaults().service_name;
    }
    Ok(config)
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    if !is_valid_service_name(&config.service_name) {
        return Err("运营商必须选择中国电信、中国联通或中国移动".into());
    }
    fs::create_dir_all(paths::data_dir()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(paths::config_path(), text).map_err(|e| e.to_string())
}

pub fn read_view() -> Result<ConfigView, String> {
    let config = load_config()?;
    let has_credentials = secure_store::load()
        .map_err(|e| format!("credential store: {e:?}"))?
        .map(|c| !c.phone.is_empty() && !c.uid.is_empty())
        .unwrap_or(false);
    Ok(ConfigView {
        config,
        has_credentials,
    })
}

pub fn save_credentials(credentials: &Credentials) -> Result<(), String> {
    if credentials.phone.trim().is_empty() || credentials.uid.trim().is_empty() {
        return Err("手机号和 UID/认证码不能为空".into());
    }
    secure_store::save(credentials).map_err(|e| format!("credential store: {e:?}"))
}

pub fn load_credentials() -> Result<Option<Credentials>, String> {
    secure_store::load().map_err(|e| format!("credential store: {e:?}"))
}
