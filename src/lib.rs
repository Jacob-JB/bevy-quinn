
use std::{io::IoSliceMut, net::{SocketAddr, UdpSocket}, sync::Arc, time::Instant};

use bevy::{prelude::*, utils::{HashMap, HashSet}};
use quinn_udp::{RecvMeta, UdpSockRef, UdpSocketState};

pub use quinn_proto;


pub struct QuinnPlugin;
impl Plugin for QuinnPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, update_endpoints);
    }
}


#[derive(Component)]
pub struct Endpoint {
    socket: UdpSocket,
    socket_state: UdpSocketState,
    /// allocated memory for receiving udp datagrams
    receive_buffer: Box<[u8]>,
    endpoint: quinn_proto::Endpoint,
    /// the server configuration for the endpoint
    ///
    /// `None` if the endpoint is not configured to accept connections
    server_config: Option<Arc<quinn_proto::ServerConfig>>,
    connections: HashMap<quinn_proto::ConnectionHandle, ConnectionState>,
}

impl Endpoint {
    pub fn new(addr: SocketAddr, server_config: Option<quinn_proto::ServerConfig>) -> std::io::Result<Self> {

        let endpoint_config = quinn_proto::EndpointConfig::default();

        let socket = UdpSocket::bind(addr)?;
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
            server_config.map(Arc::new),
            !socket_state.may_fragment(),
            None
        );

