
use std::{collections::VecDeque, io::IoSliceMut, net::{SocketAddr, UdpSocket}, sync::Arc, time::Instant};

use bevy::{prelude::*, utils::{HashMap, HashSet}};
use quinn_udp::{RecvMeta, UdpSockRef, UdpSocketState};

pub use quinn_proto;


pub struct QuinnPlugin;
impl Plugin for QuinnPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<Connected>();

        app.add_systems(PreUpdate, update_endpoints);
    }
}


#[derive(Component)]
pub struct Endpoint {
    config: EndpointConfig,
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

pub struct EndpointConfig {
    pub bind_addr: SocketAddr,
    pub server_config: Option<quinn_proto::ServerConfig>,
    pub receive_budget: usize,
}

enum EndpointCallback {
    SuccessfulConnection(quinn_proto::ConnectionHandle),
    OpenedStream {
        stream_id: quinn_proto::StreamId,
        bi_directional: bool,
    },
    ClosedStream(quinn_proto::StreamId),
    FinishedStream(quinn_proto::StreamId),
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
            server_config: None,
            connections: HashMap::new(),
        })
    }

    fn udpate(&mut self, now: Instant, mut callback: impl FnMut(EndpointCallback)) {
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

                            debug!("processing {} bytes", remaining_data.len());

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
                                            let connection = ConnectionState::new(handle, connection, &self.config);
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

        for connection in self.connections.values_mut() {
            connection.update(
                now,
                &mut self.endpoint,
                &self.socket,
                &mut self.socket_state,
                |client_callback| match client_callback {
                    ClientCallback::SuccessfulConnection(connection_handle) => {
                        callback(EndpointCallback::SuccessfulConnection(connection_handle));
                    },
                    ClientCallback::OpenedStream { stream_id, bi_directional } => {
                        callback(EndpointCallback::OpenedStream { stream_id, bi_directional });
                    },
                    ClientCallback::ClosedStream(stream_id) => {
                        callback(EndpointCallback::ClosedStream(stream_id));
                    },
                    ClientCallback::FinishedStream(stream_id) => {
                        callback(EndpointCallback::FinishedStream(stream_id));
                    },
                },
            );
        }
    }

    pub fn connect(&mut self, client_config: quinn_proto::ClientConfig, addr: SocketAddr, server_name: &str) -> Result<(), quinn_proto::ConnectError> {
        let (handle, connection) = self.endpoint.connect(Instant::now(), client_config, addr, server_name)?;

        let connection = ConnectionState::new(handle, connection, &self.config);
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
    send_streams: HashMap<quinn_proto::StreamId, SendStreamState>,
    receive_budget: usize,
    read_data: HashMap<quinn_proto::StreamId, VecDeque<u8>>,
}

enum ClientCallback {
    SuccessfulConnection(quinn_proto::ConnectionHandle),
    OpenedStream {
        stream_id: quinn_proto::StreamId,
        bi_directional: bool,
    },
    ClosedStream(quinn_proto::StreamId),
    FinishedStream(quinn_proto::StreamId),
}

impl ConnectionState {
    fn new(handle: quinn_proto::ConnectionHandle, connection: quinn_proto::Connection, config: &EndpointConfig) -> Self {
        ConnectionState {
            connection,
            handle,
            connected: false,
            send_buffer: Vec::new(),
            readable_streams: HashSet::new(),
            send_streams: HashMap::new(),
            receive_budget: config.receive_budget,
            read_data: HashMap::new(),
        }
    }

    fn get_statistics(&self) -> quinn_proto::ConnectionStats {
        self.connection.stats()
    }

    fn update(&mut self, now: Instant, endpoint: &mut quinn_proto::Endpoint, socket: &UdpSocket, socket_state: &mut quinn_udp::UdpSocketState, mut callback: impl FnMut(ClientCallback)) {
        // perform polls as described on `quinn_proto::Connection`

        // poll transmits
        let max_datagrams = socket_state.max_gso_segments();

        loop {
            self.send_buffer.clear();
            self.send_buffer.reserve(self.connection.current_mtu() as usize);

            let Some(transmit) = self.connection.poll_transmit(now, max_datagrams, &mut self.send_buffer) else {
                break;
            };

            debug!("Transmitting {} bytes to: {}", transmit.size, transmit.destination);

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
                todo!("drop connection state");
            }
        }

