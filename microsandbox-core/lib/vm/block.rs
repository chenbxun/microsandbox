//! Block device image management for MicroVm rootfs.
//!
//! This module provides functionality for creating and managing block device images
//! that can be used as rootfs for MicroVMs via virtio-blk.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::process::Command;
use sha2::{Sha256, Digest};

use crate::{MicrosandboxError, MicrosandboxResult, oci::overlaybd};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

pub const BLOCK_IMAGE_FILENAME: &str = "rootfs.img";
pub const OVERLAYBD_CONFIG_FILENAME: &str = "config.v1.json";
const OVERLAYBD_SNAPSHOTTER_SERVICE: &str = "overlaybd-snapshotter";
const OVERLAYBD_TCMU_SERVICE: &str = "overlaybd-tcmu";
const OVERLAYBD_SNAPSHOTTER_SERVICE_PATH: &str = "/opt/overlaybd/snapshotter/overlaybd-snapshotter.service";
const OVERLAYBD_TCMU_SERVICE_PATH: &str = "/opt/overlaybd/overlaybd-tcmu.service";
const OVERLAYBD_LOG_PATH: &str = "/var/log/overlaybd.log";
const OVERLAYBD_CREATE_BIN: &str = "/opt/overlaybd/bin/overlaybd-create";
const OVERLAYBD_ATTACHER_BIN: &str = "/opt/overlaybd/snapshotter/overlaybd-attacher";
const WRITABLE_DATA_FILENAME: &str = "data.lsmt";
const WRITABLE_INDEX_FILENAME: &str = "index.lsmt";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A loop device that wraps a backing file or can represent any block device path.
///
/// This can be used to:
/// 1. Attach a disk image file to a loop device (/dev/loopX)
/// 2. Pass any block device path directly (e.g., /dev/sdX for overlaybd)
#[derive(Debug)]
pub struct LoopDevice {
    /// The block device path (e.g., /dev/loop0)
    device_path: Option<PathBuf>,

    // required for OverlayBDImage
    device_id: Option<String>,
}

/// A block device image that can be used as rootfs for a MicroVm.
///
/// BlockImage represents a sparse file formatted with a filesystem (typically ext4)
/// that can be exposed to the guest VM via virtio-blk.
#[derive(Debug)]
pub struct BlockImage {
    /// Path to the block image file.
    path: PathBuf,

    /// Size of the block image in GiB.
    size_gib: u64,

    /// Filesystem type (e.g., "ext4").
    filesystem: String,

    /// The attached loop device.
    loop_device: LoopDevice,
}

/// An OverlayBD block device image backed by overlaybd-tcmu.
///
/// Unlike a flat BlockImage, OverlayBD preserves the OCI layer structure
/// and exposes the image as a virtual block device (/dev/sdX) via TCMU.
#[derive(Debug)]
pub struct OverlayBDImage {
    /// directory where the rw file will be created
    rw_path: PathBuf,

    /// path to the overlaybd config file
    config_path: PathBuf,

    /// Size of the block image in GiB.
    size_gib: u64,

    /// Whether the rw layer is sparse file.
    is_sparse: bool,

    /// Filesystem type (e.g., "ext4").
    filesystem: String,

