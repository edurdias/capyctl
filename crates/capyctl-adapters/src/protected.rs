//! Launcher-owned private files, inheritable only by the gated child.
//!
//! The type lives in this crate because the builder's process tools are
//! declared here, so the tools' descriptor-capable spawn can name it without
//! inverting the crate dependency (`capyctl-launchers` depends on this crate and
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
    /// The private launch descriptor, then the inference and admin credentials
    /// unless the launch carries none (`launch_only`).
    files: Vec<std::fs::File>,
}

/// A credential's shape: 1 to 4096 printable, non-space ASCII bytes.
fn credential(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() <= 4096 && bytes.iter().all(|b| (33..=126).contains(b))
}

impl ProtectedLaunchDescriptors {
    pub fn new(
        launch: &[u8],
        inference: &[u8],
        admin: &[u8],
    ) -> Result<Self, ProtectedDescriptorError> {
        if !credential(inference) || !credential(admin) || inference == admin {
            return Err(invalid());
        }
        Self::files(&[launch, inference, admin])
    }

    /// ADR 0028 §10, ADR 0012: a group worker serves no API and is handed no
    /// API credential: the private launch descriptor, then, when its
    /// residency is deep, its own observation credential (ADR 0028 §12).
    pub fn for_worker(
        launch: &[u8],
        observation: Option<&[u8]>,
    ) -> Result<Self, ProtectedDescriptorError> {
        match observation {
            Some(observation) if credential(observation) => Self::files(&[launch, observation]),
            Some(_) => Err(invalid()),
            None => Self::files(&[launch]),
        }
    }

    fn files(contents: &[&[u8]]) -> Result<Self, ProtectedDescriptorError> {
        let launch = contents[0];
        if launch.is_empty() || launch.len() > 65536 {
            return Err(invalid());
        }
        fn file(bytes: &[u8]) -> std::io::Result<std::fs::File> {
            use nix::fcntl::{fcntl, FcntlArg, SealFlag};
            use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
            let fd = memfd_create(
                c"capyctl-private-launch",
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
        let sanitized = |e: std::io::Error| ProtectedDescriptorError::Create(e.to_string());
        let files = contents
            .iter()
            .map(|bytes| file(bytes).map_err(sanitized))
            .collect::<Result<_, _>>()?;
        Ok(Self { files })
    }

    /// The inherited descriptor numbers, in the order the entry names them.
    pub fn numbers(&self) -> Vec<i32> {
        self.files.iter().map(AsRawFd::as_raw_fd).collect()
    }

    /// SPEC §13.3 / T21: hand the credentials (every descriptor after the
    /// launch descriptor) to the engine log's redacting writer, which replaces
    /// them by value wherever the engine echoes them. They go nowhere else.
    pub fn own_credentials(
        &self,
        redactor: &mut crate::engine_log::LogRedactor,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        for file in self.files.iter().skip(1) {
            let mut bytes = vec![0; file.metadata()?.len() as usize];
            file.read_exact_at(&mut bytes, 0)?;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                redactor.own(text);
            }
        }
        Ok(())
    }
}

fn invalid() -> ProtectedDescriptorError {
    ProtectedDescriptorError::Create("invalid protected descriptors".into())
}
