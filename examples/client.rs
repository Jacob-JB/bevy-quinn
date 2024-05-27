
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
    let mut endpoint = Endpoint::new("0.0.0.0:0".parse().unwrap(), None).unwrap();

    let chain = File::open("keys/cert.pem").expect("failed to open cert file");
    let mut chain = std::io::BufReader::new(chain);

    let chain: Vec<rustls::pki_types::CertificateDer> = rustls_pemfile::certs(&mut chain)
        .collect::<Result<_, _>>()
        .unwrap();

    let mut root_cert_store = rustls::RootCertStore::empty();
    for cert in chain {
        root_cert_store.add(&cert)?;
    }

    let client_config = quinn_proto::ClientConfig::with_root_certificates(todo!()).unwrap();

    endpoint.connect(client_config, "127.0.0.1:27510".parse().unwrap(), "localhost").unwrap();

    commands.spawn(endpoint);
}