    /// The attached loop device.
    loop_device: LoopDevice,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl LoopDevice {
    pub fn new(device_path: Option<PathBuf>, device_id: Option<String>) -> Self {
        let device_id = device_id.map(|id| {
            if id.len() <= 13 {
                id
            } else {
                let hash = Sha256::digest(id.as_bytes());
                format!("{:013x}", u64::from_be_bytes(hash[..8].try_into().unwrap()) % 10u64.pow(13))
            }
        });
        Self { device_path, device_id }
    }

    /// Returns the block device path.
    pub fn device_path(&self) -> Option<&Path> {
        self.device_path.as_deref()
    }

    /// Creates a loop device from a backing file using losetup.
    pub async fn attach(&mut self, backing_file: &Path) -> MicrosandboxResult<()> {
        if self.device_path.is_some() {
            return Ok(());
        }

        let output = Command::new("losetup")
            .arg("-f")
            .arg("--show")
            .arg(backing_file)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute losetup: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "losetup failed: {}",
                stderr.trim()
            )));
        }

        let device_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        tracing::info!(
            backing_file = %backing_file.display(),
            device = %device_path,
            "loop device attached"
        );

        self.device_path = Some(PathBuf::from(device_path));

        Ok(())
    }

    pub async fn attach_overlaybd(&mut self, config_file: &Path) -> MicrosandboxResult<()> {
        if self.device_path.is_some() {
            return Ok(());
        }

        let device_id = self.device_id.as_ref().ok_or_else(|| {
            MicrosandboxError::BlockImageError("device_id is required for overlaybd attach".to_string())
        })?;

        let output = Command::new(OVERLAYBD_ATTACHER_BIN)
            .arg("attach")
            .arg("--id")
            .arg(device_id)
            .arg("--config")
            .arg(config_file)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute overlaybd-attacher: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "overlaybd-attacher attach failed: {}", stderr.trim()
            )));
        }

        // stdout contains lines like:
        //   INFO[0000] Device has been created. {id: 111: dev: /dev/sdb}
        //   /dev/sdb
        //   INFO[0000] device attached successfully: /dev/sdb
        // Extract the bare /dev/sdX line (no INFO prefix)
        let stdout = String::from_utf8_lossy(&output.stdout);
        let device_path = stdout
            .lines()
            .find(|line| line.starts_with("/dev/"))
            .ok_or_else(|| {
                MicrosandboxError::BlockImageError(format!(
                    "failed to parse device path from overlaybd-attacher output: {}", stdout.trim()
                ))
            })?
            .trim()
            .to_string();

        tracing::info!(
            id = %device_id,
            device = %device_path,
            "overlaybd device attached"
        );

        self.device_path = Some(PathBuf::from(device_path));

        Ok(())
    }

    pub async fn detach(&mut self) -> MicrosandboxResult<()> {
        if self.device_path.is_none() {
            return Ok(());
        }
        let device_path = self.device_path.as_ref().unwrap();

        let output = Command::new("losetup")
            .arg("-d")
            .arg(device_path)
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                tracing::info!(device = %device_path.display(), "loop device detached on VM exit");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(
                    device = %device_path.display(),
                    error = %stderr.trim(),
                    "failed to detach loop device on VM exit"
                );
            }
            Err(e) => {
                tracing::warn!(
                    device = %device_path.display(),
                    error = %e,
                    "failed to execute losetup -d on VM exit"
                );
            }
        }

        self.device_path = None;

        Ok(())
    }

    pub async fn detach_overlaybd(&mut self) -> MicrosandboxResult<()> {
        if self.device_path.is_none() {
            return Ok(());
        }

        let device_id = self.device_id.as_ref().ok_or_else(|| {
            MicrosandboxError::BlockImageError("device_id is required for overlaybd attach".to_string())
        })?;

        let output = Command::new(OVERLAYBD_ATTACHER_BIN)
            .arg("detach")
            .arg("--id")
            .arg(device_id)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute overlaybd-attacher: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "overlaybd-attacher detach failed: {}", stderr.trim()
            )));
        }

        tracing::info!(id = %device_id, "overlaybd device detached");

        self.device_path = None;

        Ok(())
    }
}

impl BlockImage {
    /// Creates a new BlockImage with the given path, size and filesystem type.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the block image file will be created
    /// * `size_gib` - Size of the block image in GiB
    /// * `filesystem` - Filesystem type (e.g., `"ext4"`)
    pub fn new(path: PathBuf, size_gib: u64, filesystem: impl Into<String>) -> Self {
        Self {
            path,
            size_gib,
            filesystem: filesystem.into(),
            loop_device: LoopDevice::new(None, None),
        }
    }

    /// Returns the path to the loop device if attached.
    pub fn loop_device_path(&self) -> Option<&Path> {
        self.loop_device.device_path()
    }

    /// Creates the block image by creating a sparse file and formatting it.
    ///
    /// This method performs three steps:
    /// 1. Creates a sparse file
    /// 2. Attaches the sparse file to a loop device
    /// 3. Formats the loop device with the configured filesystem
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The parent directory does not exist
    /// - Failed to create the sparse file
    /// - Failed to attach the sparse file to a loop device
    /// - Failed to format the filesystem
    pub async fn create(&mut self) -> MicrosandboxResult<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() { // self.path: pathbuf
            if !parent.exists() {
                return Err(MicrosandboxError::BlockImageError(format!(
                    "parent directory does not exist: {}",
                    parent.display()
                )));
            }
        }