        Ok(Endpoint {
            socket,
            socket_state,
            receive_buffer,
            endpoint,
            server_config: None,
            connections: HashMap::new(),
        })
    }

    fn udpate(&mut self, now: Instant) {
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
                        let mut data: bytes::BytesMut = buffer[0..meta.len].into();

                        // `data` could contain many datagrams due to gro, process each individually
                        while !data.is_empty() {
                            let data = data.split_off(meta.stride.min(data.len()));

                            let ecn = meta.ecn.map(quinn_proto_ecn);

                            let mut response_buffer = Vec::new();

                            debug!("processing datagram");

                            let Some(datagram_event) = self.endpoint.handle(
                                now,
                                meta.addr,
                                meta.dst_ip,
                                ecn,
                                data,
                                &mut response_buffer,
                            ) else {
                                continue;
                            };

                            match datagram_event {
                                quinn_proto::DatagramEvent::NewConnection(incoming) => {
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
                                            let connection = ConnectionState::new(handle, connection);
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

            for connection in self.connections.values_mut() {
                connection.update(
                    now,
                    &mut self.endpoint,
                    &self.socket,
                    &mut self.socket_state,
                );
            }
        }
    }

    pub fn connect(&mut self, client_config: quinn_proto::ClientConfig, addr: SocketAddr, server_name: &str) -> Result<(), quinn_proto::ConnectError> {
        let (handle, connection) = self.endpoint.connect(Instant::now(), client_config, addr, server_name)?;

        let connection = ConnectionState::new(handle, connection);
        if let Some(_) = self.connections.insert(handle, connection) {
            panic!("got the same connection handle twice");
        }

        Ok(())
    }
}


struct ConnectionState {
    connection: quinn_proto::Connection,
    handle: quinn_proto::ConnectionHandle,
    connected: bool,
    send_buffer: Vec<u8>,
    /// a set of streams that have data ready to be read
    readable_streams: HashSet<quinn_proto::StreamId>,
    send_streams: HashSet<quinn_proto::StreamId>,
}

impl ConnectionState {
    fn new(handle: quinn_proto::ConnectionHandle, connection: quinn_proto::Connection) -> Self {
        ConnectionState {
            connection,
            handle,
            connected: false,
            send_buffer: Vec::new(),
            readable_streams: HashSet::new(),
            send_streams: HashSet::new(),
        }
    }

    fn update(&mut self, now: Instant, endpoint: &mut quinn_proto::Endpoint, socket: &UdpSocket, socket_state: &mut quinn_udp::UdpSocketState) {
        // perform polls as described on `quinn_proto::Connection`

        // poll transmits
        let max_datagrams = socket_state.max_gso_segments();

        loop {
            self.send_buffer.clear();
            self.send_buffer.reserve(self.connection.current_mtu() as usize);

            let Some(transmit) = self.connection.poll_transmit(now, max_datagrams, &mut self.send_buffer) else {
                break;
            };

            if let Err(err) = socket_state.send(UdpSockRef::from(&socket), &udp_transmit(&transmit, &self.send_buffer)) {
                panic!("{:?}", err);
            }
        }


        if let Some(deadline) = self.connection.poll_timeout() {
            if deadline >= now {
                self.connection.handle_timeout(now);
            }
        }


        while let Some(endpoint_event) = self.connection.poll_endpoint_events() {
            // this is the last event the connection will emit, the connection state can be freed
            let end_connection = endpoint_event.is_drained();

            if let Some(connection_event) = endpoint.handle_event(self.handle, endpoint_event) {
                self.connection.handle_event(connection_event);
            }

            if end_connection {
                // drop connection state
            }
        }


        while let Some(event) = self.connection.poll() {
            match event {
                quinn_proto::Event::Connected => {
                    self.connected = true;
                },
                quinn_proto::Event::ConnectionLost { reason } => {

                },
                quinn_proto::Event::HandshakeDataReady => {

                },
                quinn_proto::Event::Stream(stream_event) => match stream_event {
                    quinn_proto::StreamEvent::Opened { dir } => {},
                    quinn_proto::StreamEvent::Readable { id } => {
                        self.readable_streams.insert(id);
                    },
                    quinn_proto::StreamEvent::Writable { id } => {

                    },
                    quinn_proto::StreamEvent::Finished { id } => {

                    },
                    quinn_proto::StreamEvent::Stopped { id, error_code } => {

                    },
                    quinn_proto::StreamEvent::Available { dir } => {},
                },
                quinn_proto::Event::DatagramReceived => {

                },
                quinn_proto::Event::DatagramsUnblocked => {

                },

            }
        }


        // accept new streams
        while let Some(stream_id) = self.connection.streams().accept(quinn_proto::Dir::Uni) {
            // new uni directional stream
            self.readable_streams.insert(stream_id);
        }

        while let Some(stream_id) = self.connection.streams().accept(quinn_proto::Dir::Bi) {
            // new bi directional stream
            self.readable_streams.insert(stream_id);
            self.send_streams.insert(stream_id);
        }


        // read streams
        for stream_id in self.readable_streams.drain() {
            let mut stream = self.connection.recv_stream(stream_id);

            let mut chunks = match stream.read(true) {
                Err(quinn_proto::ReadableError::ClosedStream) => continue,
                Err(quinn_proto::ReadableError::IllegalOrderedRead) => unreachable!("should never read unordered"),
                Ok(chunks) => chunks,
            };

            loop {
                match chunks.next(usize::MAX) {
                    Ok(None) | Err(quinn_proto::ReadError::Blocked) => {
                        // no more data is ready to be read
                        break;
                    },
                    Err(quinn_proto::ReadError::Reset(reason)) => {
                        // stream was reset
                    },
                    Ok(Some(chunk)) => {
                        let data = chunk.bytes.as_ref();
                    }
                }
            }

            // no need to promt, will be done anyway
            let _ = chunks.finalize();
        }
    }
}



fn update_endpoints(
    mut endpoint_q: Query<&mut Endpoint>,
) {
    let now = Instant::now();

    for mut endpoint in endpoint_q.iter_mut() {
        endpoint.udpate(now);
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

/// converts from [quinn_udp::EcnCodepoint] to [quinn_proto::EcnCodepoint]
fn quinn_proto_ecn(ecn: quinn_udp::EcnCodepoint) -> quinn_proto::EcnCodepoint {
    match ecn {
        quinn_udp::EcnCodepoint::Ect0 => quinn_proto::EcnCodepoint::Ect0,
        quinn_udp::EcnCodepoint::Ect1 => quinn_proto::EcnCodepoint::Ect1,
        quinn_udp::EcnCodepoint::Ce => quinn_proto::EcnCodepoint::Ce,
    }
}

fn udp_transmit<'a>(transmit: &'a quinn_proto::Transmit, buffer: &'a [u8]) -> quinn_udp::Transmit<'a> {
    quinn_udp::Transmit {
        destination: transmit.destination,
        ecn: transmit.ecn.map(quinn_udp_ecn),
        contents: &buffer[0..transmit.size],
        segment_size: transmit.segment_size,
        src_ip: transmit.src_ip,
    }
}

fn respond(socket_state: &mut quinn_udp::UdpSocketState, socket: &UdpSocket, transmit: &quinn_proto::Transmit, response_buffer: &[u8]) {
    // convert to `quinn_proto` transmit
    let transmit = udp_transmit(transmit, response_buffer);

    // send if there is kernal buffer space, else drop it
    _ = socket_state.send(UdpSockRef::from(&socket), &transmit);
}
