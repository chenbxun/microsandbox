//! OverlayBD config generation for overlaybd-tcmu backing store.
//!
//! This module constructs the `config.v1.json` file that the overlaybd-tcmu
//! service reads to create a virtual block device backed by remote OCI layers.
//!
//! Supports both standard OverlayBD and TurboOCI remote layer formats.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oci_client::manifest::OciImageManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::{MicrosandboxResult, oci::Reference}; // crate::oci::Reference -> oci_client::Reference -> oci_spec::distribution::Reference
use microsandbox_utils::path::{
    OVERLAYBD_CACHE_SUBDIR, OVERLAYBD_CONFIG_FILENAME, OVERLAYBD_RESULT_FILENAME,
    OVERLAYBD_SUBDIR,
};

use super::registry::{
    FAST_OCI_DIGEST_ANNOTATION, FAST_OCI_MEDIA_TYPE_ANNOTATION, OVERLAYBD_BLOB_DIGEST_ANNOTATION,
    OVERLAYBD_BLOB_SIZE_ANNOTATION, TURBO_OCI_DIGEST_ANNOTATION, TURBO_OCI_MEDIA_TYPE_ANNOTATION,
};

/// Default filesystem type for TurboOCI layers when `blob-fs-type` annotation is absent.
const DEFAULT_TURBO_OCI_FS_TYPE: &str = "ext4";

/// Gzip metadata index filename for TurboOCI layers.
const TURBO_OCI_GZIP_INDEX_FILENAME: &str = "gzip.meta";

