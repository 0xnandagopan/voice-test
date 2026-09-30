//! Runtime behavior shared by the independently supervised API and worker.
use std::net::SocketAddr;

/// The S3 and voice clients enable different rustls crypto backends. Choose one
/// before SQLx or WebSocket TLS builds a config instead of relying on inference.
pub fn initialize_tls() {
    v0_voice::client::initialize_tls();
}

pub fn bind_address(bind: Option<&str>, port: Option<&str>) -> Result<SocketAddr, &'static str> {
    if let Some(bind) = bind {
        return bind
            .parse()
            .map_err(|_| "BIND_ADDR must be an IP address and port");
    }
    if let Some(port) = port {
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("PORT must be a number from 1 to 65535")?;
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    Ok(SocketAddr::from(([127, 0, 0, 1], 3000)))
}

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = terminate.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_builder_works_with_both_storage_and_voice_crypto_features_enabled() {
        initialize_tls();
        let _ = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        initialize_tls(); // Safe on a repeated call; the installed provider stays stable.
    }

    #[test]
    fn hosted_port_binds_all_interfaces_without_changing_local_default() {
        assert_eq!(
            bind_address(None, None).unwrap().to_string(),
            "127.0.0.1:3000"
        );
        assert_eq!(
            bind_address(None, Some("8080")).unwrap().to_string(),
            "0.0.0.0:8080"
        );
        assert_eq!(
            bind_address(Some("127.0.0.1:4000"), Some("8080"))
                .unwrap()
                .to_string(),
            "127.0.0.1:4000"
        );
        for bad in ["", "0", "65536", "invalid"] {
            assert!(bind_address(None, Some(bad)).is_err());
        }
    }
}
