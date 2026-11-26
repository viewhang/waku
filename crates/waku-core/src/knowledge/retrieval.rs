//! Retrieve evidence across the saved catalog while keeping each embedding space separate

use super::*;

/// Use the shortest unique directory suffix so citations distinguish libraries with the same name
fn library_name<'a>(root: &'a str, catalog: &[LibraryConfig]) -> &'a str {
    let path = Path::new(root);
    for parent in path.ancestors().skip(1) {
        let suffix = path.strip_prefix(parent).expect("ancestor is a prefix");
        if !catalog
            .iter()
            .any(|other| other.root != root && Path::new(&other.root).ends_with(suffix))
        {
            return suffix.to_str().expect("library roots are UTF-8");
        }
    }
    root
}

impl KnowledgeLibrary {
    /// Rank all compatible passages before limiting the evidence sent to the dialogue model
    pub(super) fn retrieve(
        &self,
        db: &Connection,
        generation: u64,
        query: &str,
    ) -> Result<(Vec<KnowledgeSource>, Vec<String>)> {
        let query = query.trim();
        ensure!(
            !query.is_empty() && query.chars().count() <= 4000,
            "Enter a question of 1–4000 characters"
        );
        let libraries = self.state.lock().status.libraries.clone();
        let mut queries = HashMap::new();
        let mut ranked = Vec::new();
        let mut warnings = Vec::new();
        for config in &libraries {
            self.current(generation)?;
            let indexed: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM documents WHERE root=? AND indexed_with IS NOT NULL)",
                [&config.root],
                |row| row.get(0),
            )?;
            if !indexed {
                continue;
            }
            let identity = (config.embedding.service_id, &config.embedding.model);
            // Cache failures too so a shared unavailable service is contacted only once per model
            let vector = queries.entry(identity).or_insert_with(|| -> Result<_> {
                let model = LibraryModel::resolve(&self.settings, &config.embedding, true)?;
                let vector = model
                    .embed(&[query])?
                    .pop()
                    .context("Missing query vector")?;
                Ok((model.index_key(&config.embedding), vector))
            });
            let hits = match vector {
                Ok((key, vector)) => index::search(db, &config.root, key, query, vector),
                Err(error) => Err(anyhow::anyhow!(error.to_string())),
            };
            match hits {
                Ok(hits) => ranked.extend(hits),
                Err(error) => warnings.push(format!(
                    "{}: {error}",
                    library_name(&config.root, &libraries)
                )),
            }
        }
        if !warnings.is_empty() && ranked.is_empty() {
            bail!("{}", warnings.join("\n"));
        }
        ranked.sort_by(|(a, _), (b, _)| b.total_cmp(a));
        let mut revisions = HashMap::new();
        let sources = ranked
            .into_iter()
            .filter_map(|(_, mut source)| {
                let revision = revisions.entry(source.document_id).or_insert_with(|| {
                    index::read(Path::new(&source.library), &source.path)
                        .ok()
                        .map(|text| index::revision(&text))
                });
                if revision.as_ref() != Some(&source.revision) {
                    return None;
                }
                source.library = library_name(&source.library, &libraries).to_owned();
                Some(source)
            })
            .take(5)
            .collect();
        self.current(generation)?;
        Ok((sources, warnings))
    }
}
