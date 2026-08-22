//! Production vs source-build runtime defaults.
//!
//! The original Electron app kept `~/.zeron` + the packaged IPC port for the
//! installed binary and `~/.zeron-dev` + a second port for unpackaged runs.
//! Without that split, `cargo build --bin zeron && ./target/debug/zeron`
//! probes (and may host) the same engine the Mac app is already using.

use std::path::{Path, PathBuf};

use zeron_update::InstallKind;

/// Localhost IPC port used by `/Applications/Zeron.app` and the curl|sh tree.
pub const PACKAGED_IPC_PORT: u16 = 27654;
/// Isolated port for source / `target/debug` / `target/release` binaries.
pub const DEV_IPC_PORT: u16 = 27634;

pub fn install_kind() -> InstallKind {
    zeron_update::detect_install()
}

pub fn ipc_port() -> u16 {
    ipc_port_for(&install_kind())
}

pub fn ipc_port_for(kind: &InstallKind) -> u16 {
    if kind.uses_production_runtime() {
        PACKAGED_IPC_PORT
    } else {
        DEV_IPC_PORT
    }
}

pub fn data_dir() -> PathBuf {
    data_dir_for(&install_kind(), &home_dir())
}

pub fn data_dir_for(kind: &InstallKind, home: &Path) -> PathBuf {
    if kind.uses_production_runtime() {
        let dir = home.join(".zeron");
        migrate_legacy_comet_native(&dir);
        dir
    } else {
        home.join(".zeron-dev")
    }
}

fn home_dir() -> PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME not set"))
}

/// One-shot 0.2.0 migration: adopt the pre-rename data dir (sign-in,
/// device identity, prefs) instead of starting fresh. Packaged installs only
/// — a source build must not inherit production state.
fn migrate_legacy_comet_native(dir: &Path) {
    if dir.exists() {
        return;
    }
    let Some(home) = dir.parent() else {
        return;
    };
    let old = home.join(".comet-native");
    if old.exists() && std::fs::rename(&old, dir).is_ok() {
        eprintln!("migrated data dir {} -> {}", old.display(), dir.display());
    }
}

pub fn log_isolation(data_dir: &Path, ipc_port: u16) {
    if install_kind().uses_production_runtime() {
        return;
    }
    tracing::info!(
        data_dir = %data_dir.display(),
        ipc_port,
        packaged_data_dir = "~/.zeron",
        packaged_ipc_port = PACKAGED_IPC_PORT,
        "unpackaged build; isolated from the installed app"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpackaged_defaults_do_not_collide_with_the_installed_app() {
        let home = Path::new("/Users/dev");
        assert_eq!(
            data_dir_for(&InstallKind::Unmanaged, home),
            PathBuf::from("/Users/dev/.zeron-dev")
        );
        assert_eq!(ipc_port_for(&InstallKind::Unmanaged), DEV_IPC_PORT);
        assert_ne!(DEV_IPC_PORT, PACKAGED_IPC_PORT);
    }

    #[test]
    fn packaged_defaults_keep_the_production_home() {
        let home = Path::new("/Users/dev");
        let mac = InstallKind::MacApp {
            bundle: PathBuf::from("/Applications/Zeron.app"),
        };
        let managed = InstallKind::Managed {
            app_root: PathBuf::from("/Users/dev/.zeron/app"),
        };
        assert_eq!(data_dir_for(&mac, home), PathBuf::from("/Users/dev/.zeron"));
        assert_eq!(
            data_dir_for(&managed, home),
            PathBuf::from("/Users/dev/.zeron")
        );
        assert_eq!(ipc_port_for(&mac), PACKAGED_IPC_PORT);
        assert_eq!(ipc_port_for(&managed), PACKAGED_IPC_PORT);
        assert_eq!(
            data_dir_for(&InstallKind::UserInstalled, home),
            PathBuf::from("/Users/dev/.zeron")
        );
        assert_eq!(ipc_port_for(&InstallKind::UserInstalled), PACKAGED_IPC_PORT);
    }
}
