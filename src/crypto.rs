use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};
use thiserror::Error;
pub use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

/// Fingerprint of the leaf certificate.
///
/// A certificate chain without any certificate cannot be fingerprinted; this
/// returns an empty string instead of panicking (the process is built with
/// `panic = "abort"`) so callers can refuse the certificate.
pub fn certificate_fingerprint(cert: &Certificate) -> String {
    match cert.certificate.first() {
        Some(certificate) => generate_fingerprint(certificate),
        None => {
            log::warn!("certificate has no leaf certificate, refusing it");
            String::new()
        }
    }
}

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if !path.exists() || !path.is_file() {
        return generate_key_and_cert(path);
    }
    match load_certificate(path) {
        Ok(cert) => Ok(cert),
        Err(e) => {
            /* A truncated or corrupted key file (an interrupted write, a full
             * disk, a mangled manual edit) cannot be recovered from: fail every
             * start forever, or move it aside and start over. Keep the broken
             * file so the user can still restore it, and warn loudly because
             * the public key - and therefore the peers' authorization - changes.
             */
            let backup = path.with_extension("broken");
            log::error!(
                "could not load the certificate from {path:?}: {e}; moving it to {backup:?} and generating a new one - peers have to authorize the new public key"
            );
            fs::rename(path, &backup)?;
            generate_key_and_cert(path)
        }
    }
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    let f = File::create(path)?;
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400); /* r-- --- --- */
        f.set_permissions(perm)?;
    }
    /* FIXME windows permissions */
    /* The PEM is smaller than the writer's buffer, so `write_all` cannot fail
     * and a `Drop`-flushed writer would swallow every I/O error (ENOSPC, ...).
     * Flush explicitly so a half written key is reported instead of silently
     * producing a certificate file that fails to load on the next start.
     */
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    writer.flush()?;
    Ok(cert)
}
