
use std::path::Path;

use bevy::prelude::*;
use bevy_quinn::*;

fn main() {
    let mut app = App::new();

    app.add_plugins(MinimalPlugins);
    app.add_plugins(bevy::log::LogPlugin {
        level: bevy::log::Level::DEBUG,
        ..default()
    });

    app.add_plugins(QuinnPlugin);

    app.add_systems(Startup, spawn_endpoint);

    app.run();
}

fn spawn_endpoint(
    mut commands: Commands,
) {
    generate_self_signed_cert();

    let cert_chain = todo!();
    let key = todo!();

    let mut server_crypto = bevy_quinn::rustls::server::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .unwrap();

    let mut server_config = bevy_quinn::quinn_proto::ServerConfig::with_single_cert(cert_chain, key);

    let endpoint = Endpoint::new("0.0.0.0:27510".parse().unwrap(), ).unwrap();

    commands.spawn(endpoint);
}