/// Filesystem types supported by TurboOCI, in priority order.
/// Mirrors `turboFsType` configuration in accelerated-container-image.
const TURBO_OCI_FS_TYPES: &[&str] = &["erofs", "ext4"];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Top-level overlaybd-tcmu backing store config (maps to `config.v1.json`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverlayBDConfig {
    /// The url of the repository blobs of the remote image. It is required for a registry image.
    /// overlaybd-tcmu appends `/<digest>` to fetch each layer on demand.
    /// e.g. `https://registry-1.docker.io/v2/library/ubuntu/blobs`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_blob_url: Option<String>,

    /// A list describing the lower layers of the image in bottom-upper order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lowers: Option<Vec<OverlayBDConfigLower>>,

    /// Upper (writable) layer config. Empty for read-only images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper: Option<OverlayBDConfigUpper>,

    /// The file for saving the failure reasons. If a device is successfully lauched, success is writen into the file, otherwise, the failure s reported by this file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_file: Option<String>,
}

/// Per-layer lower config for overlaybd-tcmu.
///
/// Field order follows Go struct `OverlayBDBSConfigLower` in
/// accelerated-container-image/pkg/types/types.go.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverlayBDConfigLower {
    /// Gzip index file path for gzip-compressed TurboOCI layers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gzip_index: Option<String>,

    /// Filesystem metadata file path.
    /// - For TurboOCI layers: points to `<dir>/<fstype>.fs.meta` for on-demand block mapping.
    /// - For local layers: path to the local overlaybd commit file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,

    /// The digest of a standard (non-TurboOCI) remote layer.
    /// From annotation `containerd.io/snapshot/overlaybd/blob-digest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,

    /// Local file path for the layer blob (storageTypeLocalBlock).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,

    /// Target digest for TurboOCI remote layers.
    /// From annotation `containerd.io/snapshot/overlaybd/turbo-oci/target-digest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_digest: Option<String>,

    /// The size of a standard (non-TurboOCI) remote layer in bytes.
    /// From annotation `containerd.io/snapshot/overlaybd/blob-size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,

    /// Cache directory for the layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OverlayBDConfigUpper {
    /// Path to the writable data file.
    pub data: String,

    /// Path to the writable index file.
    pub index: String,

    /// Virtual disk size in GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsize: Option<u32>,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Rules:
/// 1. `upper` and `lowers` must not both be `None` — the image must have at least one layer.
/// 2. `digest` (standard) and `target_digest` (TurboOCI) are mutually exclusive per lower.
/// 3. Each lower must have at least one identifying field (`digest`, `target_digest`, `file`, or `dir`).
/// 4. If any lower uses remote fields (`digest` or `target_digest`), `repo_blob_url` must be set.
fn validate_overlaybd_config(config: &OverlayBDConfig) -> MicrosandboxResult<()> {
    // Rule 1: upper and lowers must not both be None.
    if config.upper.is_none() && config.lowers.as_ref().map_or(true, |l| l.is_empty()) {
        return Err(anyhow::anyhow!("invalid OverlayBD config: upper and lowers must not both be empty").into());
    }

    if let Some(lowers) = &config.lowers {
        for (i, lower) in lowers.iter().enumerate() {
            let has_digest = lower.digest.is_some();
            let has_target_digest = lower.target_digest.is_some();
            let has_file = lower.file.is_some();
            let has_dir = lower.dir.is_some();

            // Rule 2: digest (standard) and target_digest (TurboOCI) are mutually exclusive.
            if has_digest && has_target_digest {
                return Err(anyhow::anyhow!(
                    "invalid OverlayBD config: lower[{}] has both 'digest' (standard) and 'target_digest' (TurboOCI)",
                    i
                ).into());
            }

            // Rule 3: must have at least one identifying field.
            if !has_digest && !has_target_digest && !has_file && !has_dir {
                return Err(anyhow::anyhow!(
                    "invalid OverlayBD config: lower[{}] has no identifying fields (digest, target_digest, file, or dir)",
                    i
                ).into());
            }

            // Rule 4: remote lower requires repo_blob_url.
            if (has_digest || has_target_digest) && config.repo_blob_url.is_none() {
                return Err(anyhow::anyhow!(
                    "invalid OverlayBD config: lower[{}] uses remote fields but repo_blob_url is not set",
                    i
                ).into());
            }
        }
    }

    Ok(())
}

/// Checks whether a layer's annotations indicate TurboOCI format.
///
/// Returns `(is_turbo_oci, target_digest, media_type)`.
/// Checks the current `turbo-oci` annotation first, then falls back to legacy `fastoci`.
///
/// Mirrors `checkTurboOCI` in accelerated-container-image/pkg/snapshot/overlay.go:405.
pub(crate) fn check_turbo_oci(
    annotations: &BTreeMap<String, String>,
) -> (bool, Option<&String>, Option<&String>) {
    if let Some(digest) = annotations.get(TURBO_OCI_DIGEST_ANNOTATION) {
        return (
            true,
            Some(digest),
            annotations.get(TURBO_OCI_MEDIA_TYPE_ANNOTATION),
        );
    }
    if let Some(digest) = annotations.get(FAST_OCI_DIGEST_ANNOTATION) {
        return (
            true,
            Some(digest),
            annotations.get(FAST_OCI_MEDIA_TYPE_ANNOTATION),
        );
    }
    (false, None, None)
}

/// Checks if the media type indicates gzip compression.
///
/// Mirrors `isGzipLayerType` in accelerated-container-image/pkg/snapshot/storage.go:948.
fn is_gzip_layer_type(media_type: &str) -> bool {
    // OCI: application/vnd.oci.image.layer.v1.tar+gzip
    // Docker: application/vnd.docker.image.rootfs.diff.tar.gzip
    media_type == "application/vnd.oci.image.layer.v1.tar+gzip"
        || media_type == "application/vnd.docker.image.rootfs.diff.tar.gzip"
}

/// Rust implementation of constructImageBlobURL in accelerated-container-image/pkg/snapshot/storage.go: 848.
///
/// Constructs the registry blob URL prefix from an image reference.
///
/// For example, `docker.io/library/ubuntu:latest` becomes
/// `https://registry-1.docker.io/v2/library/ubuntu/blobs`.
fn construct_repo_blob_url(reference: &Reference) -> String {
    let host = match reference.registry() {
        "docker.io" => "registry-1.docker.io",
        other => other,
    };
    let repo = reference.repository();

    // Default to HTTPS. In the future we could support insecure registries.
    format!("https://{}/v2/{}/blobs", host, repo)
}

/// Returns the base directory for an OverlayBD image's config and cache.
///
/// Path: `<microsandbox_home>/overlaybd/<reference_hash>/`
///
/// The reference is hashed (SHA-256, truncated to 16 hex chars) to produce
/// a filesystem-safe directory name.
pub(crate) fn overlaybd_image_dir(microsandbox_home: &Path, reference: &Reference) -> PathBuf {
    let hash = format!("{:x}", Sha256::digest(reference.as_db_key().as_bytes()));
    let short_hash = &hash[..16];
    microsandbox_home
        .join(OVERLAYBD_SUBDIR)
        .join(short_hash)
}

/// Simplified Rust implementation of ConstructOverlayBDSpec in accelerated-container-image/pkg/snapshot/storage.go: 598.
/// Handles `storageTypeRemoteBlock` for both standard OverlayBD and TurboOCI layers.
///
/// Builds the [`OverlayBDConfig`] and writes it to `config.v1.json`.
///
/// ## Arguments
///
/// * `microsandbox_home` - The microsandbox global data directory (`~/.microsandbox`)
/// * `reference` - The OCI image reference
/// * `manifest` - The OCI image manifest (must be an OverlayBD image)
///
/// ## Returns
///
/// The path to the written `config.v1.json` file.
pub(crate) async fn write_overlaybd_config(
    microsandbox_home: &Path,
    reference: &Reference,
    manifest: &OciImageManifest,
) -> MicrosandboxResult<PathBuf> {
    let image_dir = overlaybd_image_dir(microsandbox_home, reference);

    // Build per-layer lower configs
    let mut lowers = Vec::with_capacity(manifest.layers.len());
    for (i, layer) in manifest.layers.iter().enumerate() {
        let annotations = layer
            .annotations
            .as_ref()
            .expect("OverlayBD layer must have annotations");

        let layer_dir = layer_cache_dir(microsandbox_home, reference, i);
        fs::create_dir_all(&layer_dir).await?;

        let (is_turbo_oci, target_digest, media_type) = check_turbo_oci(annotations);

        if is_turbo_oci {
            // TurboOCI remote layer: uses target_digest + filesystem metadata file.
            // Mirrors storage.go:666-674.
            let target_digest = target_digest.unwrap().clone();
            // Detect which fs.meta file was extracted from the turboOCIv1.tar.gz archive.
            // Mirrors turboOCIFsMeta() in overlay.go:1539.
            let fsmeta_path = detect_fs_meta_in_dir(&layer_dir);

            let mut lower = OverlayBDConfigLower {
                gzip_index: None,
                file: Some(fsmeta_path.to_string_lossy().to_string()),
                digest: None,
                target_file: None,
                target_digest: Some(target_digest),
                size: None,
                dir: Some(layer_dir.to_string_lossy().to_string()),
            };

            // If the layer is gzip-compressed, set the gzip index path.
            if let Some(media_type) = media_type {
                if is_gzip_layer_type(media_type) {
                    let gzip_idx_path = layer_dir.join(TURBO_OCI_GZIP_INDEX_FILENAME);
                    lower.gzip_index = Some(gzip_idx_path.to_string_lossy().to_string());
                }
            }

            lowers.push(lower);
        } else {
            // Standard non-TurboOCI remote layer: uses digest + size.
            // Mirrors storage.go:677-681.
            let blob_digest = annotations
                .get(OVERLAYBD_BLOB_DIGEST_ANNOTATION)
                .expect("OverlayBD layer must have blob-digest annotation")
                .clone();

            let blob_size: i64 = annotations
                .get(OVERLAYBD_BLOB_SIZE_ANNOTATION)
                .expect("OverlayBD layer must have blob-size annotation")
                .parse()
                .map_err(|e| anyhow::anyhow!("failed to parse overlaybd blob-size: {}", e))?;

            lowers.push(OverlayBDConfigLower {
                gzip_index: None,
                file: None,
                digest: Some(blob_digest),
                target_file: None,
                target_digest: None,
                size: Some(blob_size),
                dir: Some(layer_dir.to_string_lossy().to_string()),
            });
        }
    }

    let config = OverlayBDConfig {
        repo_blob_url: Some(construct_repo_blob_url(reference)),
        lowers: Some(lowers),
        upper: None,
        result_file: Some(image_dir.join(OVERLAYBD_RESULT_FILENAME).to_string_lossy().to_string()),
    };

    // Write config.v1.json
    let config_path = image_dir.join(OVERLAYBD_CONFIG_FILENAME);
    fs::create_dir_all(&image_dir).await?;

    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| anyhow::anyhow!("failed to serialize OverlayBD config: {}", e))?;

    fs::write(&config_path, json).await?;
    tracing::info!(path = %config_path.display(), "wrote OverlayBD config");
    tracing::debug!(?config, "OverlayBD config content");

    // Validate the config before returning.
    validate_overlaybd_config(&config)?;

    Ok(config_path)
}

