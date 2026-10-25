//! `allowInsecure` without `allowInsecure`.
//!
//! Xray refuses configs with `tlsSettings.allowInsecure` since 2026-06-01 and
//! points to `pinnedPeerCertSha256` instead. Share links and panels still set
//! the flag (self-signed certs, IP-only servers), so before xray starts we
//! fetch the certificate the server presents and pin it. A pinned leaf skips
//! CA and name checks in xray, which is what the flag used to do, and a
//! certificate swapped later by someone in the middle no longer passes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};

const TIMEOUT: Duration = Duration::from_secs(5);
/// Pings build a config per server; one handshake per server is enough for a while.
const CACHE_FOR: Duration = Duration::from_secs(600);

type Cache = HashMap<(IpAddr, u16, String), (Instant, String)>;

/// SHA-256 of the leaf certificate `ip:port` presents for `sni`, as hex, the
/// form xray's `pinnedPeerCertSha256` takes. Cached for a few minutes.
pub fn leaf_sha256(ip: IpAddr, port: u16, sni: &str) -> Result<String> {
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let key = (ip, port, sni.to_string());
    if let Some((at, hash)) = CACHE.lock().unwrap().get_or_insert_default().get(&key)
        && at.elapsed() < CACHE_FOR
    {
        return Ok(hash.clone());
    }
    let hash = fetch(ip, port, sni)?;
    CACHE.lock().unwrap().get_or_insert_default().insert(key, (Instant::now(), hash.clone()));
    Ok(hash)
}

fn fetch(ip: IpAddr, port: u16, sni: &str) -> Result<String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    let name = match sni.parse::<IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(sni.to_string()).with_context(|| format!("bad server name {sni}"))?,
    };
    let mut conn = ClientConnection::new(Arc::new(config), name)?;
    let mut sock = TcpStream::connect_timeout(&SocketAddr::new(ip, port), TIMEOUT)
        .with_context(|| format!("TLS {ip}:{port}"))?;
    sock.set_read_timeout(Some(TIMEOUT))?;
    sock.set_write_timeout(Some(TIMEOUT))?;
    let deadline = Instant::now() + TIMEOUT;
    while conn.is_handshaking() {
        if Instant::now() > deadline {
            bail!("TLS {ip}:{port}: handshake timed out");
        }
        conn.complete_io(&mut sock).with_context(|| format!("TLS {ip}:{port}"))?;
    }
    let leaf = conn.peer_certificates().and_then(|c| c.first()).context("server sent no certificate")?;
    let hash = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
    // Polite close; the server may already be gone.
    conn.send_close_notify();
    let _ = conn.write_tls(&mut sock);
    let _ = sock.flush();
    let _ = sock.read(&mut [0; 1]);
    Ok(hash.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// Accepts any certificate: we only want to see it, never to trust the channel.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
