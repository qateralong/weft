use std::path::PathBuf;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use weft_session::tls::{TlsAcceptor, provider};

use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("cannot read {path}: {source}")]
    Pem { path: PathBuf, source: rustls::pki_types::pem::Error },
    #[error("set both tls_cert and tls_key, or neither")]
    Incomplete,
    #[error(transparent)]
    Rustls(#[from] rustls::Error),
    #[error(transparent)]
    Generate(#[from] rcgen::Error),
}

/// Uses the configured certificate, or a fresh self-signed one: clients authenticate the
/// server through Noise, TLS only makes the stream look like HTTPS.
pub fn acceptor(config: &Config) -> Result<TlsAcceptor, TlsError> {
    let (certs, key) = match (&config.tls_cert, &config.tls_key) {
        (Some(cert), Some(key)) => {
            let certs = CertificateDer::pem_file_iter(cert)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .map_err(|source| TlsError::Pem { path: cert.clone(), source })?;
            let key =
                PrivateKeyDer::from_pem_file(key).map_err(|source| TlsError::Pem { path: key.clone(), source })?;
            (certs, key)
        }
        (None, None) => {
            let name = config.public_host.clone().unwrap_or_else(|| "localhost".to_string());
            let key_pair = rcgen::KeyPair::generate()?;
            let mut params = rcgen::CertificateParams::new(vec![name.clone()])?;
            params.distinguished_name = rcgen::DistinguishedName::new();
            params.distinguished_name.push(rcgen::DnType::CommonName, name);
            let cert = params.self_signed(&key_pair)?;
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
            (vec![cert.der().clone()], key)
        }
        _ => return Err(TlsError::Incomplete),
    };
    let mut server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(server)))
}

const HTTP_METHODS: [&[u8; 4]; 9] = [b"GET ", b"POST", b"HEAD", b"PUT ", b"OPTI", b"DELE", b"PATC", b"CONN", b"PRI "];

pub fn is_http(start: &[u8]) -> bool {
    HTTP_METHODS.iter().any(|method| start == method.as_slice())
}

/// A reply in the style of nginx for scanners that speak HTTP.
pub fn http_reply(status: &str, message: &str) -> Vec<u8> {
    let body = format!(
        "<html>\r\n<head><title>{status}</title></head>\r\n<body>\r\n<center><h1>{status}</h1></center>\r\n{message}<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n"
    );
    format!(
        "HTTP/1.1 {status}\r\nServer: nginx\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http() {
        assert!(is_http(b"GET "));
        assert!(is_http(b"PRI "));
        assert!(!is_http(b"GET"));
        assert!(!is_http(&[0x16, 3, 1, 0]));
        let reply = String::from_utf8(http_reply("404 Not Found", "")).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
    }

    #[test]
    fn self_signed_and_incomplete_configs() {
        assert!(acceptor(&Config::default()).is_ok());
        let config = Config { tls_cert: Some("cert.pem".into()), ..Config::default() };
        assert!(matches!(acceptor(&config), Err(TlsError::Incomplete)));
    }
}