/// Updates the `upper` field in an existing `config.v1.json` with writable layer info.
///
/// # Arguments
///
/// * `config_path` - Path to the existing `config.v1.json`
/// * `data_path` - Path to the writable data file (e.g., `rw_dir/data.lsmt`)
/// * `index_path` - Path to the writable index file (e.g., `rw_dir/index.lsmt`)
/// * `vsize` - Virtual disk size in GiB
pub(crate) async fn update_overlaybd_config_upper(
    ro_config_path: &Path,
    rw_config_path: &Path,
    data_path: &Path,
    index_path: &Path,
    vsize: u64,
) -> MicrosandboxResult<()> {
    let content = fs::read_to_string(ro_config_path).await?;
    let mut config: OverlayBDConfig = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse OverlayBD config: {}", e))?;

    config.upper = Some(OverlayBDConfigUpper {
        data: data_path.to_string_lossy().to_string(),
        index: index_path.to_string_lossy().to_string(),
        vsize: Some(vsize as u32),
    });

    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| anyhow::anyhow!("failed to serialize OverlayBD config: {}", e))?;

    fs::write(rw_config_path, json).await?;
    tracing::info!(path = %rw_config_path.display(), "updated OverlayBD config upper");

    Ok(())
}

/// Simplified Rust implementation of identifySnapshotStorageType in accelerated-container-image/pkg/snapshot/overlay.go: 1412.
///
/// Checks whether an OCI image manifest describes an OverlayBD image.
///
/// An OverlayBD image (including TurboOCI) is identified by the presence of both
/// `containerd.io/snapshot/overlaybd/blob-digest` and
/// `containerd.io/snapshot/overlaybd/blob-size` annotations
/// on **all** layer descriptors in the manifest.
///
/// TurboOCI images also carry these standard annotations, so they are detected here as well.
pub(crate) fn is_overlaybd_image(manifest: &OciImageManifest) -> bool {
    if manifest.layers.is_empty() { // layers: Vec<OciDescriptor>
        return false;
    }

    manifest.layers.iter().all(|layer| {
        layer
            .annotations // annotations: Option<BTreeMap<String, String>>
            .as_ref()
            .is_some_and(|annotations| {
                annotations.contains_key(OVERLAYBD_BLOB_DIGEST_ANNOTATION)
                    && annotations.contains_key(OVERLAYBD_BLOB_SIZE_ANNOTATION)
            })
    })
}

