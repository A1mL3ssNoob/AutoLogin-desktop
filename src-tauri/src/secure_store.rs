use crate::model::Credentials;
use crate::paths;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::fs;
use std::io;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    InvalidData,
    Platform(String),
}

impl From<io::Error> for StoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn save(credentials: &Credentials) -> Result<(), StoreError> {
    fs::create_dir_all(paths::data_dir())?;
    let plaintext = serde_json::to_vec(credentials).map_err(|_| StoreError::InvalidData)?;
    let protected = protect(&plaintext)?;
    fs::write(paths::credentials_path(), STANDARD.encode(protected))?;
    Ok(())
}

pub fn load() -> Result<Option<Credentials>, StoreError> {
    let path = paths::credentials_path();
    if !path.exists() {
        return Ok(None);
    }
    let encoded = fs::read_to_string(path)?;
    let encrypted = STANDARD
        .decode(encoded.trim())
        .map_err(|_| StoreError::InvalidData)?;
    let plaintext = unprotect(&encrypted)?;
    serde_json::from_slice(&plaintext)
        .map(Some)
        .map_err(|_| StoreError::InvalidData)
}

#[cfg(windows)]
fn protect(input: &[u8]) -> Result<Vec<u8>, StoreError> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &mut input_blob,
            null(),
            null(),
            null_mut(),
            null_mut(),
            0,
            &mut output_blob,
        )
    };
    if ok == 0 || output_blob.pbData.is_null() {
        return Err(StoreError::Platform("CryptProtectData failed".into()));
    }
    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };
    unsafe {
        LocalFree(output_blob.pbData as _);
    }
    Ok(result)
}

#[cfg(not(windows))]
fn protect(input: &[u8]) -> Result<Vec<u8>, StoreError> {
    Ok(input.to_vec())
}

#[cfg(windows)]
fn unprotect(input: &[u8]) -> Result<Vec<u8>, StoreError> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &mut input_blob,
            null_mut(),
            null(),
            null_mut(),
            null_mut(),
            0,
            &mut output_blob,
        )
    };
    if ok == 0 || output_blob.pbData.is_null() {
        return Err(StoreError::Platform("CryptUnprotectData failed".into()));
    }
    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };
    unsafe {
        LocalFree(output_blob.pbData as _);
    }
    Ok(result)
}

#[cfg(not(windows))]
fn unprotect(input: &[u8]) -> Result<Vec<u8>, StoreError> {
    Ok(input.to_vec())
}
