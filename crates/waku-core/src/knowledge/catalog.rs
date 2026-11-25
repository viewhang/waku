//! The first saved directory is active, so switching and removal persist in one atomic write

use super::*;

/// Existing installations stored one config object beside the SQLite database
pub(super) fn load(path: &Path) -> Result<Vec<LibraryConfig>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Saved {
        Libraries(Vec<LibraryConfig>),
        Legacy(Option<LibraryConfig>),
    }
    Ok(match serde_json::from_slice(&fs::read(path)?)? {
        Saved::Libraries(configs) => configs,
        Saved::Legacy(config) => config.into_iter().collect(),
    })
}

pub(super) fn position(libraries: &[LibraryConfig], root: &str) -> Result<usize> {
    libraries
        .iter()
        .position(|item| item.root == root)
        .context("Unknown knowledge library")
}

impl KnowledgeLibrary {
    /// Validate the entire selection before saving it and deduplicate aliases by canonical root
    pub(super) fn save_roots(
        &self,
        roots: Vec<String>,
        embedding: KnowledgeModel,
        update_model: bool,
    ) -> Result<KnowledgeResult> {
        ensure!(!roots.is_empty(), "Select at least one knowledge library");
        LibraryModel::resolve(&self.settings, &embedding, true)?;
        let mut selected = Vec::new();
        for path in roots {
            ensure!(
                Path::new(&path).is_absolute(),
                "Choose an absolute directory on the daemon host"
            );
            let root = fs::canonicalize(&path).with_context(|| format!("Cannot open {path}"))?;
            ensure!(
                root.is_dir(),
                "The selected library is not a directory: {path}"
            );
            let root = root
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Invalid directory encoding"))?;
            if !selected.contains(&root) {
                selected.push(root);
            }
        }
        self.update_libraries(move |libraries| {
            for root in selected.into_iter().rev() {
                let config =
                    if let Some(index) = libraries.iter().position(|item| item.root == root) {
                        let mut config = libraries.remove(index);
                        if update_model {
                            config.embedding.clone_from(&embedding);
                        }
                        config
                    } else {
                        LibraryConfig {
                            root,
                            embedding: embedding.clone(),
                        }
                    };
                libraries.insert(0, config);
            }
            Ok(())
        })
    }

    /// Serialize catalog changes with note writes and publish only after persistence succeeds
    pub(super) fn update_libraries(
        &self,
        edit: impl FnOnce(&mut Vec<LibraryConfig>) -> Result<()>,
    ) -> Result<KnowledgeResult> {
        let _serial = self.writes.lock();
        let mut libraries = self.state.lock().status.libraries.clone();
        edit(&mut libraries)?;
        crate::settings::write_atomic(&self.path.with_extension("json"), &libraries)?;
        let mut state = self.state.lock();
        let config = libraries.first().cloned();
        if libraries != state.status.libraries {
            state.generation += 1;
            let _ = self.wake.try_send(());
        }
        if config != state.status.config {
            state.status = LibraryStatus {
                config,
                ..Default::default()
            };
        }
        state.status.libraries = libraries;
        Ok(KnowledgeResult::Status {
            status: state.status.clone(),
        })
    }
}