        while let Some(event) = self.connection.poll() {
            match event {
                quinn_proto::Event::Connected => {
                    self.connected = true;
                    debug!("Connected successfully to {}", self.connection.remote_address());

                    callback(ClientCallback::SuccessfulConnection(self.handle));

                    let stream_id = self.open_uni();
                    self.get_send_stream_mut(stream_id).unwrap().write(&vec![69; 100]);
                },
                quinn_proto::Event::ConnectionLost { reason } => {
                    debug!("Connection lost with reason: {:?}", reason);
                },
                quinn_proto::Event::HandshakeDataReady => {
                    debug!("Handshake ready for connection!");
                },
                quinn_proto::Event::Stream(stream_event) => match stream_event {
                    quinn_proto::StreamEvent::Opened { dir } => {
                        debug!("Opened new stream on connection!");
                    },
                    quinn_proto::StreamEvent::Readable { id } => {
                        debug!("stream {} has readable data", id);
                        self.readable_streams.insert(id);
                    },
                    quinn_proto::StreamEvent::Writable { id } => {

                    },
                    quinn_proto::StreamEvent::Finished { id } => {

                    },
                    quinn_proto::StreamEvent::Stopped { id, error_code } => {

                    },
                    quinn_proto::StreamEvent::Available { dir } => {
                        debug!("stream is now available!");
                    },
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
            info!("peer opened unidirectional stream {}", stream_id);
            self.readable_streams.insert(stream_id);
        }

        while let Some(stream_id) = self.connection.streams().accept(quinn_proto::Dir::Bi) {
            // new bi directional stream
            info!("peer opened bidirectional stream {}", stream_id);
            self.readable_streams.insert(stream_id);
            self.send_streams.insert(stream_id, SendStreamState::default());
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
                if self.receive_budget == 0 {
                    break;
                }

                match chunks.next(self.receive_budget) {
                    Ok(None) | Err(quinn_proto::ReadError::Blocked) => {
                        // no more data is ready to be read
                        break;
                    },
                    Err(quinn_proto::ReadError::Reset(reason)) => {
                        // stream was reset
                    },
                    Ok(Some(chunk)) => {
                        let data = chunk.bytes.as_ref();
                        self.receive_budget = self.receive_budget.checked_sub(data.len()).expect("should not receive more than the receive budget");

                        info!("received {} bytes", data.len());

                        self.read_data.entry(stream_id).or_insert_with(VecDeque::default).extend(data);
                    }
                }
            }

            // no need to promt, will be done anyway
            let _ = chunks.finalize();
        }


        // write to streams
        for (&stream_id, stream_state) in self.send_streams.iter_mut() {
            loop {
                if stream_state.send_queue.is_empty() {
                    break;
                }

                let (slice_a, slice_b) = stream_state.send_queue.as_slices();
                let slice = if slice_a.is_empty() {slice_b} else {slice_a};

                match self.connection.send_stream(stream_id).write(slice) {
                    Ok(bytes) => {
                        stream_state.send_queue.drain(..bytes);
                    },
                    Err(quinn_proto::WriteError::Blocked) => break,
                    Err(quinn_proto::WriteError::ClosedStream) => panic!("Wrote to a closed stream"),
                    Err(quinn_proto::WriteError::Stopped(reason)) => panic!("Wrote to a stopped stream ({})", reason),
                }
            }
        }
    }

    fn open_uni(&mut self) -> quinn_proto::StreamId {
        let stream_id = self.connection.streams().open(quinn_proto::Dir::Uni).expect("Ran out of streams");
        self.send_streams.insert(stream_id, SendStreamState::default());
        stream_id
    }

    fn open_bi(&mut self) -> quinn_proto::StreamId {
        let stream_id = self.connection.streams().open(quinn_proto::Dir::Bi).expect("Ran out of streams");
        self.send_streams.insert(stream_id, SendStreamState::default());
        stream_id
    }

    fn get_send_stream_mut(&mut self, stream_id: quinn_proto::StreamId) -> Option<&mut SendStreamState> {
        self.send_streams.get_mut(&stream_id)
    }
}

#[derive(Default)]
struct SendStreamState {
    send_queue: VecDeque<u8>,
}

impl SendStreamState {
    fn write(&mut self, data: &[u8]) {
        self.send_queue.extend(data);
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Connection(quinn_proto::ConnectionHandle);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Stream(quinn_proto::StreamId);


#[derive(Event)]
pub struct Connected {
    pub endpoint_entity: Entity,
    pub connection: Connection,
}



fn update_endpoints(
    mut endpoint_q: Query<(Entity, &mut Endpoint)>,
    mut connected_w: EventWriter<Connected>,
) {
    let now = Instant::now();

    for (endpoint_entity, mut endpoint) in endpoint_q.iter_mut() {
        endpoint.udpate(now, |callback| match callback {
            EndpointCallback::SuccessfulConnection(connection_handle) => {
                connected_w.send(Connected {
                    endpoint_entity,
                    connection: Connection(connection_handle),
                });
            },
            EndpointCallback::OpenedStream { stream_id, bi_directional } => {

            },
            EndpointCallback::ClosedStream(stream_id) => {

            },
            EndpointCallback::FinishedStream(stream_id) => {

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
