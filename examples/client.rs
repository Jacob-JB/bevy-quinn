
use std::sync::Arc;

use bevy::prelude::*;
use bevy_quinn::*;
use quinn_proto::crypto::rustls::QuicClientConfig;

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

fn spawn_endpoint(
    mut commands: Commands,
) {

    let mut endpoint = Endpoint::new(EndpointConfig {
        bind_addr: "0.0.0.0:0".parse().unwrap(),
        server_config: None,
        receive_budget: 10_000,
    }).unwrap();

    let mut cfg = rustls_platform_verifier::tls_config_with_provider(Arc::new(rustls::crypto::ring::default_provider())).unwrap();
    cfg.alpn_protocols = vec![b"h3".to_vec()];

    let quic_cfg: QuicClientConfig = cfg.try_into().unwrap();
    let cfg = quinn_proto::ClientConfig::new(Arc::new(quic_cfg));


    endpoint.connect(cfg, "127.0.0.1:27510".parse().unwrap(), "dev.drewridley.com").unwrap();

    commands.spawn(endpoint);
}