/// Detects which filesystem metadata file exists in the given directory.
///
/// Mirrors `turboOCIFsMeta` in accelerated-container-image/pkg/snapshot/overlay.go:1539.
/// Checks for `erofs.fs.meta`, `ext4.fs.meta` in priority order.
/// If erofs.fs.meta exists but the host kernel does not support erofs, it is skipped.
/// Falls back to `ext4.fs.meta` if none found.
fn detect_fs_meta_in_dir(dir: &Path) -> PathBuf {
    for fs_type in TURBO_OCI_FS_TYPES {
        let path = dir.join(format!("{fs_type}.fs.meta"));
        if path.exists() {
            if *fs_type == "erofs" && !is_erofs_supported() {
                tracing::warn!("erofs.fs.meta found but erofs is not supported on this system, skipping");
                continue;
            }
            return path;
        }
    }
    dir.join(format!("{DEFAULT_TURBO_OCI_FS_TYPE}.fs.meta"))
}

/// Checks whether the host kernel supports the erofs filesystem.
///
/// Mirrors `IsErofsSupported` in accelerated-container-image/pkg/snapshot/overlay.go:1519.
/// Reads `/proc/filesystems` and looks for a `\terofs\n` entry.
fn is_erofs_supported() -> bool {
    match std::fs::read_to_string("/proc/filesystems") {
        Ok(content) => content.contains("\terofs\n"),
        Err(_) => false,
    }
}

/// Checks if any TurboOCI filesystem metadata file exists in the given directory.
pub(crate) fn has_fs_meta(dir: &Path) -> bool {
    TURBO_OCI_FS_TYPES
        .iter()
        .any(|t| dir.join(format!("{t}.fs.meta")).exists())
}

/// Returns the per-layer cache directory for an OverlayBD image.
///
/// Path: `<microsandbox_home>/overlaybd/<reference_hash>/cache/<layer_index>/`
pub(crate) fn layer_cache_dir(
    microsandbox_home: &Path,
    reference: &Reference,
    layer_index: usize,
) -> PathBuf {
    overlaybd_image_dir(microsandbox_home, reference)
        .join(OVERLAYBD_CACHE_SUBDIR)
        .join(layer_index.to_string())
}

/// Extracts TurboOCI metadata from a `turboOCIv1.tar.gz` blob.
///
/// The blob is a gzip-compressed tar archive produced during TurboOCI image conversion
/// (see accelerated-container-image/cmd/convertor/builder/turboOCI_builder.go).
/// It typically contains:
/// - `<fstype>.fs.meta` — filesystem block mapping metadata (e.g., `ext4.fs.meta`)
/// - `gzip.meta` (optional) — gzip decompression index for gzip-compressed layers
/// - `.turbo.ociv1` — format identifier marker (ignored by overlaybd-tcmu)
///
/// All files are extracted to `target_dir`.
pub(crate) async fn extract_turbo_oci_metadata(
    blob_data: Vec<u8>,
    target_dir: PathBuf,
) -> MicrosandboxResult<()> {
    tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
        std::fs::create_dir_all(&target_dir)
            .map_err(|e| anyhow::anyhow!("failed to create dir {}: {e}", target_dir.display()))?;
        let cursor = std::io::Cursor::new(blob_data);
        let gz = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(gz);
        archive
            .unpack(&target_dir)
            .map_err(|e| anyhow::anyhow!("failed to unpack turboOCIv1 archive: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("task join error in extract_turbo_oci_metadata: {e}"))??;
    Ok(())
}