        // Create sparse file
        self.create_sparse_file().await?;

        // Attach to loop device
        self.attach_loop().await?;

        // Format with filesystem
        self.format_filesystem().await?;

        tracing::info!(
            path = %self.path.display(),
            size_gib = self.size_gib,
            filesystem = %self.filesystem,
            "block image created successfully"
        );

        Ok(())
    }

    /// Creates a sparse file at the configured path with the configured size.
    ///
    /// Sparse files only allocate disk blocks for data that is actually written,
    /// making them ideal for VM disk images.
    async fn create_sparse_file(&self) -> MicrosandboxResult<()> {
        if self.path.exists() {
            return Err(MicrosandboxError::BlockImageError(format!(
                "sparse file already exists: {}",
                self.path.display()
            )));
        }

        let size_str = format!("{}G", self.size_gib);
        let output = Command::new("truncate")
            .arg("-s")
            .arg(&size_str)
            .arg(&self.path)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute truncate: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "truncate failed: {}",
                stderr.trim()
            )));
        }

        tracing::debug!(path = %self.path.display(), size_gib = self.size_gib, "sparse file created");

        Ok(())
    }

    /// Attaches the block image to a loop device.
    /// Returns a LoopDevice that can be passed to krun_add_disk.
    async fn attach_loop(&mut self) -> MicrosandboxResult<()> {
        let path = self.path.clone();
        self.loop_device.attach(&path).await?;
        Ok(())
    }

    pub async fn detach_loop(&mut self) -> MicrosandboxResult<()> {
        self.loop_device.detach().await?;
        Ok(())
    }

    /// Formats the block image with the configured filesystem.
    async fn format_filesystem(&self) -> MicrosandboxResult<()> {
        let mkfs_cmd = format!("mkfs.{}", self.filesystem);

        let loop_device_path = self.loop_device_path().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?;

        let output = Command::new(&mkfs_cmd)
            .arg(&loop_device_path)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute {}: {}", mkfs_cmd, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "{} failed: {}",
                mkfs_cmd,
                stderr.trim()
            )));
        }

        tracing::debug!(
            path = %loop_device_path.display(),
            filesystem = %self.filesystem,
            "filesystem formatted"
        );

        Ok(())
    }

    /// Attach an existing block image file to a loop device (no create/format).
    pub async fn from_existing(image_path: Option<PathBuf>, device_path: Option<PathBuf>) -> MicrosandboxResult<Self> {
        match (image_path, device_path) {
            (Some(image_path), None) => {
                let mut loop_device = LoopDevice::new(None, None);
                loop_device.attach(&image_path).await?;
                Ok(Self {
                    path: image_path,
                    size_gib: 0,
                    filesystem: String::new(),
                    loop_device,
                })
            }
            (None, Some(device_path)) => {
                Ok(Self {
                    path: PathBuf::new(),
                    size_gib: 0,
                    filesystem: String::new(),
                    loop_device: LoopDevice::new(Some(device_path), None),
                })
            }
            (Some(_), Some(_)) => Err(MicrosandboxError::BlockImageError(
                "only one of image_path or device_path should be provided".to_string(),
            )),
            (None, None) => Err(MicrosandboxError::BlockImageError(
                "one of image_path or device_path must be provided".to_string(),
            )),
        }
    }

    /// Populates the block image with content from layer directories.
    /// Layers are copied in order (base layer first, top layer last).
    pub async fn populate_from_layers(&self, layers: &[PathBuf]) -> MicrosandboxResult<()> {
        if layers.is_empty() {
            return Ok(());
        }

        // Create temp mount point
        let mount_point = tempfile::tempdir().map_err(|e| {
            MicrosandboxError::BlockImageError(format!("failed to create mount point: {}", e))
        })?;
        let mount_path = mount_point.path();

        // Mount the block image
        self.mount(mount_path).await?;

        // Copy each layer in order
        let result = copy_layers(mount_path, layers).await;

        // Restore permissions from xattr if copy succeeded
        let restore_result = if result.is_ok() {
            restore_permissions_from_xattr(mount_path).await
        } else {
            Ok(())
        };

        // Always try to unmount
        let unmount_result = self.unmount(mount_path).await;

        // Return first error if any
        result?;
        restore_result?;
        unmount_result?;

        tracing::info!(layer_count = layers.len(), "layers populated into block image");
        Ok(())
    }

    /// Mount the block image to target path.
    pub async fn mount(&self, target: &Path) -> MicrosandboxResult<()> {
        let loop_device_path = self.loop_device_path().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?;
        let output = Command::new("mount")
            .arg(loop_device_path)
            .arg(target)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute mount: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "mount failed: {}",
                stderr.trim()
            )));
        }

        tracing::debug!(target = %target.display(), "block image mounted");
        Ok(())
    }

    /// Unmount the block image from target path.
    pub async fn unmount(&self, target: &Path) -> MicrosandboxResult<()> {
        let output = Command::new("umount")
            .arg(target)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute umount: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "umount failed: {}",
                stderr.trim()
            )));
        }

        tracing::debug!(target = %target.display(), "block image unmounted");
        Ok(())
    }
}

