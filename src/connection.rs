
use std::{collections::VecDeque, net::UdpSocket, time::Instant};

use bevy::{prelude::*, utils::{HashSet, HashMap}};
use quinn_udp::{UdpSockRef, UdpSocketState};

use crate::{udp_transmit, EndpointConfig};

pub struct Connection {
    pub(crate) connection: quinn_proto::Connection,
    handle: quinn_proto::ConnectionHandle,
    connected: bool,
    send_buffer: Vec<u8>,
    /// a set of streams that have data ready to be read
    readable_streams: HashSet<quinn_proto::StreamId>,
    receive_streams: HashMap<quinn_proto::StreamId, ReceiveBuffer>,
    send_streams: HashMap<quinn_proto::StreamId, SendBuffer>,
    receive_budget: usize,
}

#[derive(Default)]
pub struct SendBuffer {
    buffer: VecDeque<u8>,
    /// when true the stream will be closed after all data has been sent,
    /// and no more data can be written to the stream
    closed: bool,
}

#[derive(Default)]
pub struct ReceiveBuffer {
    buffer: VecDeque<u8>,
    closed: bool,
}

pub(crate) enum ConnectionCallback {
    SuccessfulConnection(quinn_proto::ConnectionHandle),

    /// the peer opened a receive stream
    OpenedReceiveStream(quinn_proto::StreamId),
    /// the peer closed/reset a receive stream,
    /// including an error code if the stream was reset
    ClosedReceiveStream(quinn_proto::StreamId, Option<quinn_proto::VarInt>),
    /// a receive stream's data was entirely read
    FinishedReceiveStream(quinn_proto::StreamId),

    /// the peer opened a send stream
    OpenedSendStream(quinn_proto::StreamId),
    /// the peer closed a send stream
    ClosedSendStream(quinn_proto::StreamId, quinn_proto::VarInt),

    /// remove the connection state after update is finished being called
    CloseConnection,
}

impl Connection {
    pub(crate) fn new(handle: quinn_proto::ConnectionHandle, connection: quinn_proto::Connection, config: &EndpointConfig) -> Self {
        Connection {
            connection,
            handle,
            connected: false,
            send_buffer: Vec::new(),
            readable_streams: HashSet::new(),
            receive_streams: HashMap::new(),
            send_streams: HashMap::new(),
            receive_budget: config.receive_budget,
        }
    }

