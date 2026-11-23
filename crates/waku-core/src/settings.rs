//! Daemon-owned, user-editable configuration.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;
pub use waku_protocol::settings::DaemonSettings;
use waku_protocol::settings::{ModelService, ServiceModel};

pub struct DaemonSettingsStore {
    path: PathBuf,
    settings: Mutex<DaemonSettings>,
}

impl DaemonSettingsStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with_legacy(path, std::iter::empty())
    }

    /// Open the daemon document, importing the first old combined settings
    /// file only when the new path does not exist. The source is left intact
    /// so the desktop can independently migrate its app-owned fields.
    pub fn open_with_legacy(
        path: PathBuf,
        legacy_paths: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<Self> {
        let (mut settings, write_current) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(settings) => (settings, false),
                Err(error) => {
                    let backup = quarantine_corrupt_settings(&path)?;
                    eprintln!(
                        "Waku daemon moved invalid settings to {}: {error}",
                        backup.display()
                    );
                    (DaemonSettings::default(), true)
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut restored: Option<DaemonSettings> = None;
                for legacy_path in legacy_paths {
                    if legacy_path == path {
                        continue;
                    }
                    match fs::read(legacy_path) {
                        Ok(bytes) => {
                            if let Ok(settings) = serde_json::from_slice(&bytes) {
                                restored = Some(settings);
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                let migrated = restored.is_some();
                (restored.unwrap_or_default(), migrated)
            }
            Err(error) => return Err(error),
        };
        settings.discard_legacy_app_keys();
        if migrate_assistant_services(&mut settings) || write_current {
            write_atomic(&path, &settings)?;
        }
        Ok(Self {
            path,
            settings: Mutex::new(settings),
        })
    }

    pub fn get(&self) -> DaemonSettings {
        self.settings.lock().clone()
    }

    pub fn model_service(&self, id: Uuid) -> Option<ModelService> {
        self.settings
            .lock()
            .model_services
            .iter()
            .find(|service| service.id == id)
            .cloned()
    }

    /// Older sessions still refer to a model service by URL
    pub fn legacy_model_service(&self, assistant: Uuid, base_url: &str) -> Option<ModelService> {
        let base_url = base_url.trim().trim_end_matches('/');
        let settings = self.settings.lock();
        let default = settings
            .assistants
            .iter()
            .find(|item| item.id == assistant)
            .and_then(|item| item.model_service_id);
        settings
            .model_services
            .iter()
            .filter(|service| service.base_url == base_url)
            .filter(|service| service.legacy_key || default == Some(service.id))
            .max_by_key(|service| default == Some(service.id))
            .cloned()
    }

    pub fn replace(&self, mut settings: DaemonSettings) -> io::Result<()> {
        settings.discard_legacy_app_keys();
        migrate_assistant_services(&mut settings);
        let mut current = self.settings.lock();
        write_atomic(&self.path, &settings)?;
        *current = settings;
        Ok(())
    }

    /// Catalog edits are committed separately so older preference snapshots cannot overwrite them
    pub(crate) fn replace_preferences(&self, mut settings: DaemonSettings) -> io::Result<()> {
        let mut current = self.settings.lock();
        settings.model_services.clone_from(&current.model_services);
        settings.discard_legacy_app_keys();
        migrate_assistant_services(&mut settings);
        write_atomic(&self.path, &settings)?;
        *current = settings;
        Ok(())
    }

    pub(crate) fn save_model_service(
        &self,
        mut service: ModelService,
        key: Option<&str>,
    ) -> anyhow::Result<ModelService> {
        if service.name.trim().is_empty() {
            anyhow::bail!("Enter a model service name");
        }
        if let Some(model) = service
            .models
            .iter()
            .find(|model| !model.options.valid_token_limits())
        {
            anyhow::bail!("Invalid token limits for {}", model.id);
        }
        let endpoint = crate::driver::assistant::service_url(&service.base_url)?;
        let key = key.map(str::trim);
        if key.is_some_and(|key| !key.bytes().all(|byte| byte.is_ascii_graphic())) {
            bail!("The API key must be a single token");
        }
        let mut current = self.settings.lock();
        let mut next = current.clone();
        let saved = next
            .model_services
            .iter_mut()
            .find(|item| item.id == service.id);
        let endpoint_changed = saved.as_ref().is_some_and(|item| {
            crate::driver::assistant::service_url(&item.base_url)
                .map_or(true, |previous| previous != endpoint)
        });
        service.legacy_key = saved
            .as_ref()
            .is_some_and(|item| item.legacy_key && !endpoint_changed);
        if let Some(saved) = saved {
            *saved = service.clone();
        } else {
            next.model_services.push(service.clone());
        }
        if let Some(key) = key.or(endpoint_changed.then_some("")) {
            let mut database = open_model_service_key_db(&self.path)
                .context("Could not open the local API key store")?;
            let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if key.is_empty() {
                transaction.execute(
                    "DELETE FROM model_service_keys WHERE service_id = ?1",
                    [service.id.to_string()],
                )?;
            } else {
                transaction.execute(
                    "INSERT INTO model_service_keys (service_id, endpoint, api_key)
                     VALUES (?1, ?2, ?3) ON CONFLICT(service_id) DO UPDATE SET
                     endpoint = excluded.endpoint, api_key = excluded.api_key",
                    params![service.id.to_string(), endpoint.as_str(), key],
                )?;
            }
            // Settings write failure rolls the credential back with the SQLite transaction
            write_atomic(&self.path, &next)?;
            if let Err(error) = transaction.commit() {
                write_atomic(&self.path, &current)
                    .context("Could not restore the model service settings")?;
                return Err(error.into());
            }
        } else {
            write_atomic(&self.path, &next)?;
        }
        *current = next;
        Ok(service)
    }

    /// Update the stored flag without replacing the catalog or touching its credential
    pub(crate) fn set_model_service_enabled(
        &self,
        id: Uuid,
        enabled: bool,
    ) -> anyhow::Result<ModelService> {
        let mut current = self.settings.lock();
        let mut next = current.clone();
        let service = next
            .model_services
            .iter_mut()
            .find(|service| service.id == id)
            .context("Model service is missing")?;
        if service.enabled == enabled {
            return Ok(service.clone());
        }
        service.enabled = enabled;
        let saved = service.clone();
        write_atomic(&self.path, &next)?;
        *current = next;
        Ok(saved)
    }

    /// Read only the credential saved for the service's current endpoint
    pub(crate) fn model_service_key(
        &self,
        id: Uuid,
        endpoint: &str,
    ) -> anyhow::Result<Option<String>> {
        let path = DaemonSettings::model_service_key_db_path(&self.path);
        if !path.try_exists()? {
            return Ok(None);
        }
        let database = Connection::open_with_flags(
            resolved_key_db_path(&path)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        database
            .query_row(
                "SELECT api_key FROM model_service_keys WHERE service_id = ?1 AND endpoint = ?2",
                params![id.to_string(), endpoint],
                |row| row.get(0),
            )
            .optional()
            .context("Could not read the saved API key")
    }
}

/// Resolve system directory aliases before SQLite rejects symlinks in the database path
fn resolved_key_db_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().expect("credential database has a parent");
    Ok(parent.canonicalize()?.join(
        path.file_name()
            .expect("credential database has a filename"),
    ))
}

/// Create the plaintext credential file with owner-only permissions before SQLite writes to it
fn open_model_service_key_db(settings_path: &Path) -> anyhow::Result<Connection> {
    let path = DaemonSettings::model_service_key_db_path(settings_path);
    let directory = path.parent().expect("credential database has a parent");
    crate::fs_ext::create_private_dir_all(directory)?;
    if !fs::symlink_metadata(directory)?.is_dir() {
        bail!("The model service credentials directory must be a local directory");
    }
    crate::fs_ext::restrict_to_owner(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let database = Connection::open_with_flags(
        resolved_key_db_path(&path)?,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    database.execute_batch(
        "PRAGMA secure_delete = ON;
         CREATE TABLE IF NOT EXISTS model_service_keys (
             service_id TEXT PRIMARY KEY NOT NULL,
             endpoint TEXT NOT NULL,
             api_key TEXT NOT NULL
         );",
    )?;
    Ok(database)
}

/// Move legacy endpoint and model pairs into shared services on first load
fn migrate_assistant_services(settings: &mut DaemonSettings) -> bool {
    let mut changed = false;
    for profile in &mut settings.assistants {
        let endpoint = profile.base_url.trim().trim_end_matches('/');
        if profile.model_service_id.is_some() || endpoint.is_empty() {
            continue;
        }
        let index = settings
            .model_services
            .iter()
            .position(|service| service.legacy_key && service.base_url == endpoint)
            .unwrap_or_else(|| {
                settings.model_services.push(ModelService {
                    id: profile.id,
                    name: url::Url::parse(endpoint)
                        .ok()
                        .filter(|url| url.has_host())
                        .map(|url| {
                            url[url::Position::BeforeHost..url::Position::AfterPort].to_owned()
                        })
                        .unwrap_or_else(|| profile.name.clone()),
                    base_url: endpoint.to_owned(),
                    enabled: true,
                    models: Vec::new(),
                    legacy_key: true,
                });
                settings.model_services.len() - 1
            });
        let service = &mut settings.model_services[index];
        if !profile.model.trim().is_empty() && service.model(&profile.model).is_none() {
            service
                .models
                .push(ServiceModel::new(profile.model.clone()));
        }
        profile.model_service_id = Some(service.id);
        profile.base_url.clear();
        changed = true;
    }
    changed
}

fn quarantine_corrupt_settings(path: &Path) -> io::Result<PathBuf> {
    let extension = format!("json.corrupt-{}", Uuid::new_v4().simple());
    let backup = path.with_extension(extension);
    fs::rename(path, &backup)?;
    Ok(backup)
}

fn write_atomic(path: &Path, settings: &DaemonSettings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings).map_err(to_io_error)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}

fn to_io_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_model_keys_locally_and_scopes_them_to_the_exact_endpoint() {
        let directory = std::env::temp_dir().join(format!("waku-model-keys-{}", Uuid::new_v4()));
        let path = directory.join("settings.json");
        let mut service = ModelService {
            id: Uuid::new_v4(),
            name: "Test service".into(),
            base_url: "https://models.example/v1/".into(),
            enabled: true,
            models: vec![ServiceModel::new("fixture-model".into())],
            legacy_key: false,
        };
        let initial_endpoint = "https://models.example/v1";
        let replacement_endpoint = "https://other.example/v1";

        let store = DaemonSettingsStore::open(path.clone()).unwrap();
        assert_eq!(
            store
                .model_service_key(service.id, initial_endpoint)
                .unwrap(),
            None
        );
        store
            .save_model_service(service.clone(), Some("fixture-model-key"))
            .unwrap();

        // The settings-page switch must persist the flag without changing the catalog or key
        let mut disabled = service.clone();
        disabled.enabled = false;
        assert_eq!(
            store.set_model_service_enabled(service.id, false).unwrap(),
            disabled
        );
        drop(store);

        let store = DaemonSettingsStore::open(path.clone()).unwrap();
        assert_eq!(store.model_service(service.id), Some(disabled));
        assert_eq!(
            store
                .model_service_key(service.id, initial_endpoint)
                .unwrap()
                .as_deref(),
            Some("fixture-model-key"),
        );
        assert_eq!(
            store.set_model_service_enabled(service.id, true).unwrap(),
            service
        );
        assert_eq!(
            store
                .model_service_key(service.id, replacement_endpoint)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .model_service_key(Uuid::new_v4(), initial_endpoint)
                .unwrap(),
            None
        );
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("fixture-model-key")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let db = DaemonSettings::model_service_key_db_path(&path);
            assert_eq!(
                fs::metadata(&db).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(db.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        // Force a settings write failure after the credential transaction has started
        let blocked_write = path.with_extension("json.tmp");
        fs::create_dir(&blocked_write).unwrap();
        assert!(store.set_model_service_enabled(service.id, false).is_err());
        assert_eq!(store.model_service(service.id), Some(service.clone()));
        assert!(
            store
                .save_model_service(service.clone(), Some("unsaved-key"))
                .is_err()
        );
        assert_eq!(
            store
                .model_service_key(service.id, initial_endpoint)
                .unwrap()
                .as_deref(),
            Some("fixture-model-key"),
        );
        fs::remove_dir(blocked_write).unwrap();

        service.base_url = replacement_endpoint.into();
        store.save_model_service(service.clone(), None).unwrap();
        assert_eq!(
            store
                .model_service_key(service.id, initial_endpoint)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .model_service_key(service.id, replacement_endpoint)
                .unwrap(),
            None
        );
        store
            .save_model_service(service.clone(), Some("rotated-key"))
            .unwrap();
        assert_eq!(
            store
                .model_service_key(service.id, replacement_endpoint)
                .unwrap()
                .as_deref(),
            Some("rotated-key"),
        );
        store.save_model_service(service.clone(), Some("")).unwrap();
        assert_eq!(
            store
                .model_service_key(service.id, replacement_endpoint)
                .unwrap(),
            None
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_assistants_to_one_service_per_legacy_endpoint() {
        let mut settings: DaemonSettings = serde_json::from_value(serde_json::json!({
            "assistants": [
                { "id": Uuid::new_v4(), "name": "Guide", "system_prompt": "read", "model": "first", "base_url": "https://example.test/v1/", "read_readme": true },
                { "id": Uuid::new_v4(), "name": "Chat", "system_prompt": "help", "model": "second", "base_url": "https://example.test/v1", "read_readme": false }
            ]
        })).unwrap();

        assert!(migrate_assistant_services(&mut settings));
        assert!(!migrate_assistant_services(&mut settings));
        assert_eq!(settings.model_services.len(), 1);
        let service = &settings.model_services[0];
        assert!(service.legacy_key);
        assert_eq!(
            service
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(settings.assistants.iter().all(|profile| {
            profile.model_service_id == Some(service.id) && profile.base_url.is_empty()
        }));
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved["assistants"][0].get("read_readme").is_none());
    }
    use serde_json::Value;
    use uuid::Uuid;
    use waku_protocol::model::ProviderKind;

    #[test]
    fn legacy_combined_settings_keep_only_daemon_fields() {
        let path = std::env::temp_dir().join(format!("waku-settings-{}.json", Uuid::new_v4()));
        fs::write(
            &path,
            r#"{"theme":"dark","analytics_enabled":false,"computer_use_enabled":true,"future":42}"#,
        )
        .unwrap();

        let store = DaemonSettingsStore::open(path.clone()).unwrap();
        let settings = store.get();
        assert!(settings.computer_use_enabled);
        assert_eq!(settings.extra.get("future"), Some(&Value::from(42)));
        assert!(!settings.extra.contains_key("theme"));
        store.replace(settings).unwrap();

        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(value.get("theme").is_none());
        assert!(value.get("analytics_enabled").is_none());
        assert_eq!(value["future"], 42);
        fs::remove_file(path).ok();
    }

    #[test]
    fn imports_daemon_fields_from_a_debug_combined_settings_file() {
        let directory = std::env::temp_dir().join(format!("waku-settings-{}", Uuid::new_v4()));
        let path = directory.join("home/settings.json");
        let legacy = directory.join("checkout/temp/settings.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(
            &legacy,
            r#"{"theme":"dark","disabled_providers":["claude"]}"#,
        )
        .unwrap();

        let store = DaemonSettingsStore::open_with_legacy(path.clone(), [legacy.clone()]).unwrap();

        assert_eq!(store.get().disabled_providers, vec![ProviderKind::Claude]);
        let migrated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["disabled_providers"][0], "claude");
        assert!(migrated.get("theme").is_none());
        assert!(legacy.exists());
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn corrupt_current_settings_are_quarantined_and_replaced() {
        let directory = std::env::temp_dir().join(format!("waku-settings-{}", Uuid::new_v4()));
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, b"{ definitely not json").unwrap();

        let store = DaemonSettingsStore::open(path.clone()).unwrap();

        assert_eq!(store.get(), DaemonSettings::default());
        assert!(serde_json::from_slice::<DaemonSettings>(&fs::read(&path).unwrap()).is_ok());
        assert!(fs::read_dir(&directory).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("settings.json.corrupt-")
        }));
        fs::remove_dir_all(directory).ok();
    }
}
