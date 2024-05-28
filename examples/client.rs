
use std::sync::Arc;

use bevy::prelude::*;
use bevy_quinn::*;
use quinn_proto::crypto::rustls::QuicClientConfig;

fn main() {
    let mut app = App::new();

    app.add_plugins(MinimalPlugins);
    app.add_plugins(bevy::log::LogPlugin {
        level: bevy::log::Level::DEBUG,
        ..default()
    });

    app.add_plugins(QuinnPlugin::default());

    app.add_systems(Startup, spawn_endpoint);
    app.add_systems(Update, (
        (
            spawn_streams,
            remove_streams,
        ),
        apply_deferred,
        read_data,
    ).chain());

    app.run();
}

#[derive(Component)]
struct Stream {
    connection_id: ConnectionId,
    stream_id: StreamId,
    data: Vec<u8>,
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

fn spawn_streams(
    mut commands: Commands,
    mut stream_r: EventReader<OpenedReceiveStream>,
) {
    for &OpenedReceiveStream { endpoint_entity, connection_id, stream_id } in stream_r.read() {
        commands.spawn(Stream {
            connection_id,
            stream_id,
            data: Vec::new(),
        }).set_parent(endpoint_entity);
    }
}

fn read_data(
    mut stream_q: Query<(&mut Stream, &Parent)>,
    mut endpoint_q: Query<&mut Endpoint>,
) {
    for (mut stream, stream_parent) in stream_q.iter_mut() {
        let mut endpoint = endpoint_q.get_mut(stream_parent.get()).unwrap();
        let connection = endpoint.connection_mut(stream.connection_id).unwrap();
        let stream_buffer = connection.get_receive_stream_mut(stream.stream_id).unwrap();

        stream.data.extend(stream_buffer.read().as_ref());
    }
}

fn remove_streams(
    mut commands: Commands,
    mut stream_r: EventReader<FinishedReceiveStream>,
    stream_q: Query<(Entity, &Stream, &Parent)>,
) {
    for &FinishedReceiveStream { endpoint_entity, connection_id, stream_id } in stream_r.read() {
        for (stream_entity, stream, stream_parent) in stream_q.iter() {
            if {
                stream_parent.get() == endpoint_entity &&
                stream.connection_id == connection_id &&
                stream.stream_id == stream_id
            } {
                let message = std::str::from_utf8(stream.data.as_slice()).unwrap();
                info!("stream finished \"{}\"", message);

                commands.entity(stream_entity).despawn_recursive();
                break;
            }
        }
    }
}
