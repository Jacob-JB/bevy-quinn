
use std::time::Instant;

use bevy::prelude::*;
pub use quinn_proto;


mod endpoint;
mod connection;

pub use endpoint::*;
pub use connection::*;


pub struct QuinnPlugin;
impl Plugin for QuinnPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<Connected>();
        app.add_event::<OpenedReceiveStream>();
        app.add_event::<ClosedReceiveStream>();
        app.add_event::<FinishedReceiveStream>();
        app.add_event::<OpenedSendStream>();
        app.add_event::<ClosedSendStream>();

        app.add_systems(PreUpdate, update_endpoints);
    }
}


/// fired when a new connection has been successfully opened
#[derive(Event)]
pub struct Connected {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
}

/// a receive stream was opened by another endpoint
///
/// this will not be fired for streams opened by this endpoint
#[derive(Event)]
pub struct OpenedReceiveStream {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
    pub stream: StreamId,
}

/// a receive stream was closed by another endpoint
///
/// this does not mean that the data has been entirely read,
/// just that no more data will be received.
///
/// [FinishedReceiveStream] will be fired once all the data has been read
#[derive(Event)]
pub struct ClosedReceiveStream {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
    pub stream: StreamId,
}

/// a receive stream's data has been entirely read
#[derive(Event)]
pub struct FinishedReceiveStream {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
    pub stream: StreamId,
}


/// a new send stream has been opened by another endpoint
///
/// this will not be fired for streams opened by this endpoint
#[derive(Event)]
pub struct OpenedSendStream {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
    pub stream: StreamId,
}

/// a ssend tream was closed by another endpoint
///
/// this will not be fired for streams closed by this endpoint
#[derive(Event)]
pub struct ClosedSendStream {
    pub endpoint_entity: Entity,
    pub connection: ConnectionId,
    pub stream: StreamId,
    pub error_code: quinn_proto::VarInt,
}



fn update_endpoints(
    mut endpoint_q: Query<(Entity, &mut Endpoint)>,
    mut connected_w: EventWriter<Connected>,
    mut opened_receive_stream_w: EventWriter<OpenedReceiveStream>,
    mut closed_receive_stream_w: EventWriter<ClosedReceiveStream>,
    mut finished_receive_stream_w: EventWriter<FinishedReceiveStream>,
    mut opened_send_stream_w: EventWriter<OpenedSendStream>,
    mut closed_send_stream_w: EventWriter<ClosedSendStream>,
) {
    let now = Instant::now();

    for (endpoint_entity, mut endpoint) in endpoint_q.iter_mut() {
        endpoint.udpate(now, |callback| match callback {
            EndpointCallback::SuccessfulConnection(connection_handle) => {
                connected_w.send(Connected {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                });
            },
            EndpointCallback::OpenedReceiveStream { connection_handle, stream_id } => {
                opened_receive_stream_w.send(OpenedReceiveStream {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                    stream: StreamId(stream_id),
                });
            },
            EndpointCallback::ClosedReceiveStream { connection_handle, stream_id } => {
                closed_receive_stream_w.send(ClosedReceiveStream {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                    stream: StreamId(stream_id),
                });
            },
            EndpointCallback::FinishedReceiveStream { connection_handle, stream_id } => {
                finished_receive_stream_w.send(FinishedReceiveStream {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                    stream: StreamId(stream_id),
                });
            },
            EndpointCallback::OpenedSendStream { connection_handle, stream_id } => {
                opened_send_stream_w.send(OpenedSendStream {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                    stream: StreamId(stream_id),
                });
            },
            EndpointCallback::ClosedSendStream { connection_handle, stream_id, error_code } => {
                closed_send_stream_w.send(ClosedSendStream {
                    endpoint_entity,
                    connection: ConnectionId(connection_handle),
                    stream: StreamId(stream_id),
                    error_code,
                });
            },
        });
    }
}


/// converts from [quinn_proto::EcnCodepoint] to [quinn_udp::EcnCodepoint]
fn quinn_udp_ecn(ecn: quinn_proto::EcnCodepoint) -> quinn_udp::EcnCodepoint {
    match ecn {
        quinn_proto::EcnCodepoint::Ect0 => quinn_udp::EcnCodepoint::Ect0,
        quinn_proto::EcnCodepoint::Ect1 => quinn_udp::EcnCodepoint::Ect1,
        quinn_proto::EcnCodepoint::Ce => quinn_udp::EcnCodepoint::Ce,
    }
}

pub(crate) fn udp_transmit<'a>(transmit: &'a quinn_proto::Transmit, buffer: &'a [u8]) -> quinn_udp::Transmit<'a> {
    quinn_udp::Transmit {
        destination: transmit.destination,
        ecn: transmit.ecn.map(quinn_udp_ecn),
        contents: &buffer[0..transmit.size],
        segment_size: transmit.segment_size,
        src_ip: transmit.src_ip,
    }
}