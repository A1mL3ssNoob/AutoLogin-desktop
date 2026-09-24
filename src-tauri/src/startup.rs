use crate::config;
use crate::model::AppConfig;

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let mut value = config::load_config()?;
    value.auto_start = enabled;
    config::save_config(&value)?;
    #[cfg(windows)]
    write_run_key(enabled)?;
    Ok(())
}

#[cfg(windows)]
fn write_run_key(enabled: bool) -> Result<(), String> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let key_name: Vec<u16> =
        std::ffi::OsStr::new("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .encode_wide()
            .chain(once(0))
            .collect();
    let value_name: Vec<u16> = std::ffi::OsStr::new("CampusAutoLogin")
        .encode_wide()
        .chain(once(0))
        .collect();
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(format!("registry open failed: {status}"));
    }
    let result = if enabled {
        let exe: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        // Keep automatic startup in the tray. A manual launch without this
        // flag always opens the control panel, even when credentials exist.
        let command = format!("\"{}\" --background", exe.display());
        let data: Vec<u16> = std::ffi::OsStr::new(&command)
            .encode_wide()
            .chain(once(0))
            .collect();
        let status = unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        if status != 0 {
            Err(format!("registry write failed: {status}"))
        } else {
            Ok(())
        }
    } else {
        let status = unsafe { RegDeleteValueW(key, value_name.as_ptr()) };
        if status != 0 && status != 2 {
            Err(format!("registry delete failed: {status}"))
        } else {
            Ok(())
        }
    };
    unsafe {
        RegCloseKey(key);
    }
    result
}

#[allow(dead_code)]
fn _keep_config_type(_: AppConfig) {}
