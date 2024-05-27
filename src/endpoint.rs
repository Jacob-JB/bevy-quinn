
use std::{io::IoSliceMut, net::{SocketAddr, UdpSocket}, sync::Arc, time::Instant};

use bevy::{prelude::*, utils::HashMap};
use quinn_udp::{RecvMeta, UdpSockRef, UdpSocketState};

use crate::{udp_transmit, Connection, ConnectionCallback, ConnectionId};


#[derive(Component)]
pub struct Endpoint {
    config: EndpointConfig,
    socket: UdpSocket,
    socket_state: quinn_udp::UdpSocketState,
    /// allocated memory for receiving udp datagrams
    receive_buffer: Box<[u8]>,
    endpoint: quinn_proto::Endpoint,
    connections: HashMap<quinn_proto::ConnectionHandle, Connection>,
}

pub struct EndpointConfig {
    pub bind_addr: SocketAddr,
    pub server_config: Option<quinn_proto::ServerConfig>,
    pub receive_budget: usize,
}

pub(crate) enum EndpointCallback {
    SuccessfulConnection(quinn_proto::ConnectionHandle),

    /// another endpoint opened a receive stream
    OpenedReceiveStream {
        connection_handle: quinn_proto::ConnectionHandle,
        stream_id: quinn_proto::StreamId,
    },
    /// another endpoint closed a receive stream
    ClosedReceiveStream {
        connection_handle: quinn_proto::ConnectionHandle,
        stream_id: quinn_proto::StreamId,
    },
    /// a receive stream's data was entirely read
    FinishedReceiveStream {
        connection_handle: quinn_proto::ConnectionHandle,
        stream_id: quinn_proto::StreamId,
    },

    /// another endpoint opened a send stream
    OpenedSendStream {
        connection_handle: quinn_proto::ConnectionHandle,
        stream_id: quinn_proto::StreamId,
    },
    /// another endpoint closed a send stream
    ClosedSendStream {
        connection_handle: quinn_proto::ConnectionHandle,
        stream_id: quinn_proto::StreamId,
        error_code: quinn_proto::VarInt,
    },
}

impl Endpoint {
    pub fn new(config: EndpointConfig) -> std::io::Result<Self> {

        let endpoint_config = quinn_proto::EndpointConfig::default();

        let socket = UdpSocket::bind(config.bind_addr)?;
        let socket_state = UdpSocketState::new(UdpSockRef::from(&socket))?;

        let receive_buffer = vec![
            0;
            endpoint_config.get_max_udp_payload_size().min(64 * 1024) as usize
                * socket_state.max_gso_segments()
                * quinn_udp::BATCH_SIZE
        ].into_boxed_slice();

        let endpoint = quinn_proto::Endpoint::new(
            Arc::new(endpoint_config),
            // the default server config to use for connections
            config.server_config.clone().map(Arc::new),
            !socket_state.may_fragment(),
            None
        );

        Ok(Endpoint {
            config,
            socket,
            socket_state,
            receive_buffer,
            endpoint,
            connections: HashMap::new(),
        })
    }

