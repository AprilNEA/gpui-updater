//! Small helpers shared by the modules.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Lowercase hex SHA-256 of a file, read in chunks.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(Error::io("open", path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(Error::io("read", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The final path component as UTF-8.
pub(crate) fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::invalid(format!("`{}` has no UTF-8 file name", path.display())))
}

/// Run `command`, returning its standard output. A failure to start or a
/// non-zero exit becomes [`Error::Command`] with the end of standard error.
pub(crate) fn run(command: &mut Command) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|e| Error::Command {
        program: program.clone(),
        detail: format!("could not start: {e}"),
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tail: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .rev()
        .take(12)
        .collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(Error::Command {
        program,
        detail: format!("{}\n{}", output.status, tail.join("\n")),
    })
}

/// Join a base URL and a file name with exactly one `/`.
pub(crate) fn join_url(base: &str, name: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        name.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn hashes_and_joins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(join_url("https://e/x/", "/f"), "https://e/x/f");
    }

    #[test]
    fn command_failure_reports_stderr() {
        let err = run(Command::new("sh").args(["-c", "echo boom >&2; exit 3"])).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }
}
