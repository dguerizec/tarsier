//! Local avatar imports are prepared privately, then published by atomic rename.
use std::{
    collections::HashSet,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::extract::Multipart;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

pub const MAX_MODEL_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_MODEL_FILES: usize = 128;

pub fn default_library() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .filter(|path| path.is_absolute())
        .map(|path| path.join("tarsier/avatars"))
}

pub fn private_directory(path: &Path) -> Result<()> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent().context("Invalid avatar library path")?;
    }
    let resolved = existing.canonicalize()?.join(path.strip_prefix(existing)?);
    if resolved.starts_with(Path::new(env!("CARGO_MANIFEST_DIR"))) {
        bail!("Avatar imports must be stored outside the repository");
    }

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

pub struct StagingDirectory(pub PathBuf);
impl StagingDirectory {
    pub fn new(parent: &Path) -> Result<Self> {
        private_directory(parent)?;
        let path = parent.join(format!(".import-{:032x}", rand::random::<u128>()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

pub fn import_image(parent: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let normalized = crate::avatar_source::normalize(bytes)?;
    let staging = StagingDirectory::new(parent)?;
    let base: String = Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .take(80)
        .collect();
    let base = if base.trim().is_empty() {
        "Portrait"
    } else {
        base.trim()
    };
    let name = format!("{base}-{:08x}.png", rand::random::<u32>());
    let file = staging.0.join("image.png");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)?
        .write_all(&normalized)?;
    std::fs::rename(file, parent.join(name))?;
    Ok(())
}

pub async fn receive_model(multipart: &mut Multipart, directory: &Path) -> Result<()> {
    let mut names = HashSet::new();
    let mut total = 0usize;
    while let Some(mut field) = multipart.next_field().await? {
        let name = field
            .file_name()
            .context("Each model file needs a filename")?
            .to_owned();
        if !safe_filename(&name) || !names.insert(name.clone()) || names.len() > MAX_MODEL_FILES {
            bail!("Invalid or duplicate model filename, or too many files");
        }
        let allowed = name == "manifest.json"
            || name == "mesh.npz"
            || name.to_ascii_lowercase().ends_with(".png")
            || name.to_ascii_lowercase().ends_with(".jpg")
            || name.to_ascii_lowercase().ends_with(".jpeg");
        if !allowed {
            bail!("Import an exported Tarsier model folder, not a Blender scene");
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(&name))
            .await?;
        let mut file_bytes = 0usize;
        while let Some(chunk) = field.chunk().await? {
            file_bytes += chunk.len();
            if name == "manifest.json" && file_bytes > 1024 * 1024 {
                bail!("Model manifest exceeds 1 MB");
            }
            total = total
                .checked_add(chunk.len())
                .context("Model is too large")?;
            if total > MAX_MODEL_BYTES {
                bail!("Model exceeds 256 MB");
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
    }
    if !names.contains("manifest.json") || !names.contains("mesh.npz") {
        bail!("The model folder must contain manifest.json and mesh.npz");
    }
    Ok(())
}

pub fn preview_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::io::{Cursor, Read};
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1024 * 1024)
        .read_to_end(&mut bytes)?;
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(256);
    limits.max_image_height = Some(256);
    limits.max_alloc = Some(1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    if image.width() != 256 || image.height() != 256 {
        bail!("Invalid model preview dimensions");
    }
    // Preserve the generated silhouette instead of normalizing it to an RGB portrait.
    Ok(bytes)
}

#[derive(Deserialize, serde::Serialize)]
pub struct Preparation {
    pub preview: bool,
    pub warning: Option<String>,
}

pub async fn prepare_model(project: &Path, model: &Path, preview: &Path) -> Result<Preparation> {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join(project);
    let mut command = tokio::process::Command::new(project.join(".venv/bin/python"));
    command
        .args(["-m", "tarsier_perception.avatar_import"])
        .arg(model)
        .arg(preview)
        .current_dir(&project)
        .env("PYTHONPATH", project.join("src"))
        .env("LIBGL_ALWAYS_SOFTWARE", "true")
        .env("OPENBLAS_NUM_THREADS", "1")
        .env("OMP_NUM_THREADS", "1")
        .env("LP_NUM_THREADS", "1")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(75), command.output())
        .await
        .context("Model preparation timed out")?
        .context(
            "Model importer unavailable. Install the perception worker with avatar dependencies",
        )?;
    if !output.status.success() {
        bail!(
            "Invalid Personal 3D export, or model preparation exceeded its limits. Check the manifest, mesh and textures"
        );
    }
    serde_json::from_slice(&output.stdout).context("Model importer returned an invalid result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_preserve_transparent_png_pixels() {
        let root =
            std::env::temp_dir().join(format!("tarsier-preview-{:032x}", rand::random::<u128>()));
        let staging = StagingDirectory::new(&root).unwrap();
        let path = staging.0.join("preview.png");
        image::RgbaImage::from_pixel(256, 256, image::Rgba([10, 20, 30, 0]))
            .save(&path)
            .unwrap();
        let bytes = preview_bytes(&path).unwrap();
        assert_eq!(
            image::load_from_memory(&bytes)
                .unwrap()
                .to_rgba8()
                .get_pixel(0, 0)
                .0,
            [10, 20, 30, 0]
        );
        drop(staging);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_refuse_repository_storage_and_unsafe_filenames() {
        assert!(private_directory(Path::new(env!("CARGO_MANIFEST_DIR"))).is_err());
        for name in [
            "",
            ".hidden",
            "../mesh.npz",
            "a/b.png",
            "a\\b.png",
            "a\n.png",
        ] {
            assert!(!safe_filename(name), "{name}");
        }
        assert!(safe_filename("texture-0.png"));
    }

    #[test]
    fn images_are_private_and_staging_is_removed_after_success_and_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("tarsier-import-{:032x}", rand::random::<u128>()));
        import_image(
            &root,
            "../../portrait.jpg",
            include_bytes!("../assets/avatars/liveportrait-default.png"),
        )
        .unwrap();
        let entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(import_image(&root, "broken.png", b"broken image").is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let staging = StagingDirectory::new(&root).unwrap();
        let stage = staging.0.clone();
        drop(staging);
        assert!(!stage.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