impl OverlayBDImage {
    pub fn new(rw_path: PathBuf, config_path: PathBuf, size_gib: u64, is_sparse: bool, filesystem: impl Into<String>, device_id: String) -> Self {
        Self {
            rw_path,
            config_path,
            size_gib,
            is_sparse,
            filesystem: filesystem.into(),
            loop_device: LoopDevice::new(None, Some(device_id)),
        }
    }

    /// Returns the path to the loop device if attached.
    pub fn loop_device_path(&self) -> Option<&Path> {
        self.loop_device.device_path()
    }

    pub async fn start_service() -> MicrosandboxResult<()> {
        // Clear overlaybd log
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("cat /dev/null > {}", OVERLAYBD_LOG_PATH))
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to clear overlaybd log: {}", e))
            })?;
        if !output.status.success() {
            tracing::warn!("failed to clear overlaybd log, continuing anyway");
        }

        // Enable and start snapshotter
        let output = Command::new("systemctl")
            .arg("enable")
            .arg(OVERLAYBD_SNAPSHOTTER_SERVICE_PATH)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to enable snapshotter: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl enable snapshotter failed: {}", stderr.trim()
            )));
        }

        let output = Command::new("systemctl")
            .arg("start")
            .arg(OVERLAYBD_SNAPSHOTTER_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to start snapshotter: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl start snapshotter failed: {}", stderr.trim()
            )));
        }

        // Enable and start tcmu
        let output = Command::new("systemctl")
            .arg("enable")
            .arg(OVERLAYBD_TCMU_SERVICE_PATH)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to enable tcmu: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl enable tcmu failed: {}", stderr.trim()
            )));
        }

        let output = Command::new("systemctl")
            .arg("start")
            .arg(OVERLAYBD_TCMU_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to start tcmu: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl start tcmu failed: {}", stderr.trim()
            )));
        }

        tracing::info!("overlaybd services started");
        Ok(())
    }

    pub async fn stop_service() -> MicrosandboxResult<()> {
        // Stop and disable snapshotter
        let output = Command::new("systemctl")
            .arg("stop")
            .arg(OVERLAYBD_SNAPSHOTTER_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to stop snapshotter: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl stop snapshotter failed: {}", stderr.trim()
            )));
        }

        let output = Command::new("systemctl")
            .arg("disable")
            .arg(OVERLAYBD_SNAPSHOTTER_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to disable snapshotter: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl disable snapshotter failed: {}", stderr.trim()
            )));
        }

        // Stop and disable tcmu
        let output = Command::new("systemctl")
            .arg("stop")
            .arg(OVERLAYBD_TCMU_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to stop tcmu: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl stop tcmu failed: {}", stderr.trim()
            )));
        }

        let output = Command::new("systemctl")
            .arg("disable")
            .arg(OVERLAYBD_TCMU_SERVICE)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to disable tcmu: {}", e))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "systemctl disable tcmu failed: {}", stderr.trim()
            )));
        }

        tracing::info!("overlaybd services stopped");
        Ok(())
    }

    pub async fn create(&mut self) -> MicrosandboxResult<()> {
        // Create rw layer
        self.create_rw().await?;

        // Attach a device
        self.attach_loop().await?;

        // Format with filesystem
        self.format_filesystem().await?;

        tracing::info!(
            rw_path = %self.rw_path.display(),
            config_path = %self.config_path.display(),
            size_gib = self.size_gib,
            is_sparse = self.is_sparse,
            filesystem = %self.filesystem,
            "overlaybd image created successfully"
        );

        Ok(())
    }

    async fn create_rw(&mut self) -> MicrosandboxResult<()> {
        let data_path = self.rw_path.join(WRITABLE_DATA_FILENAME);
        let index_path = self.rw_path.join(WRITABLE_INDEX_FILENAME);

        // Check if files already exist
        if data_path.exists() || index_path.exists() {
            return Err(MicrosandboxError::BlockImageError(format!(
                "writable layer files already exist in {}",
                self.rw_path.display()
            )));
        }

        let mut cmd = Command::new(OVERLAYBD_CREATE_BIN);
        cmd.arg(&data_path)
            .arg(&index_path)
            .arg(self.size_gib.to_string());

        if self.is_sparse {
            cmd.arg("-s");
        }

        let output = cmd.output().await.map_err(|e| {
            MicrosandboxError::BlockImageError(format!("failed to execute overlaybd-create: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "overlaybd-create failed: {}", stderr.trim()
            )));
        }

        tracing::info!(
            data = %data_path.display(),
            index = %index_path.display(),
            size_gib = self.size_gib,
            sparse = self.is_sparse,
            "writable layer created"
        );

        if !self.config_path.exists() {
            return Err(MicrosandboxError::BlockImageError(format!(
                "config file does not exist: {}",
                self.config_path.display()
            )));
        }
        let rw_config_path = self.rw_path.join(OVERLAYBD_CONFIG_FILENAME);
        overlaybd::update_overlaybd_config_upper(
            &self.config_path,
            &rw_config_path,
            &data_path,
            &index_path,
            self.size_gib,
        )
        .await?;
        self.config_path = rw_config_path;

        Ok(())
    }

    async fn attach_loop(&mut self) -> MicrosandboxResult<()> {
        let config_path = self.config_path.clone();
        self.loop_device.attach_overlaybd(&config_path).await?;
        Ok(())
    }

    pub async fn detach_loop(&mut self) -> MicrosandboxResult<()> {
        self.loop_device.detach_overlaybd().await?;
        Ok(())
    }

    async fn format_filesystem(&mut self) -> MicrosandboxResult<()> {
        let loop_device_path = self.loop_device_path().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?.to_path_buf();

        // Check if a filesystem already exists
        let probe = Command::new("blkid").arg("-o").arg("value").arg("-s").arg("TYPE")
            .arg(&loop_device_path)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute blkid: {}", e))
            })?;

        let existing_fs = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        if !existing_fs.is_empty() {
            if existing_fs != self.filesystem {
                tracing::warn!(
                    expected = %self.filesystem,
                    found = %existing_fs,
                    "filesystem type mismatch, using existing"
                );
                self.filesystem = existing_fs;
            }
            tracing::debug!(
                path = %loop_device_path.display(),
                filesystem = %self.filesystem,
                "filesystem already exists, skipping format"
            );
            return Ok(());
        }
        else {
            tracing::debug!(
                "no existing filesystem found, formatting"
            );
        }

        // Format with filesystem
        let mkfs_cmd = format!("mkfs.{}", self.filesystem);

        let output = Command::new(&mkfs_cmd)
            .arg(&loop_device_path)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute {}: {}", mkfs_cmd, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "{} failed: {}",
                mkfs_cmd,
                stderr.trim()
            )));
        }

        tracing::debug!(
            path = %loop_device_path.display(),
            filesystem = %self.filesystem,
            "filesystem formatted"
        );

        Ok(())
    }

    pub async fn from_existing(config_path: Option<PathBuf>, device_path: Option<PathBuf>, device_id: String) -> MicrosandboxResult<Self> {
        match (config_path, device_path) {
            (Some(config_path), None) => {
                if !config_path.exists() {
                    return Err(MicrosandboxError::BlockImageError(format!(
                        "rw config file does not exist: {}",
                        config_path.display()
                    )));
                }
                let mut loop_device = LoopDevice::new(None, Some(device_id));
                loop_device.attach_overlaybd(&config_path).await?;
                Ok(Self {
                    rw_path: PathBuf::new(),
                    config_path: config_path,
                    size_gib: 0,
                    is_sparse: false,
                    filesystem: String::new(),
                    loop_device,
                })
            }
            (None, Some(device_path)) => {
                Ok(Self {
                    rw_path: PathBuf::new(),
                    config_path: PathBuf::new(),
                    size_gib: 0,
                    is_sparse: false,
                    filesystem: String::new(),
                    loop_device: LoopDevice::new(Some(device_path), Some(device_id)),
                })
            }
            (Some(_), Some(_)) => Err(MicrosandboxError::BlockImageError(
                "only one of image_path or device_path should be provided".to_string(),
            )),
            (None, None) => Err(MicrosandboxError::BlockImageError(
                "one of image_path or device_path must be provided".to_string(),
            )),
        }
    }

    pub async fn mount(&self, target: &Path) -> MicrosandboxResult<()> {
        let loop_device_path = self.loop_device_path().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?;

        let output = Command::new("mount")
            .arg(loop_device_path)
            .arg(target)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute mount: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "mount failed: {}",
                stderr.trim()
            )));
        }

        tracing::debug!(target = %target.display(), "overlaybd image mounted");
        Ok(())
    }

    pub async fn unmount(&self, target: &Path) -> MicrosandboxResult<()> {
        let output = Command::new("umount")
            .arg(target)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute umount: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "umount failed: {}",
                stderr.trim()
            )));
        }

        tracing::debug!(target = %target.display(), "overlaybd image unmounted");
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Copy layer contents to mount point using cp -a.
async fn copy_layers(mount_point: &Path, layers: &[PathBuf]) -> MicrosandboxResult<()> {
    for layer in layers {
        if !layer.exists() {
            return Err(MicrosandboxError::BlockImageError(format!(
                "layer not found: {}",
                layer.display()
            )));
        }

        // Use cp -a to preserve attributes and copy recursively
        // cp -a layer/. mount_point/ (the "/." copies contents, not directory itself)
        let src = format!("{}/.", layer.display());
        let output = Command::new("cp")
            .arg("-a")
            .arg(&src)
            .arg(mount_point)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute cp: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "cp failed for layer {}: {}",
                layer.display(),
                stderr.trim()
            )));
        }

        tracing::debug!(layer = %layer.display(), "layer copied");
    }

    Ok(())
}

