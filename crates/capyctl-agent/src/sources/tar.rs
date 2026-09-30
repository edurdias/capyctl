//! ADR 0008: extraction of a verified `http` tar archive.
//!
//! A deliberately small POSIX (ustar/v7) reader. Only regular files and
//! directories are extracted, under relative paths without `..`; links,
//! devices, extended headers and anything else refuse the whole archive
//! (`unsafe_archive`). The archive's digest is verified before this runs, so
//! the reader guards the store against a well-formed but hostile archive, not
//! against transport corruption.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;

/// Why an archive was not extracted.
#[derive(Debug)]
pub enum TarError {
    /// The archive holds something capyctl will not write into a model store.
    Unsafe,
    Io(#[allow(dead_code)] io::Error),
}

impl From<io::Error> for TarError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn octal(field: &[u8]) -> Result<u64, TarError> {
    let text = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect::<String>();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| TarError::Unsafe)
}

fn text(field: &[u8]) -> Result<String, TarError> {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| TarError::Unsafe)
}

fn safe_relative(name: &str) -> Result<PathBuf, TarError> {
    let path = Path::new(name);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(TarError::Unsafe),
        }
    }
    if out.as_os_str().is_empty() || name.contains('\\') || name.chars().any(char::is_control) {
        return Err(TarError::Unsafe);
    }
    Ok(out)
}

/// Extract `archive` into `target` (created, must not exist). Returns the
/// bytes of regular files written. `limit` bounds the total written.
pub fn extract(archive: &Path, target: &Path, limit: u64) -> Result<u64, TarError> {
    let mut input = io::BufReader::new(File::open(archive)?);
    fs::create_dir(target)?;
    let mut written = 0_u64;
    let mut header = [0_u8; BLOCK];
    let mut zero_blocks = 0;
    loop {
        if let Err(error) = input.read_exact(&mut header) {
            return if error.kind() == io::ErrorKind::UnexpectedEof && zero_blocks > 0 {
                Ok(written)
            } else {
                Err(TarError::Unsafe)
            };
        }
        if header.iter().all(|b| *b == 0) {
            zero_blocks += 1;
            if zero_blocks == 2 {
                return Ok(written);
            }
            continue;
        }
        zero_blocks = 0;
        // The header checksum treats its own field as spaces.
        let stored = octal(&header[148..156])?;
        let computed: u64 = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*b)
                }
            })
            .sum();
        if stored != computed {
            return Err(TarError::Unsafe);
        }
        let mut name = text(&header[0..100])?;
        if &header[257..262] == b"ustar" {
            let prefix = text(&header[345..500])?;
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let size = octal(&header[124..136])?;
        let relative = safe_relative(&name)?;
        let destination = target.join(&relative);
        match header[156] {
            b'0' | 0 => {
                written = written.checked_add(size).ok_or(TarError::Unsafe)?;
                if written > limit {
                    return Err(TarError::Unsafe);
                }
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::AlreadyExists => TarError::Unsafe,
                        _ => TarError::Io(error),
                    })?;
                let copied = io::copy(&mut (&mut input).take(size), &mut file)?;
                if copied != size {
                    return Err(TarError::Unsafe);
                }
                file.flush()?;
                file.sync_all()?;
                let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                io::copy(&mut (&mut input).take(padding), &mut io::sink())?;
            }
            b'5' => {
                if size != 0 {
                    return Err(TarError::Unsafe);
                }
                fs::create_dir_all(&destination)?;
            }
            // Links, devices, FIFOs, pax and GNU extension headers.
            _ => return Err(TarError::Unsafe),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a ustar archive of `(name, type, contents)` entries.
    pub(crate) fn archive(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, contents) in entries {
            let mut header = [0_u8; BLOCK];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..107].copy_from_slice(b"0000644");
            header[108..115].copy_from_slice(b"0000000");
            header[116..123].copy_from_slice(b"0000000");
            header[124..135].copy_from_slice(format!("{:011o}", contents.len()).as_bytes());
            header[136..147].copy_from_slice(b"00000000000");
            header[156] = *kind;
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(contents);
            out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        out.extend_from_slice(&[0; BLOCK * 2]);
        out
    }

    #[test]
    fn extracts_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        fs::write(
            &path,
            archive(&[
                ("model/", b'5', b""),
                ("model/config.json", b'0', b"{}"),
                ("model/w.bin", b'0', &[7_u8; 700]),
            ]),
        )
        .unwrap();
        let target = dir.path().join("out");
        assert_eq!(extract(&path, &target, 10_000).unwrap(), 702);
        assert_eq!(fs::read(target.join("model/config.json")).unwrap(), b"{}");
        assert_eq!(fs::read(target.join("model/w.bin")).unwrap().len(), 700);
    }

    #[test]
    fn refuses_links_traversal_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        for (index, entries) in [
            vec![("../escape", b'0', &b"x"[..])],
            vec![("/abs", b'0', &b"x"[..])],
            vec![("link", b'2', &b""[..])],
            vec![("big", b'0', &[1_u8; 64][..])],
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("{index}.tar"));
            fs::write(&path, archive(&entries)).unwrap();
            let target = dir.path().join(format!("out{index}"));
            assert!(
                matches!(extract(&path, &target, 32), Err(TarError::Unsafe)),
                "{index}"
            );
        }
    }
}