    pub(crate) fn udpate(&mut self, now: Instant, mut callback: impl FnMut(EndpointCallback)) {
        // construct an array of size `quinn_udp::BATCH_SIZE` from the allocated receive buffer
        let mut buffers = self.receive_buffer.chunks_mut(self.receive_buffer.len() / quinn_udp::BATCH_SIZE).map(IoSliceMut::new);
        let mut buffers: [IoSliceMut; quinn_udp::BATCH_SIZE] = std::array::from_fn(|_| buffers.next().unwrap());

        let mut metas = [RecvMeta::default(); quinn_udp::BATCH_SIZE];

        // receive as many datagrams as possible
        loop {
            match self.socket_state.recv(UdpSockRef::from(&self.socket), &mut buffers, &mut metas) {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,

                Err(err) => panic!("{:?}", err),

                Ok(message_count) => {
                    // process the number of received messages
                    for (meta, buffer) in metas.iter().zip(buffers.iter()).take(message_count) {
                        let mut remaining_data = &buffer[0..meta.len];

                        // `data` could contain many datagrams due to gro, process each individually
                        while !remaining_data.is_empty() {
                            let stride_length = meta.stride.min(remaining_data.len());
                            let data = &remaining_data[0..stride_length];
                            remaining_data = &remaining_data[stride_length..];

                            let ecn = meta.ecn.map(quinn_proto_ecn);

                            let mut response_buffer = Vec::new();
                            let Some(datagram_event) = self.endpoint.handle(
                                now,
                                meta.addr,
                                meta.dst_ip,
                                ecn,
                                data.into(),
                                &mut response_buffer,
                            ) else {
                                continue;
                            };

                            match datagram_event {
                                quinn_proto::DatagramEvent::NewConnection(incoming) => {
                                    // if the endpoint is not configured as a server don't accept the connection
                                    if self.config.server_config.is_none() {
                                        continue;
                                    }

                                    // new incoming connection
                                    let mut response_buffer = Vec::new();

                                    // will panic if no server config was provided to the endpoint
                                    match self.endpoint.accept(
                                        incoming,
                                        now,
                                        &mut response_buffer,
                                        None,
                                    ) {
                                        Err(err) => {
                                            if let Some(transmit) = err.response {
                                                respond(&mut self.socket_state, &self.socket, &transmit, &response_buffer);
                                            }
                                            warn!("failed to accept incoming connection {:?}", err.cause);
                                        },
                                        Ok((handle, connection)) => {
                                            let connection = Connection::new(handle, connection, &self.config);
                                            if let Some(_) = self.connections.insert(handle, connection) {
                                                panic!("got the same connection handle twice");
                                            }
                                        }
                                    }
                                },
                                quinn_proto::DatagramEvent::ConnectionEvent(handle, event) => {
                                    self.connections.get_mut(&handle).expect("connection should exist").connection.handle_event(event);
                                },
                                quinn_proto::DatagramEvent::Response(transmit) => {
                                    // transmit response
                                    respond(&mut self.socket_state, &self.socket, &transmit, &response_buffer);
                                }
                            }
                        }
                    }
                }
            }
        }

        self.connections.retain(|&connection_handle, connection| {
            let mut close_stream = false;

            connection.update(
                now,
                &mut self.endpoint,
                &self.socket,
                &mut self.socket_state,
                |client_callback| match client_callback {
                    ConnectionCallback::SuccessfulConnection(connection_handle) => {
                        callback(EndpointCallback::SuccessfulConnection(connection_handle));
                    },
                    ConnectionCallback::OpenedReceiveStream(stream_id) => callback(EndpointCallback::OpenedReceiveStream { connection_handle, stream_id }),
                    ConnectionCallback::ClosedReceiveStream(stream_id) => callback(EndpointCallback::ClosedReceiveStream { connection_handle, stream_id }),
                    ConnectionCallback::FinishedReceiveStream(stream_id) => callback(EndpointCallback::FinishedReceiveStream { connection_handle, stream_id }),
                    ConnectionCallback::OpenedSendStream(stream_id) => callback(EndpointCallback::OpenedSendStream { connection_handle, stream_id }),
                    ConnectionCallback::ClosedSendStream(stream_id, error_code) => callback(EndpointCallback::ClosedSendStream { connection_handle, stream_id, error_code }),
                    ConnectionCallback::CloseConnection => {
                        close_stream = true;
                    }
                },
            );

            !close_stream
        });
    }

    pub fn connect(&mut self, client_config: quinn_proto::ClientConfig, addr: SocketAddr, server_name: &str) -> Result<(), quinn_proto::ConnectError> {
        let (handle, connection) = self.endpoint.connect(Instant::now(), client_config, addr, server_name)?;

        let connection = Connection::new(handle, connection, &self.config);
        if let Some(_) = self.connections.insert(handle, connection) {
            panic!("got the same connection handle twice ({:?})", handle);
        }

        Ok(())
    }

    pub fn connection(&self, connection: ConnectionId) -> Option<&Connection> {
        self.connections.get(&connection.0)
    }

    pub fn connection_mut(&mut self, connection: ConnectionId) -> Option<&mut Connection> {
        self.connections.get_mut(&connection.0)
    }
}



/// converts from [quinn_udp::EcnCodepoint] to [quinn_proto::EcnCodepoint]
fn quinn_proto_ecn(ecn: quinn_udp::EcnCodepoint) -> quinn_proto::EcnCodepoint {
    match ecn {
        quinn_udp::EcnCodepoint::Ect0 => quinn_proto::EcnCodepoint::Ect0,
        quinn_udp::EcnCodepoint::Ect1 => quinn_proto::EcnCodepoint::Ect1,
        quinn_udp::EcnCodepoint::Ce => quinn_proto::EcnCodepoint::Ce,
    }
}

fn respond(socket_state: &mut quinn_udp::UdpSocketState, socket: &UdpSocket, transmit: &quinn_proto::Transmit, response_buffer: &[u8]) {
    // convert to `quinn_proto` transmit
    let transmit = udp_transmit(transmit, response_buffer);

    // send if there is kernal buffer space, else drop it
    _ = socket_state.send(UdpSockRef::from(&socket), &transmit);
}
