//! Launcher-owned private files, inheritable only by the gated child.
//!
//! The type lives in this crate because the builder's process tools are
//! declared here, so the tools' descriptor-capable spawn can name it without
//! inverting the crate dependency (`mllm-launchers` depends on this crate and
//! re-exports it under its own name).
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};

#[derive(Debug, thiserror::Error)]
pub enum ProtectedDescriptorError {
    #[error("protected descriptor creation failed: {0}")]
    Create(String),
}

/// Launcher-owned private files. No formatting or serialization surface exposes
/// their contents; the descriptors close when this non-cloneable value is dropped.
pub struct ProtectedLaunchDescriptors {
    files: [std::fs::File; 3],
}

impl ProtectedLaunchDescriptors {
    pub fn new(
        launch: &[u8],
        inference: &[u8],
        admin: &[u8],
    ) -> Result<Self, ProtectedDescriptorError> {
        let credential = |bytes: &[u8]| {
            !bytes.is_empty() && bytes.len() <= 4096 && bytes.iter().all(|b| (33..=126).contains(b))
        };
        if launch.is_empty()
            || launch.len() > 65536
            || !credential(inference)
            || !credential(admin)
            || inference == admin
        {
            return Err(ProtectedDescriptorError::Create(
                "invalid protected descriptors".into(),
            ));
        }
        fn file(bytes: &[u8]) -> std::io::Result<std::fs::File> {
            use nix::fcntl::{FcntlArg, SealFlag, fcntl};
            use nix::sys::memfd::{MemFdCreateFlag, memfd_create};
            let fd = memfd_create(
                c"mllm-private-launch",
                MemFdCreateFlag::MFD_CLOEXEC | MemFdCreateFlag::MFD_ALLOW_SEALING,
            )?;
            nix::sys::stat::fchmod(
                fd.as_raw_fd(),
                nix::sys::stat::Mode::from_bits_truncate(0o600),
            )?;
            // FD 9 belongs to the initialization gate. Duplication also ensures
            // no source descriptor can be overwritten while that gate is installed.
            let number = fcntl(fd.as_raw_fd(), FcntlArg::F_DUPFD_CLOEXEC(10))?;
            let mut file = unsafe { std::fs::File::from_raw_fd(number) };
            file.write_all(bytes)?;
            file.rewind()?;
            fcntl(
                number,
                FcntlArg::F_ADD_SEALS(
                    SealFlag::F_SEAL_WRITE
                        | SealFlag::F_SEAL_GROW
                        | SealFlag::F_SEAL_SHRINK
                        | SealFlag::F_SEAL_SEAL,
                ),
            )?;
            Ok(file)
        }
        let files = [launch, inference, admin].map(file);
        let [launch, inference, admin] = files;
        let sanitized = |_| ProtectedDescriptorError::Create(std::io::Error::last_os_error().to_string());
        Ok(Self {
            files: [
                launch.map_err(sanitized)?,
                inference.map_err(sanitized)?,
                admin.map_err(sanitized)?,
            ],
        })
    }

    pub fn numbers(&self) -> [i32; 3] {
        self.files.each_ref().map(AsRawFd::as_raw_fd)
    }
}
