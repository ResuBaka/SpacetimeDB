//! Opt-in, bounded diagnostic capture of decompressed server messages.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"STDBCAP1";
const RECORD_HEADER: u64 = 25;

pub(crate) struct PayloadCapture {
    file: File,
    limit: u64,
    written: u64,
}

impl PayloadCapture {
    pub(crate) fn create(path: &Path, limit: u64) -> io::Result<Self> {
        if limit < MAGIC.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture limit is smaller than the file header",
            ));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(MAGIC)?;
        Ok(Self {
            file,
            limit,
            written: MAGIC.len() as u64,
        })
    }

    /// Returns false if the next complete record does not fit.
    pub(crate) fn record_message(&mut self, wire: &[u8]) -> io::Result<bool> {
        let decoded =
            crate::compression::decompress_server_message(wire).map_err(|error| io::Error::other(error.to_string()))?;
        let size = u64::try_from(decoded.len()).map_err(|_| io::Error::other("payload too large"))?;
        let Some(end) = self
            .written
            .checked_add(RECORD_HEADER)
            .and_then(|n| n.checked_add(size))
        else {
            return Ok(false);
        };
        if end > self.limit {
            return Ok(false);
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        self.file.write_all(&timestamp.to_le_bytes())?;
        self.file.write_all(&(wire.len() as u64).to_le_bytes())?;
        self.file.write_all(&size.to_le_bytes())?;
        self.file.write_all(&wire[..1])?;
        self.file.write_all(&decoded)?;
        self.written = end;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn path() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "stdb-capture-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn captures_compressed_and_uncompressed_and_stops_at_limit() {
        let path = path();
        let payload = b"example BSATN bytes";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(payload).unwrap();
        let mut compressed = vec![2];
        compressed.extend(gzip.finish().unwrap());
        let mut plain = vec![0];
        plain.extend(payload);
        let record_size = RECORD_HEADER + payload.len() as u64;
        let mut capture = PayloadCapture::create(&path, MAGIC.len() as u64 + 2 * record_size).unwrap();
        assert!(PayloadCapture::create(&path, 100).is_err());
        assert!(capture.record_message(&compressed).unwrap());
        assert!(capture.record_message(&plain).unwrap());
        assert!(!capture.record_message(&plain).unwrap());
        drop(capture);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        let mut pos = 8;
        for wire in [&compressed, &plain] {
            let wire_len = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap());
            let len = u64::from_le_bytes(bytes[pos + 16..pos + 24].try_into().unwrap()) as usize;
            assert_eq!(wire_len, wire.len() as u64);
            assert_eq!(len, payload.len());
            assert_eq!(bytes[pos + 24], wire[0]);
            assert_eq!(&bytes[pos + 25..pos + 25 + len], payload);
            pos += 25 + len;
        }
        assert_eq!(pos, bytes.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_tiny_limit_and_invalid_message() {
        let path = path();
        assert!(PayloadCapture::create(&path, 7).is_err());
        assert!(!path.exists());
        let mut capture = PayloadCapture::create(&path, 1024).unwrap();
        assert!(capture.record_message(&[99]).is_err());
        drop(capture);
        assert_eq!(std::fs::read(&path).unwrap(), MAGIC);
        std::fs::remove_file(path).unwrap();
    }
}
