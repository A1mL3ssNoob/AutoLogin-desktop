use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("APPDATA") {
        return PathBuf::from(value).join("CampusAutoLogin");
    }
    PathBuf::from(".").join("CampusAutoLogin")
}

pub fn local_data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(value).join("CampusAutoLogin");
    }
    data_dir()
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn credentials_path() -> PathBuf {
    data_dir().join("credentials.bin")
}

pub fn log_dir() -> PathBuf {
    local_data_dir().join("logs")
}
