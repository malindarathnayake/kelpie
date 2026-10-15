use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use sha2::{Digest, Sha256};

pub(crate) fn verify_sha256(path: &Path, expected: &str) -> io::Result<()> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected sha256 must be 64 hexadecimal characters",
        ));
    }

    let actual = file_sha256(path)?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sha256 mismatch: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

pub(crate) fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_lower_hex(&hasher.finalize()))
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn verifies_matching_sha256() {
        let path =
            std::env::temp_dir().join(format!("kelpie-checksum-test-{}", std::process::id()));
        fs::write(&path, b"kelpie").unwrap();
        let result = super::verify_sha256(
            &path,
            "33e5580db5325399d2cc9e13269cbc03205c8198e8824fedc469915ba083d001",
        );
        let _ = fs::remove_file(&path);
        assert!(result.is_ok());
    }
}