    /// perform polls as described on `quinn_proto::Connection`
    pub(crate) fn update(&mut self, now: Instant, endpoint: &mut quinn_proto::Endpoint, socket: &UdpSocket, socket_state: &mut UdpSocketState, mut callback: impl FnMut(ConnectionCallback)) {

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


        // callback to poll for application events after `handle_timeout` and `handle_event` as described by `quinn_proto::Connection::poll`
        let mut poll = |connection: &mut quinn_proto::Connection| while let Some(event) = connection.poll() {
            match event {
                quinn_proto::Event::Connected => {
                    self.connected = true;
                    debug!("Connected successfully to {}", connection.remote_address());
                    callback(ConnectionCallback::SuccessfulConnection(self.handle));
                },
                quinn_proto::Event::ConnectionLost { reason } => {
                    debug!("Connection lost with reason: {:?}", reason);
                },
                quinn_proto::Event::HandshakeDataReady => {
                },
                quinn_proto::Event::Stream(stream_event) => match stream_event {
                    quinn_proto::StreamEvent::Opened { .. } => {
                    },
                    quinn_proto::StreamEvent::Readable { id } => {
                        debug!("{} readable", id);
                        self.readable_streams.insert(id);
                    },
                    quinn_proto::StreamEvent::Writable { .. } => {
                    },
                    quinn_proto::StreamEvent::Finished { id } => {
                        debug!("stream finished ({})", id);
                    },
                    quinn_proto::StreamEvent::Stopped { id, error_code } => {
                        debug!("stream stopped ({})", id);
                        self.send_streams.remove(&id);
                        callback(ConnectionCallback::ClosedSendStream(id, error_code));
                    },
                    quinn_proto::StreamEvent::Available { .. } => {
                    },
                },
                quinn_proto::Event::DatagramReceived => {
                },
                quinn_proto::Event::DatagramsUnblocked => {
                },
            }
        };


        if let Some(deadline) = self.connection.poll_timeout() {
            if deadline >= now {
                self.connection.handle_timeout(now);
                poll(&mut self.connection);
            }
        }


        let mut close_connection = false;

        while let Some(endpoint_event) = self.connection.poll_endpoint_events() {
            // this is the last event the connection will emit, the connection state can be freed
            close_connection |= endpoint_event.is_drained();

            if let Some(connection_event) = endpoint.handle_event(self.handle, endpoint_event) {
                self.connection.handle_event(connection_event);
                poll(&mut self.connection);
            }
        }

        if close_connection {
            callback(ConnectionCallback::CloseConnection);
        }


        // accept new streams
        while let Some(stream_id) = self.connection.streams().accept(quinn_proto::Dir::Uni) {
            // new uni directional stream
            debug!("peer ({}) opened stream ({})", self.connection.remote_address(), stream_id);
            self.readable_streams.insert(stream_id);
            self.receive_streams.insert(stream_id, ReceiveBuffer::default());
            callback(ConnectionCallback::OpenedReceiveStream(stream_id));
        }

        while let Some(stream_id) = self.connection.streams().accept(quinn_proto::Dir::Bi) {
            // new bi directional stream
            debug!("peer ({}) opened stream ({})", self.connection.remote_address(), stream_id);
            self.readable_streams.insert(stream_id);
            self.receive_streams.insert(stream_id, ReceiveBuffer::default());
            self.send_streams.insert(stream_id, SendBuffer::default());
            callback(ConnectionCallback::OpenedReceiveStream(stream_id));
            callback(ConnectionCallback::OpenedSendStream(stream_id));
        }


        // read streams
        for stream_id in self.readable_streams.drain() {
            let mut stream = self.connection.recv_stream(stream_id);

            let mut chunks = match stream.read(true) {
                Err(quinn_proto::ReadableError::ClosedStream) => panic!("stream should exist"),
                Err(quinn_proto::ReadableError::IllegalOrderedRead) => panic!("should never read unordered"),
                Ok(chunks) => chunks,
            };

            loop {
                if self.receive_budget == 0 {
                    break;
                }

                let stream = self.receive_streams.get_mut(&stream_id).expect("stream should exist");

                match chunks.next(self.receive_budget) {
                    Ok(None) => {
                        debug!("stream finished ({})", stream_id);
                        stream.closed = true;
                        callback(ConnectionCallback::ClosedReceiveStream(stream_id, None));
                        break;
                    },
                    Err(quinn_proto::ReadError::Reset(reason)) => {
                        debug!("stream reset ({}) {}", stream_id, reason);
                        stream.closed = true;
                        callback(ConnectionCallback::ClosedReceiveStream(stream_id, Some(reason)));
                        break;
                    },
                    Err(quinn_proto::ReadError::Blocked) => {
                        // no more data is ready to be read
                        break;
                    },
                    Ok(Some(chunk)) => {
                        let data = chunk.bytes.as_ref();
                        self.receive_budget = self.receive_budget.checked_sub(data.len()).expect("should not receive more than the receive budget");

                        debug!("received {} bytes", data.len());

                        stream.buffer.extend(data);
                    }
                }
            }

            // no need to promt, will be done anyway
            let _ = chunks.finalize();
        }


        // remove finished receive streams
        self.receive_streams.retain(|&stream_id, stream| {
            if stream.closed && stream.buffer.is_empty() {
                callback(ConnectionCallback::FinishedReceiveStream(stream_id));
                false
            } else {
                true
            }
        });


        // write to streams
        self.send_streams.retain(|&stream_id, stream| {
            loop {
                if stream.buffer.is_empty() {
                    // if the stream is empty and closed, remove it
                    if stream.closed {
                        debug!("finishing {}", stream_id);
                        self.connection.send_stream(stream_id).finish().expect("stream should not be closed or stopped");
                        break false;
                    } else {
                        break true;
                    }
                }

                let (slice_a, slice_b) = stream.buffer.as_slices();
                let slice = if slice_a.is_empty() {slice_b} else {slice_a};

                match self.connection.send_stream(stream_id).write(slice) {
                    Ok(bytes) => {
                        stream.buffer.drain(..bytes);
                    },
                    Err(quinn_proto::WriteError::Blocked) => break true,
                    Err(quinn_proto::WriteError::ClosedStream) => panic!("Wrote to a closed stream"),
                    Err(quinn_proto::WriteError::Stopped(reason)) => panic!("Wrote to a stopped stream ({})", reason),
                }
            }
        });
    }

