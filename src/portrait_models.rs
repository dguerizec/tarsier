use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::avatar_source::Portrait;

// Discover exported bundles only; the renderer validates the mesh before using it.
pub fn catalog(directories: &[PathBuf], current: &Path) -> Result<Vec<Portrait>> {
    let current = current.canonicalize().ok();
    let mut candidates = current.iter().cloned().collect::<Vec<_>>();
    for directory in directories {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            candidates.push(entry?.path());
        }
    }
    let mut models: Vec<Portrait> = Vec::new();
    for path in candidates {
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        if models.iter().any(|model| model.path == path) || !path.join("mesh.npz").is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path.join("manifest.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if manifest["schema_version"] != 1
            || !manifest["draws"]
                .as_array()
                .is_some_and(|draws| !draws.is_empty())
        {
            continue;
        }
        models.push(Portrait {
            id: format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes())),
            name: manifest["revision"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }),
            selected: current.as_ref() == Some(&path),
            path,
        });
    }
    models.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_deduplicates_current_and_excludes_incomplete_bundles() {
        let root =
            std::env::temp_dir().join(format!("tarsier-models-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        let model = root.as_path().join("model");
        std::fs::create_dir(&model).unwrap();
        std::fs::write(
            model.join("manifest.json"),
            r#"{"schema_version":1,"revision":"My model","draws":[{}]}"#,
        )
        .unwrap();
        assert!(
            catalog(&[root.as_path().into()], &model)
                .unwrap()
                .is_empty()
        );
        std::fs::write(model.join("mesh.npz"), b"mesh").unwrap();
        let models = catalog(&[root.as_path().into(), root.as_path().into()], &model).unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].selected);
        assert_eq!(models[0].name, "My model");
        std::fs::remove_dir_all(root).unwrap();
    }
}
