//! Explicit, one-shot Studio catalog transfer for a personal deployment.

use std::{path::PathBuf, sync::Arc};

use zeron_engine::{
    AuthState, EdgeConfig, Engine, EngineConfig, InstanceLock, StudioStore, StudioSync,
    WorkspaceScope, studio::DEFAULT_MAX_ARTIFACT_BYTES,
};

/// Copy `profiles/local/studio` to the active signed-in profile and publish it.
/// This command refuses any ambiguous target. It never runs as a side effect of
/// starting Zeron, so development and production accounts stay independent.
pub async fn import_local(config: EngineConfig, source: Option<PathBuf>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = InstanceLock::acquire(&config.data_dir).map_err(|error| {
        anyhow::anyhow!(
            "{error}\nCannot import Studio while an engine is running. Quit Zeron or stop `zeron daemon` first."
        )
    })?;
    let auth = Engine::build_auth(&config).await;
    if !auth.workos_enabled() {
        anyhow::bail!("Studio import requires ZERON_EDGE_URL and ZERON_WORKOS_CLIENT_ID.");
    }
    if Engine::initial_workspace_scope(&auth) != WorkspaceScope::Synced {
        anyhow::bail!("Studio import requires an existing WorkOS login. Run `zeron login` first.");
    }
    if !matches!(
        auth.state(),
        AuthState::SignedIn {
            org_id: Some(_),
            ..
        }
    ) {
        anyhow::bail!(
            "Studio import requires a selected WorkOS workspace. Run `zeron login` first."
        );
    }
    let profile =
        Engine::resolve_profile(&config, &auth, WorkspaceScope::Synced)?.ok_or_else(|| {
            anyhow::anyhow!("Studio import could not resolve the signed-in workspace.")
        })?;
    let source = source.unwrap_or_else(|| {
        config
            .data_dir
            .join("profiles")
            .join("local")
            .join("studio")
    });
    let device = Engine::engine_info(&config, WorkspaceScope::Synced)?;
    let store = Arc::new(StudioStore::open(
        profile.store_root(),
        DEFAULT_MAX_ARTIFACT_BYTES,
    )?);
    let device_id = device.device_id;
    let edge = EdgeConfig::new(config.edge_url, Arc::new(auth)).with_device(device_id.clone());
    let sync = StudioSync::new(store, edge, profile.org_id(), device_id);
    let outcome = sync.import_local_catalog(&source).await?;
    println!("Imported {} and published {outcome:?}.", source.display());
    Ok(())
}
