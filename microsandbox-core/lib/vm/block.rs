//! Block device image management for MicroVm rootfs.
//!
//! This module provides functionality for creating and managing block device images
//! that can be used as rootfs for MicroVMs via virtio-blk.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::process::Command;

use crate::{MicrosandboxError, MicrosandboxResult};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Default block image size in GiB.
pub const DEFAULT_BLOCK_IMAGE_SIZE_GIB: u64 = 20;

/// Default filesystem type for block images.
pub const DEFAULT_FILESYSTEM_TYPE: &str = "ext4";

//--------------------------------------------------------------------------------------------------
// Types: LoopDevice
//--------------------------------------------------------------------------------------------------

/// A loop device that wraps a backing file or can represent any block device path.
///
/// This can be used to:
/// 1. Attach a disk image file to a loop device (/dev/loopX)
/// 2. Pass any block device path directly (e.g., /dev/sdX for overlaybd)
#[derive(Debug)]
pub struct LoopDevice {
    /// The block device path (e.g., /dev/loop0)
    device_path: PathBuf,
    /// Whether this is a loop device we created (needs cleanup) or external device
    owned: bool,
}

//--------------------------------------------------------------------------------------------------
// Types: BlockImage
//--------------------------------------------------------------------------------------------------

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
    loop_device: Option<LoopDevice>,
}

/// Builder for creating a BlockImage with custom configuration.
#[derive(Debug, Default)]
pub struct BlockImageBuilder {
    path: Option<PathBuf>,
    size_gib: Option<u64>,
    filesystem: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl LoopDevice {
    /// Creates a loop device from a backing file using losetup.
    pub async fn attach(backing_file: &Path) -> MicrosandboxResult<Self> {
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

        Ok(Self {
            device_path: PathBuf::from(device_path),
            owned: true,
        })
    }

    /// Creates a LoopDevice wrapper for an existing block device path.
    /// Useful for overlaybd or other external block devices.
    /// The device will NOT be detached on drop.
    pub fn from_existing(device_path: PathBuf) -> Self {
        Self {
            device_path,
            owned: false,
        }
    }

    /// Returns the block device path.
    pub fn path(&self) -> &Path {
        &self.device_path
    }

    /// Detaches the loop device (only if we created it).
    pub async fn detach(&self) -> MicrosandboxResult<()> {
        if !self.owned {
            return Ok(());
        }

        let output = Command::new("losetup")
            .arg("-d")
            .arg(&self.device_path)
            .output()
            .await
            .map_err(|e| {
                MicrosandboxError::BlockImageError(format!("failed to execute losetup -d: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MicrosandboxError::BlockImageError(format!(
                "losetup -d failed: {}",
                stderr.trim()
            )));
        }

        tracing::info!(device = %self.device_path.display(), "loop device detached");
        Ok(())
    }
}

impl BlockImage {
    /// Creates a new BlockImageBuilder for configuring a block image.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use microsandbox_core::vm::BlockImage;
    /// use std::path::PathBuf;
    ///
    /// let image = BlockImage::builder()
    ///     .path(PathBuf::from("/tmp/rootfs.img"))
    ///     .size_gib(20)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn builder() -> BlockImageBuilder {
        BlockImageBuilder::default()
    }

    /// Returns the path to the block image file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the size of the block image in GiB.
    pub fn size_gib(&self) -> u64 {
        self.size_gib
    }

    /// Returns the filesystem type.
    pub fn filesystem(&self) -> &str {
        &self.filesystem
    }

    /// Returns the path to the loop device if attached.
    pub fn loop_device_path(&self) -> Option<&Path> {
        self.loop_device.as_ref().map(|ld| ld.path())
    }

    /// Creates the block image by creating a sparse file and formatting it.
    ///
    /// This method performs two steps:
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use microsandbox_core::vm::BlockImage;
    /// use std::path::PathBuf;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let mut image = BlockImage::builder()
    ///         .path(PathBuf::from("/tmp/rootfs.img"))
    ///         .size_gib(20)
    ///         .build()?;
    ///
    ///     image.create().await?;
    ///     Ok(())
    /// }
    /// ```
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
        tracing::debug!(
            path = %self.path.display(),
            size_gib = self.size_gib,
            "creating sparse file"
        );

        if self.path.exists() {
            tokio::fs::remove_file(&self.path).await?;
            tracing::debug!(path = %self.path.display(), "removed existing sparse file");
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

        tracing::debug!(path = %self.path.display(), "sparse file created");

        Ok(())
    }

