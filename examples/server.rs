
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


    let a: QuicServerConfig;

    let mut server_config = bevy_quinn::quinn_proto::ServerConfig::with_crypto(server_crypto);

    let endpoint = Endpoint::new("0.0.0.0:27510".parse().unwrap(), Some(server_config)).unwrap();

    commands.spawn(endpoint);
}
