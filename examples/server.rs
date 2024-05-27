
use std::{io::Read, path::Path, sync::Arc};

use bevy::prelude::*;
use bevy_quinn::*;

fn main() {
    let mut app = App::new();

    app.add_plugins(MinimalPlugins);
    app.add_plugins(bevy::log::LogPlugin {
        level: bevy::log::Level::INFO,
        ..default()
    });

    app.add_plugins(QuinnPlugin);

    app.add_systems(Startup, spawn_endpoint);

    app.run();
}

fn load_certs() -> rustls::ServerConfig {
    let chain = std::fs::File::open("fullchain.pem").expect("failed to open cert file");
    let mut chain: std::io::BufReader<std::fs::File> = std::io::BufReader::new(chain);

    let chain: Vec<rustls::pki_types::CertificateDer> = rustls_pemfile::certs(&mut chain)
        .collect::<Result<_, _>>()
        .expect("failed to load certs");

    trace!("Loading private key for server.");
    let mut keys = std::fs::File::open("privkey.pem").expect("failed to open key file");

    let mut buf = Vec::new();
    keys.read_to_end(&mut buf).unwrap();

    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(&buf))
        .expect("failed to load private key")
        .expect("missing private key");

    debug!("Loaded certificate files.");

    let  config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key).unwrap();

    config
}


fn spawn_endpoint(
    mut commands: Commands,
) {
    let mut config = load_certs();

    config.max_early_data_size = u32::MAX;
    config.alpn_protocols = vec![b"h3".to_vec()]; // this one is important

    let config: quinn_proto::crypto::rustls::QuicServerConfig = config.try_into().unwrap();

    let server_config = bevy_quinn::quinn_proto::ServerConfig::with_crypto(Arc::new(config));

    let endpoint = Endpoint::new(EndpointConfig {
        bind_addr: "0.0.0.0:27510".parse().unwrap(),
        server_config: Some(server_config),
        receive_budget: 10,
    }).unwrap();

    commands.spawn(endpoint);
}