    /// Attaches the block image to a loop device.
    /// Returns a LoopDevice that can be passed to krun_add_disk.
    async fn attach_loop(&mut self) -> MicrosandboxResult<()> {
        self.loop_device = Some(LoopDevice::attach(&self.path).await?);
        Ok(())
    }

    /// Formats the block image with the configured filesystem.
    async fn format_filesystem(&self) -> MicrosandboxResult<()> {
        let mkfs_cmd = format!("mkfs.{}", self.filesystem);

        let loop_device = self.loop_device.as_ref().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?;

        tracing::debug!(
            path = %loop_device.path().display(),
            filesystem = %self.filesystem,
            "formatting filesystem"
        );

        let output = Command::new(&mkfs_cmd)
            .arg(&loop_device.path())
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
            path = %loop_device.path().display(),
            filesystem = %self.filesystem,
            "filesystem formatted"
        );

        Ok(())
    }

    /// Attach an existing block image file to a loop device (no create/format).
    pub async fn from_existing(path: PathBuf) -> MicrosandboxResult<Self> {
        let loop_device = LoopDevice::attach(&path).await?;
        Ok(Self {
            path,
            size_gib: 0,       // irrelevant for existing image
            filesystem: String::new(),
            loop_device: Some(loop_device),
        })
    }

    /// Detaches the loop device if it is attached.
    #[allow(dead_code)]
    async fn detach_loop(&mut self) -> MicrosandboxResult<()> {
        if let Some(ref loop_device) = self.loop_device {
            loop_device.detach().await?;
            tracing::info!(path = %loop_device.path().display(), "loop device detached");
        }
        self.loop_device = None;
        Ok(())
    }

    /// Removes the block image file if it exists.
    #[allow(dead_code)]
    pub async fn remove(&self) -> MicrosandboxResult<()> {
        if self.path.exists() {
            if self.loop_device.is_some() {
                return Err(MicrosandboxError::BlockImageError(
                    "cannot remove block image: loop device is still attached, call detach_loop() first".to_string()
                ));
            }
            tokio::fs::remove_file(&self.path).await?;
            tracing::info!(path = %self.path.display(), "block image removed");
        }
        Ok(())
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
        let result = self.copy_layers(mount_path, layers).await;

        // Restore permissions from xattr if copy succeeded
        let restore_result = if result.is_ok() {
            self.restore_permissions_from_xattr(mount_path).await
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
    async fn mount(&self, target: &Path) -> MicrosandboxResult<()> {
        let loop_device = self.loop_device.as_ref().ok_or_else(|| {
            MicrosandboxError::BlockImageError("loop device not attached".to_string())
        })?;

        let output = Command::new("mount")
            .arg(&loop_device.path())
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
    async fn unmount(&self, target: &Path) -> MicrosandboxResult<()> {
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

    /// Copy layer contents to mount point using cp -a.
    async fn copy_layers(&self, mount_point: &Path, layers: &[PathBuf]) -> MicrosandboxResult<()> {
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
    async fn restore_permissions_from_xattr(&self, mount_point: &Path) -> MicrosandboxResult<()> {
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
}

impl BlockImageBuilder {
    /// Sets the path for the block image file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the block image file will be created.
    pub fn path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// Sets the size of the block image in GiB.
    ///
    /// # Arguments
    ///
    /// * `size` - The size in GiB. Defaults to 20 GiB if not specified.
    pub fn size_gib(mut self, size: u64) -> Self {
        self.size_gib = Some(size);
        self
    }

    /// Sets the filesystem type for the block image.
    ///
    /// # Arguments
    ///
    /// * `filesystem` - The filesystem type (e.g., "ext4", "xfs"). Defaults to "ext4".
    pub fn filesystem(mut self, filesystem: impl Into<String>) -> Self {
        self.filesystem = Some(filesystem.into());
        self
    }

    /// Builds the BlockImage with the configured options.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not specified.
    pub fn build(self) -> MicrosandboxResult<BlockImage> {
        let path = self.path.ok_or_else(|| {
            MicrosandboxError::BlockImageError("block image path is required".to_string())
        })?;

        Ok(BlockImage {
            path,
            size_gib: self.size_gib.unwrap_or(DEFAULT_BLOCK_IMAGE_SIZE_GIB),
            filesystem: self.filesystem.unwrap_or_else(|| DEFAULT_FILESYSTEM_TYPE.to_string()),
            loop_device: None,
        })
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // ==================== BlockImageBuilder Tests ====================

    #[test]
    fn test_block_image_builder_defaults() {
        let path = PathBuf::from("/tmp/test.img");
        let image = BlockImage::builder().path(path.clone()).build().unwrap();

        assert_eq!(image.path(), path);
        assert_eq!(image.size_gib(), DEFAULT_BLOCK_IMAGE_SIZE_GIB);
        assert_eq!(image.filesystem(), DEFAULT_FILESYSTEM_TYPE);
    }

    #[test]
    fn test_block_image_builder_custom() {
        let path = PathBuf::from("/tmp/custom.img");
        let image = BlockImage::builder()
            .path(path.clone())
            .size_gib(100)
            .filesystem("ext3")
            .build()
            .unwrap();

        assert_eq!(image.path(), path);
        assert_eq!(image.size_gib(), 100);
        assert_eq!(image.filesystem(), "ext3");
    }

    #[test]
    fn test_block_image_builder_missing_path() {
        let result = BlockImage::builder().size_gib(20).build();

        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(MicrosandboxError::BlockImageError(msg)) if msg.contains("path is required")
        ));
    }

    // ==================== LoopDevice Tests ====================

    #[test]
    fn test_loop_device_from_existing() {
        let device_path = PathBuf::from("/dev/sda1");
        let loop_device = LoopDevice::from_existing(device_path.clone());

        assert_eq!(loop_device.path(), device_path);
        assert!(!loop_device.owned);
    }

    #[tokio::test]
    async fn test_loop_device_detach_unowned() {
        // Detaching an unowned device should be a no-op
        let device_path = PathBuf::from("/dev/fake_device");
        let loop_device = LoopDevice::from_existing(device_path);

        // Should succeed without actually calling losetup
        let result = loop_device.detach().await;
        assert!(result.is_ok());
    }

    // ==================== Integration Tests (require root) ====================

    #[tokio::test]
    // #[ignore = "requires root privileges to run losetup and mkfs"]
    async fn test_block_image_create() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let image_path = temp_dir.path().join("test.img");

        let mut image = BlockImage::builder()
            .path(image_path.clone())
            .size_gib(1)
            .build()?;

        image.create().await?;

        // Verify sparse file was created
        assert!(image.path().exists());
        // Verify loop device is attached
        assert!(image.loop_device_path().is_some());
        assert!(image.loop_device_path().map(|p| p.exists()).unwrap_or(false));

        // Clean up: detach loop device first, then remove file
        image.detach_loop().await?;
        image.remove().await?;
        assert!(!image_path.exists());

        Ok(())
    }

    #[tokio::test]
    // #[ignore = "requires root privileges to run losetup, mkfs, and mount"]
    async fn test_block_image_populate_from_layers() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let image_path = temp_dir.path().join("layers_test.img");

        // Create layer directories with test content
        let layer1_dir = temp_dir.path().join("layer1");
        let layer2_dir = temp_dir.path().join("layer2");

        tokio::fs::create_dir_all(&layer1_dir).await?;
        tokio::fs::create_dir_all(&layer2_dir).await?;

        // Add test files to layers
        tokio::fs::write(layer1_dir.join("base.txt"), "base layer content").await?;
        tokio::fs::write(layer2_dir.join("top.txt"), "top layer content").await?;

        let mut image = BlockImage::builder()
            .path(image_path.clone())
            .size_gib(1)
            .build()?;

        image.create().await?;

        let layers = vec![layer1_dir, layer2_dir];
        image.populate_from_layers(&layers).await?;

        // Clean up: detach loop device first, then remove file
        image.detach_loop().await?;
        image.remove().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_block_image_create_parent_not_exists() {
        let mut image = BlockImage::builder()
            .path(PathBuf::from("/non/existent/parent/test.img"))
            .build()
            .unwrap();

        let result = image.create().await;

        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(MicrosandboxError::BlockImageError(msg)) if msg.contains("parent directory does not exist")
        ));
    }

    #[tokio::test]
    async fn test_block_image_remove_nonexistent() {
        let temp_dir = tempdir().unwrap();
        let image_path = temp_dir.path().join("nonexistent.img");

        let image = BlockImage::builder()
            .path(image_path.clone())
            .build()
            .unwrap();

        // Should succeed even if file doesn't exist
        let result = image.remove().await;
        assert!(result.is_ok());
    }
}
