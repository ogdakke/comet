//! Production vs source-build runtime defaults.
//!
//! The original Electron app kept `~/.zeron` + the packaged IPC port for the
//! installed binary and `~/.zeron-dev` + a second port for unpackaged runs.
//! Without that split, `cargo build --bin zeron && ./target/debug/zeron`
//! probes (and may host) the same engine the Mac app is already using.

use std::path::Path;

use zeron_update::InstallKind;

pub use zeron_app::paths::{DEV_IPC_PORT, PACKAGED_IPC_PORT};

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
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn unpackaged_defaults_do_not_collide_with_the_installed_app() {
        assert_eq!(ipc_port_for(&InstallKind::Unmanaged), DEV_IPC_PORT);
        assert_ne!(DEV_IPC_PORT, PACKAGED_IPC_PORT);
    }

    #[test]
    fn packaged_defaults_keep_the_production_port() {
        let mac = InstallKind::MacApp {
            bundle: PathBuf::from("/Applications/Zeron.app"),
        };
        assert_eq!(ipc_port_for(&mac), PACKAGED_IPC_PORT);
        assert_eq!(ipc_port_for(&InstallKind::UserInstalled), PACKAGED_IPC_PORT);
    }
}
