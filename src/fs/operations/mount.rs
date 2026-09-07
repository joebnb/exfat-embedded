//! Filesystem construction and block-device ownership accessors.

use crate::{BlockDevice, Error, FileSystem, Geometry, Scratch, Volume};

impl<D: BlockDevice> FileSystem<D> {
    /// Mount `device`, retaining it as this filesystem's exclusive backend.
    pub fn mount(mut device: D, scratch: &mut Scratch<'_>) -> Result<Self, Error<D::Error>> {
        let volume = Volume::mount(&mut device, scratch)?;
        Ok(Self { device, volume })
    }

    /// Mount `device`, returning it on failure so removable media can be
    /// retried after a transient boot-sector read failure.
    #[inline(never)]
    pub fn mount_recoverable(
        mut device: D,
        scratch: &mut Scratch<'_>,
    ) -> Result<Self, (D, Error<D::Error>)> {
        match Volume::mount(&mut device, scratch) {
            Ok(volume) => Ok(Self { device, volume }),
            Err(error) => Err((device, error)),
        }
    }

    /// Parsed geometry of the mounted volume.
    pub fn geometry(&self) -> Geometry {
        self.volume.geometry()
    }

    /// Borrow the underlying block device.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Mutably borrow the underlying block device.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Consume the filesystem and return its block device.
    pub fn into_device(self) -> D {
        self.device
    }
}
