//! Stable local identity shared by the desktop UI and the background service.
//!
//! The token lives beside the user's private GnomeAI state rather than in the
//! Debian unit, so every desktop account gets an independent credential and a
//! package upgrade never invalidates an active WhatsApp connection.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow};

const TOKEN_FILE: &str = "native-service.token";

pub fn load_or_create_token(app_home: &Path) -> anyhow::Result<String> {
    fs::create_dir_all(app_home)
        .with_context(|| format!("failed to create {}", app_home.display()))?;
    fs::set_permissions(app_home, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", app_home.display()))?;
    let path = token_path(app_home);

    if path.exists() {
        return read_token(&path);
    }

    let token = new_token();
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(token)
        }
        // The UI and the user service may start together at login. Whichever
        // loses the create-new race must reuse the winner's credential.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_token(&path),
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

fn token_path(app_home: &Path) -> PathBuf {
    app_home.join(TOKEN_FILE)
}

fn read_token(path: &Path) -> anyhow::Result<String> {
    let token = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .trim()
        .to_string();
    if token.len() < 32 || !token.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "{} is not a valid native-service token; remove it and restart GnomeAI-RS",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to protect {}", path.display()))?;
    Ok(token)
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_private_and_stable() {
        let directory = std::env::temp_dir().join(format!(
            "gnomeai-native-service-token-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let first = load_or_create_token(&directory).unwrap();
        let second = load_or_create_token(&directory).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(
            fs::metadata(token_path(&directory))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
