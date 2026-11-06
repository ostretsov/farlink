//! Built-in HTTPS for the relay: certificates from Let's Encrypt (automatic,
//! TLS-ALPN-01 on port 443, nothing else needed) or from PEM files, plus an
//! optional plain-HTTP listener that only redirects to the HTTPS address.
//!
//! Handshakes run in their own tasks, so a slow or silent client never holds up
//! other connections; finished TLS streams are handed to axum through
//! [`TlsListener`].

use crate::config::{ServerConfig, Tls};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use rustls::ServerConfig as RustlsConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// A client that has not finished the TLS handshake by then is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Any finished connection (TLS over TCP, whichever library made it).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

/// Ready TLS connections, for `axum::serve`.
pub struct TlsListener {
    rx: mpsc::Receiver<(Box<dyn Io>, SocketAddr)>,
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = Box<dyn Io>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            // The accept task only ends when the runtime shuts down.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local)
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// HTTP/1.1 only: browsers and the programs open WebSockets with an HTTP/1.1
/// upgrade, and the server does not speak HTTP/2.
fn finish(mut cfg: RustlsConfig) -> Arc<RustlsConfig> {
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// Build a TLS setup from certificate + key PEM files (full chain first).
pub fn config_from_files(cert: &Path, key: &Path) -> Result<Arc<RustlsConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading certificate {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading certificate {}", cert.display()))?;
    if certs.is_empty() {
        bail!("no certificate found in {}", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading private key {}", key.display()))?;
    let cfg = RustlsConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and private key do not match")?;
    Ok(finish(cfg))
}

/// Accept TCP connections on `listener` and serve HTTPS according to `tls`.
/// `data_dir` keeps the Let's Encrypt account and certificates (`acme/`).
pub fn listen(listener: TcpListener, tls: &Tls, data_dir: &Path) -> Result<TlsListener> {
    let local = listener.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    match tls {
        Tls::Off => bail!("HTTPS is switched off in the settings"),
        Tls::Files { cert, key } => {
            let acceptor = tokio_rustls::TlsAcceptor::from(config_from_files(cert, key)?);
            accept_loop(listener, tx, move |tcp| {
                let acceptor = acceptor.clone();
                async move { Ok(Some(Box::new(acceptor.accept(tcp).await?) as Box<dyn Io>)) }
            });
        }
        Tls::Auto { domain, email, staging } => {
            let mut acme = rustls_acme::AcmeConfig::new([domain.as_str()])
                .cache(rustls_acme::caches::DirCache::new(data_dir.join("acme")))
                .directory_lets_encrypt(!*staging);
            if !email.trim().is_empty() {
                acme = acme.contact_push(format!("mailto:{}", email.trim()));
            }
            let mut state = acme.state();
            let config = finish(
                RustlsConfig::builder_with_provider(provider())
                    .with_safe_default_protocol_versions()?
                    .with_no_client_auth()
                    .with_cert_resolver(state.resolver()),
            );
            let challenge = state.challenge_rustls_config_with_provider(provider());
            // Drives ordering, caching and renewal for as long as the server runs.
            let domain = domain.clone();
            tokio::spawn(async move {
                while let Some(event) = state.next().await {
                    match event {
                        Ok(ok) => log::info!("HTTPS certificate for {domain}: {ok:?}"),
                        Err(err) => log::warn!(
                            "HTTPS certificate for {domain}: {err} (is the DNS record pointing here and port 443 open?)"
                        ),
                    }
                }
            });
            accept_loop(listener, tx, move |tcp| {
                let config = config.clone();
                let challenge = challenge.clone();
                async move {
                    let start = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), tcp).await?;
                    if rustls_acme::is_tls_alpn_challenge(&start.client_hello()) {
                        // Let's Encrypt checking that we own the domain: answer and close.
                        let _ = start.into_stream(challenge).await?;
                        return Ok(None);
                    }
                    Ok(Some(Box::new(start.into_stream(config).await?) as Box<dyn Io>))
                }
            });
        }
    }
    Ok(TlsListener { rx, local })
}

fn accept_loop<F, Fut>(listener: TcpListener, tx: mpsc::Sender<(Box<dyn Io>, SocketAddr)>, handshake: F)
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = std::io::Result<Option<Box<dyn Io>>>> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let (tcp, addr) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // Usually "too many open files": wait instead of spinning.
                    log::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let tx = tx.clone();
            let fut = handshake(tcp);
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await {
                    Ok(Ok(Some(io))) => {
                        let _ = tx.send((io, addr)).await;
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => log::debug!("TLS handshake with {addr}: {e}"),
                    Err(_) => log::debug!("TLS handshake with {addr}: timed out"),
                }
            });
        }
    });
}

/// Plain-HTTP listener that sends every request to the same path on `public_url`.
pub async fn serve_redirect(listener: TcpListener, public_url: String) -> Result<()> {
    use axum::http::{StatusCode, Uri, header};
    use axum::response::IntoResponse;
    let base = public_url.trim_end_matches('/').to_string();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let base = base.clone();
        async move {
            let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
            (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, format!("{base}{path}"))]).into_response()
        }
    });
    axum::serve(listener, app).await?;
    Ok(())
}

/// Bind everything the settings ask for and run the server until it fails.
pub async fn run(relay: crate::server::Shared, cfg: ServerConfig) -> Result<()> {
    let listener = TcpListener::bind(&cfg.bind).await.with_context(|| format!("listening on {}", cfg.bind))?;
    if let Some(addr) = cfg.redirect_bind.as_deref().filter(|a| !a.trim().is_empty()) {
        let redirect = TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?;
        let url = cfg.public_url.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_redirect(redirect, url).await {
                log::error!("HTTP redirect stopped: {e:#}");
            }
        });
    }
    match &cfg.tls {
        Tls::Off => crate::server::serve(relay, listener).await,
        tls => crate::server::serve(relay, listen(listener, tls, &cfg.data_dir)?).await,
    }
}
