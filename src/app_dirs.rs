use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};

/// Resolve one writable, per-user state directory for both the terminal agent
/// and the native desktop runtime. The selected workspace remains independent
/// from this path.
pub fn resolve_app_home(launch_dir: &Path) -> anyhow::Result<PathBuf> {
    let app_home = if let Some(explicit) = std::env::var_os("GNOMEF_RS_HOME") {
        PathBuf::from(explicit)
    } else if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state_home).join("gnomeai-rs")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("gnomeai-rs")
    } else {
        bail!("cannot determine a writable user state directory; set GNOMEF_RS_HOME explicitly");
    };

    fs::create_dir_all(&app_home)
        .with_context(|| format!("cannot create user state directory {}", app_home.display()))?;
    fs::set_permissions(&app_home, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect user state directory {}", app_home.display()))?;
    migrate_legacy_config(launch_dir, &app_home)?;
    Ok(app_home)
}

fn migrate_legacy_config(launch_dir: &Path, app_home: &Path) -> anyhow::Result<()> {
    let destination = app_home.join("config.json");
    if destination.exists() {
        return Ok(());
    }

    let legacy = launch_dir.join("config.json");
    if legacy.exists() && legacy != destination {
        fs::copy(&legacy, &destination).with_context(|| {
            format!(
                "cannot migrate {} to {}",
                legacy.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}
