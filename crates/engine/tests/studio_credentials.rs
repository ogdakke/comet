//! Studio provider secrets on machines without a working platform store
//! (headless Macs and Linux servers), and from the engine's environment.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zeron_engine::{
    FallbackStudioSecretBackend, StudioCredentialError, StudioCredentials, StudioSecretBackend,
    studio_environment_variable,
};
use zeron_proto::ProviderValidationState;
use zeron_studio::{ProviderId, Secret};

/// A platform store that works, or one that fails like a locked keychain.
#[derive(Default)]
struct Platform {
    available: bool,
    secrets: Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl StudioSecretBackend for Platform {
    async fn set(&self, id: &ProviderId, secret: &Secret) -> Result<(), StudioCredentialError> {
        if !self.available {
            return Err(StudioCredentialError::SecretStore(
                "User interaction is not allowed.".into(),
            ));
        }
        self.secrets
            .lock()
            .unwrap()
            .insert(id.as_str().into(), secret.expose().into());
        Ok(())
    }

    async fn get(&self, id: &ProviderId) -> Result<Secret, StudioCredentialError> {
        self.secrets
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|s| Secret::new(s.clone()))
            .ok_or(StudioCredentialError::NotConfigured)
    }

    async fn remove(&self, id: &ProviderId) -> Result<(), StudioCredentialError> {
        self.secrets.lock().unwrap().remove(id.as_str());
        Ok(())
    }
}

fn venice() -> ProviderId {
    ProviderId::new("venice")
}

fn credentials(root: &std::path::Path, platform: Arc<Platform>) -> StudioCredentials {
    let backend = FallbackStudioSecretBackend::new(platform, root.join("secrets"));
    StudioCredentials::with_backend(root, Arc::new(backend))
        .unwrap()
        .with_environment(|_| None)
}

#[tokio::test]
async fn without_a_platform_store_the_secret_goes_to_an_owner_only_file() {
    let root = tempfile::tempdir().unwrap();
    let credentials = credentials(root.path(), Arc::new(Platform::default()));

    let connection = credentials
        .set(venice(), "Venice".into(), Secret::new("venice-key"))
        .await
        .unwrap();
    assert!(connection.configured);
    assert_eq!(
        credentials.secret(&venice()).await.unwrap().expose(),
        "venice-key"
    );

    let file = root.path().join("secrets").join("venice.key");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "venice-key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&root.path().join("secrets")), 0o700);
    }

    credentials.remove(&venice()).await.unwrap();
    assert!(!file.exists());
    assert!(credentials.connection(&venice()).unwrap().is_none());
}

#[tokio::test]
async fn a_working_platform_store_wins_and_drops_the_file_copy() {
    let root = tempfile::tempdir().unwrap();
    let platform = Arc::new(Platform::default());
    credentials(root.path(), platform.clone())
        .set(venice(), "Venice".into(), Secret::new("old-key"))
        .await
        .unwrap();
    let file = root.path().join("secrets").join("venice.key");
    assert!(file.exists());

    // The keychain unlocks (a GUI session starts): the next save moves there.
    let platform = Arc::new(Platform {
        available: true,
        ..Platform::default()
    });
    let credentials = credentials(root.path(), platform.clone());
    credentials
        .set(venice(), "Venice".into(), Secret::new("new-key"))
        .await
        .unwrap();
    assert!(!file.exists());
    assert_eq!(platform.secrets.lock().unwrap()["venice"], "new-key");
    assert_eq!(
        credentials.secret(&venice()).await.unwrap().expose(),
        "new-key"
    );
}

#[tokio::test]
async fn the_environment_configures_a_provider_and_overrides_the_store() {
    assert_eq!(
        studio_environment_variable(&venice()),
        "ZERON_STUDIO_VENICE_KEY"
    );
    let root = tempfile::tempdir().unwrap();
    let platform = Arc::new(Platform {
        available: true,
        ..Platform::default()
    });
    let backend = FallbackStudioSecretBackend::new(platform, root.path().join("secrets"));
    let credentials = StudioCredentials::with_backend(root.path(), Arc::new(backend))
        .unwrap()
        .with_environment(|name| (name == "ZERON_STUDIO_VENICE_KEY").then(|| " env-key\n".into()));

    // Nothing saved: the environment alone makes it configured and usable.
    assert!(
        credentials
            .connection(&venice())
            .unwrap()
            .unwrap()
            .configured
    );
    assert_eq!(
        credentials.secret(&venice()).await.unwrap().expose(),
        "env-key"
    );
    let validated = credentials
        .record_validation(venice(), ProviderValidationState::Valid, None)
        .unwrap();
    assert_eq!(validated.validation_state, ProviderValidationState::Valid);

    credentials
        .set(venice(), "Venice".into(), Secret::new("stored-key"))
        .await
        .unwrap();
    assert_eq!(
        credentials.secret(&venice()).await.unwrap().expose(),
        "env-key"
    );
}