    /// gets connection statistics
    pub fn get_statistics(&self) -> quinn_proto::ConnectionStats {
        self.connection.stats()
    }

    /// get data negotiated during the handshake, if available
    pub fn get_handshake_data(&self) -> Option<Box<dyn std::any::Any>> {
        self.connection.crypto_session().handshake_data()
    }

    /// opens a unidirectional stream and creates a [SendBuffer] with the returned [StreamId]
    ///
    /// returns `None` if streams are exhausted
    pub fn open_uni(&mut self) -> Option<StreamId> {
        let stream_id = self.connection.streams().open(quinn_proto::Dir::Uni)?;
        self.send_streams.insert(stream_id, SendBuffer::default());
        Some(StreamId(stream_id))
    }

    /// opens a bidirectional stream and creates a [SendBuffer] with the returned [StreamId]
    ///
    /// returns `None` if streams are exhausted
    pub fn open_bi(&mut self) -> Option<StreamId> {
        let stream_id = self.connection.streams().open(quinn_proto::Dir::Bi)?;
        self.send_streams.insert(stream_id, SendBuffer::default());
        Some(StreamId(stream_id))
    }

    /// gets the send buffer for a stream
    pub fn get_send_stream(&self, stream_id: StreamId) -> Option<&SendBuffer> {
        self.send_streams.get(&stream_id.0)
    }

    /// gets the send buffer for a stream
    pub fn get_send_stream_mut(&mut self, stream_id: StreamId) -> Option<&mut SendBuffer> {
        self.send_streams.get_mut(&stream_id.0)
    }

    /// gets the receive buffer for a stream
    pub fn get_receive_stream(&self, stream_id: StreamId) -> Option<&ReceiveBuffer> {
        self.receive_streams.get(&stream_id.0)
    }

    /// gets the receive buffer for a stream
    pub fn get_receive_stream_mut(&mut self, stream_id: StreamId) -> Option<&mut ReceiveBuffer> {
        self.receive_streams.get_mut(&stream_id.0)
    }

    /// resets a send stream, abandoning transmitting any unsent data
    ///
    /// fails if the stream has not been open or is finished and has been closed
    pub fn reset_stream(&mut self, stream_id: StreamId, error_code: quinn_proto::VarInt) -> Result<(), ()> {
        if let Ok(()) = self.connection.send_stream(stream_id.0).reset(error_code) {
            self.send_streams.remove(&stream_id.0);
            Ok(())
        } else {
            Err(())
        }
    }
}

impl SendBuffer {
    /// writes some data to the stream
    ///
    /// continuing to call write even if the endpoint is congested will cause a memory leak,
    /// use [SendBuffer::bytes_buffered] to control how much data you are writing to the buffer
    ///
    /// this method will return `false` and no data will be written if the stream has been closed
    pub fn write(&mut self, data: &[u8]) -> bool {
        if self.closed {
            false
        } else {
            self.buffer.extend(data);
            true
        }
    }

    /// returns the number of bytes in the buffer that haven't been sent,
    /// either due to congestion or becuase the endpoint hasn't been update yet
    pub fn bytes_buffered(&self) -> usize {
        self.buffer.len()
    }

    /// closes the stream and prevents any more data from being written
    pub fn finish(&mut self) {
        self.closed = true
    }
}

impl ReceiveBuffer {
    /// reads every available byte
    pub fn read(&mut self) -> Box<[u8]> {
        Vec::from(std::mem::replace(&mut self.buffer, VecDeque::new())).into()
    }

    /// tries to read an exact amount of bytes from the buffer,
    /// leaving the remaining in the buffer.
    ///
    /// will return `None` if there are less than that many bytes available
    pub fn read_exact(&mut self, bytes: usize) -> Option<Box<[u8]>> {
        if self.buffer.len() < bytes {
            return None;
        }

        Some(Vec::from(self.buffer.split_off(bytes)).into())
    }

    /// returns the number of available bytes in the buffer
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// returns whether the stream has been closed and no more data will be received,
    /// however there could still be data in the buffer that hasn't been read
    pub fn closed(&self) -> bool {
        self.closed
    }
}

/// the id of a connection belonging to an [Endpoint](crate::endpoint::Endpoint)
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ConnectionId(pub(crate) quinn_proto::ConnectionHandle);

/// the id of a stream belonging to a [Connection]
///
/// could refer to a send or receive stream or both if the stream is bidirectional
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct StreamId(pub(crate) quinn_proto::StreamId);