/// Restore file permissions from xattr stored during OCI layer extraction.
///
/// OCI layers store original permissions in `user.containers.override_stat` xattr
/// in format `uid:gid:mode`. This function reads those and applies the mode.
async fn restore_permissions_from_xattr(mount_point: &Path) -> MicrosandboxResult<()> {
    const XATTR_NAME: &str = "user.containers.override_stat";

    fn restore_permissions_recursive(path: &Path) -> MicrosandboxResult<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                // Skip files that don't exist or can't be accessed
                tracing::trace!(path = %path.display(), error = %e, "skipping file during permission restore");
                return Ok(());
            }
        };
        let file_type = metadata.file_type();

        // Try to read xattr and restore permissions
        if let Ok(Some(xattr_value)) = xattr::get(path, XATTR_NAME) {
            let xattr_str = String::from_utf8_lossy(&xattr_value);
            // Format: "uid:gid:mode" where mode is octal like "0:0:0755"
            if let Some(mode_str) = xattr_str.split(':').nth(2) {
                if let Ok(mode) = u32::from_str_radix(mode_str, 8) {
                    let permission_bits = mode & 0o7777;
                    if permission_bits != 0 {
                        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(permission_bits)) {
                            tracing::trace!(
                                path = %path.display(),
                                mode = format!("{:o}", permission_bits),
                                error = %e,
                                "failed to restore permission from xattr"
                            );
                        } else {
                            tracing::trace!(
                                path = %path.display(),
                                mode = format!("{:o}", permission_bits),
                                "restored permission from xattr"
                            );
                        }
                    }
                }
            }
        }

        // Recursively process directories (but not symlinks to avoid cycles)
        if file_type.is_dir() && !file_type.is_symlink() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    restore_permissions_recursive(&entry.path())?;
                }
            }
        }

        Ok(())
    }

    tracing::debug!("restoring permissions from xattr");
    restore_permissions_recursive(mount_point)?;
    tracing::info!("restored permissions from xattr for block image");
    Ok(())
}
