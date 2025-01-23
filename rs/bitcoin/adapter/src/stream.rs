use bitcoin::io as bitcoin_io;
use bitcoin::{
    consensus::serialize,
    p2p::message::RawNetworkMessage,
    p2p::Magic,
    {consensus::encode, p2p::message::NetworkMessage},
};
use futures::TryFutureExt;
use http::Uri;
use ic_logger::{debug, error, info, ReplicaLogger};
use std::{io, net::SocketAddr, time::Duration};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::mpsc::{Sender, UnboundedReceiver},
    time::{sleep, timeout},
};
use tokio_socks::{tcp::Socks5Stream, Error as SocksError};

/// This provides a default amount of time to wait before a timeout occurs while
/// attempting to connect to a BTC node.
const CONNECTION_TIMEOUT_SECS: u64 = 5;

/// This constant represents the size of the buffer needed when reading messages
/// from the Bitcoin node.
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

/// This constant represents the maximum raw network message size we accept.
const MAX_RAW_MESSAGE_SIZE: usize = 40 * 1024 * 1024;

/// This enum is used to represent the possible errors that could occur while a stream
/// is connected.
#[derive(Debug, Error)]
pub enum StreamError {
    /// This variant is used to indicate that there was an error while connecting
    /// that involved the SOCKS proxy.
    #[error("{0}")]
    Socks(SocksError),
    /// This variant is used to indicate an error occurred while communicating
    /// using the TcpStream.
    #[error("{0}")]
    Io(io::Error),
    /// This variant is used to indicate an error while encoding or decoding the network
    /// message.
    #[error("{0}")]
    Encode(encode::Error),
    /// This variant is used to indicate that the stream has become disconnected
    /// from the parent task.
    #[error("This stream has become disconnected from the main task.")]
    UnableToReceiveMessages,
    /// This .
    #[error("Connecting to the stream timed out.")]
    Timeout,
    /// This .
    #[error("Received message exceeds maximum allowed size.")]
    TooLarge,
}

/// This type is a wrapper for results that contain StreamError.
pub type StreamResult<T> = Result<T, StreamError>;

/// This struct represents the configuration options for a Stream struct.
pub struct StreamConfig {
    /// This field represents the target address that the stream will connect to.
    pub address: SocketAddr,
    /// This field is used to provide an instance of the logger.
    pub logger: ReplicaLogger,
    /// This field is used to provide the magic value to the raw network message.
    /// The magic number is used to identity the type of Bitcoin network being accessed.
    pub magic: Magic,
    /// This field is used to receive network messages to send out to the connected
    /// BTC node.
    pub network_message_receiver: UnboundedReceiver<NetworkMessage>,
    /// This field represents the address that the stream may use to proxy
    /// requests to the address field.
    pub socks_proxy: Option<String>,
    /// This field is used to send events from the stream back to the network and connection structs.
    pub stream_event_sender: Sender<StreamEvent>,
    pub network_message_sender: Sender<(SocketAddr, NetworkMessage)>,
}

/// This struct is used to represent an event that has occurred within the Stream
/// struct.
#[derive(Eq, PartialEq, Debug)]
pub struct StreamEvent {
    /// This field is used to help identify which stream created the event.
    pub address: SocketAddr,
    /// This field is used to determine what happened with the stream.
    pub kind: StreamEventKind,
}

/// This enum is used to represent events generated by the Stream struct.
/// This is how we handle the stream interactions.
#[derive(Eq, PartialEq, Debug)]
pub enum StreamEventKind {
    /// This variant is used to indicate that the stream has been established.
    Connected,
    /// This variant is used to indicate that the stream has been disconnected.
    Disconnected,
    /// This variant is used to indicate that the connection failed due to an
    /// I/O error or timeout.
    FailedToConnect,
}

/// This struct is used to provide an interface with the raw socket that will
/// be connecting to the BTC node.
#[derive(Debug)]
pub struct Stream {
    /// This field is used to identity the node that the stream is connected to.
    address: SocketAddr,
    /// This field is used as the buffer for reading messages.
    data: Vec<u8>,
    /// This field contains the actual stream handling the network connection.
    read_half: OwnedReadHalf,
    write_half: OwnedWriteHalf,
    /// This field is used to provide the magic value to the raw network message.
    /// The magic number is used to identify the type of Bitcoin network being accessed.
    magic: Magic,
    /// This field contains the receiver used to intake messages that are to be
    /// sent to the connected node.
    network_message_receiver: UnboundedReceiver<NetworkMessage>,
    network_message_sender: Sender<(SocketAddr, NetworkMessage)>,
    /// This field is used as a buffer to contain unparsed message parts.
    unparsed: Vec<u8>,
}

impl Stream {
    /// Creates new SOCKS client. In case of missing proxy address we fall back to the direct TCP
    /// stream.
    pub async fn connect(config: StreamConfig, logger: &ReplicaLogger) -> StreamResult<Stream> {
        let StreamConfig {
            address,
            socks_proxy,
            magic,
            network_message_receiver,
            network_message_sender,
            ..
        } = config;

        let timeout_duration = Duration::from_secs(CONNECTION_TIMEOUT_SECS);
        let data = vec![0u8; STREAM_BUFFER_SIZE];
        let unparsed = vec![];

        let tcp_stream_attempt = timeout(
            timeout_duration,
            TcpStream::connect(&address).map_err(StreamError::Io),
        )
        .await
        .map_err(|_| StreamError::Timeout)?;

        // If connecting through the node socket fails we may do a second attempt through the socks proxy if it's configured.
        let stream = match tcp_stream_attempt {
            Ok(stream) => stream,
            Err(err) => {
                timeout(timeout_duration, async {
                    match socks_proxy {
                        Some(socks_proxy_addr) => {
                            // The socks stream::connect takes a socks proxy address that implements 'tokio_socks::ToProxyAddrs'.
                            // It uses the trait implementation to resolve the socks proxy address to a 'std::net::SocketAddr'
                            // The resolving assumes that the string has the following format: <host>:<port>.
                            // A badly formatted string is hard to spot since it can just resolve to nothing and a timeouts occur.
                            // By validating the proxy address config we are reasonably sure that we have a valid socks url.
                            let socks_addr_authority = socks_proxy_addr
                                .parse::<Uri>()
                                .map_err(|_| {
                                    // This should never happen since we validate the socks_proxy to be valid 'http::Uri' when reading the config.
                                    StreamError::Socks(SocksError::AddressTypeNotSupported)
                                })?
                                .authority()
                                .ok_or(
                                    // This should never happen since we validate the socks_proxy to be valid 'http::Uri' when reading the config.
                                    StreamError::Socks(SocksError::AddressTypeNotSupported),
                                )?
                                .to_owned();
                            Ok(
                                Socks5Stream::connect(socks_addr_authority.as_str(), address)
                                    .map_err(StreamError::Socks)
                                    .await?
                                    .into_inner(),
                            )
                        }
                        None => {
                            debug!(
                                logger,
                                "No direct connectivity to bitcoin peer {} and no socks proxy available.",
                                address
                            );
                            Err(err)
                        }
                    }
                })
                .await
                .map_err(|_| StreamError::Timeout)??
            }
        };

        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            address,
            data,
            read_half,
            write_half,
            magic,
            network_message_receiver,
            network_message_sender,
            unparsed,
        })
    }

    /// This function reads a message from the inner TcpStream.
    pub fn read_message(&mut self) -> StreamResult<RawNetworkMessage> {
        loop {
            // This means that in the previous iteration we failed to decode a `RawNetworkMessage`
            // and it was larger than `MAX_RAW_MESSAGE_SIZE`. In that case we return an error and
            // disconnect from this peer.
            if self.unparsed.len() > MAX_RAW_MESSAGE_SIZE + STREAM_BUFFER_SIZE {
                return Err(StreamError::TooLarge);
            }
            // The stream may only a message partial from the Bitcoin node.
            // Due to this, the stream must attempt to deserialize partial messages.
            match encode::deserialize_partial::<RawNetworkMessage>(&self.unparsed) {
                // If there was an I/O error found in the unparsed message and it was an unexpected
                // end-of-file, then the stream should try to read again. If the read fails, the stream
                // exits the read message with the error. The stream later looks at this error, if the
                // kind is WouldBlock, the stream continues; otherwise, the stream will disconnect.
                // If the read successfully received bytes, then the bytes are added to the
                // unparsed buffer to attempt another deserialize call. If no bytes found,
                // return the unexpected end-of-file error.
                Err(encode::Error::Io(ref err))
                    if err.kind() == bitcoin_io::ErrorKind::UnexpectedEof =>
                {
                    let count = self
                        .read_half
                        .try_read(&mut self.data)
                        .map_err(StreamError::Io)?;

                    if count == 0 {
                        return Err(StreamError::Io(io::Error::from(
                            io::ErrorKind::UnexpectedEof,
                        )));
                    }

                    if let Some(slice) = self.data.get(0..count) {
                        self.unparsed.extend(slice.iter());
                    }
                }
                // If an error occurred, that is not an unexpected end-of-file, unwrap the error
                // and then re-wrap it into a StreamError.
                Err(err) => {
                    return Err(match err {
                        encode::Error::Io(err) => StreamError::Io(err.into()),
                        err => StreamError::Encode(err),
                    });
                }
                // If the message can be parsed, the unparsed buffer is drained and the message
                // is returned.
                Ok((message, index)) => {
                    self.unparsed.drain(..index);
                    return Ok(message);
                }
            }
        }
    }

    /// This function is used to write a network message to the connected Bitcoin
    /// node.
    async fn write_message(&mut self, network_message: NetworkMessage) -> StreamResult<()> {
        let raw_network_message = RawNetworkMessage::new(self.magic, network_message);
        let bytes = serialize(&raw_network_message);
        self.write_half
            .write_all(bytes.as_slice())
            .await
            .map_err(StreamError::Io)?;
        self.write_half.flush().await.map_err(StreamError::Io)
    }

    /// This function is used to handle a single iteration of the stream.
    /// First, the stream writes the latest message on the network message receiver to the BTC node.
    /// Second, the stream attempt to read a message from the BTC node. If no message is found,
    /// the stream tick completes. If a message is found, the stream sends a StreamEvent on the
    /// stream event sender so the Network struct may react.
    async fn tick(&mut self) -> StreamResult<()> {
        if let Ok(network_message) = self.network_message_receiver.try_recv() {
            self.write_message(network_message).await?;
        }

        let raw_message = self.read_message()?;
        let result = self
            .network_message_sender
            .send((self.address, raw_message.payload().clone()))
            .await;
        if result.is_err() {
            return Err(StreamError::UnableToReceiveMessages);
        }

        Ok(())
    }
}

/// This function is used to kick off a new stream that will be connected to a
/// the Network struct and related connection struct via a set of channels.
pub fn handle_stream(config: StreamConfig) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        let address = config.address;
        let logger = config.logger.clone();
        // Clone the sender here to handle errors that the Stream may return.
        let stream_event_sender = config.stream_event_sender.clone();
        let stream_result = Stream::connect(config, &logger).await;
        let mut stream = match stream_result {
            Ok(stream) => {
                stream_event_sender
                    .send(StreamEvent {
                        address,
                        kind: StreamEventKind::Connected,
                    })
                    .await
                    .ok();
                stream
            }
            Err(err) => {
                info!(every_n_seconds => 300, &logger, "Failed to connect to {} ::: {}", address, err);
                let kind = match err {
                    StreamError::Io(_) => StreamEventKind::FailedToConnect,
                    StreamError::Timeout => StreamEventKind::FailedToConnect,
                    _ => {
                        error!(logger, "{}", err);
                        StreamEventKind::Disconnected
                    }
                };
                stream_event_sender
                    .send(StreamEvent { address, kind })
                    .await
                    .ok();
                return;
            }
        };

        loop {
            let result = stream.tick().await;
            if let Err(err) = result {
                let disconnect = match err {
                    StreamError::Io(io_err) => match io_err.kind() {
                        io::ErrorKind::WouldBlock => {
                            sleep(Duration::from_millis(100)).await;
                            false
                        }
                        _ => true,
                    },
                    _ => true,
                };

                if disconnect {
                    stream_event_sender
                        .send(StreamEvent {
                            address,
                            kind: StreamEventKind::Disconnected,
                        })
                        .await
                        .ok();
                }
            }
        }
    })
}

#[cfg(test)]
pub mod test {

    use std::net::{IpAddr, Ipv4Addr};

    use crate::common::DEFAULT_CHANNEL_BUFFER_SIZE;

    use super::*;
    use bitcoin::{consensus::Encodable, Network};
    use ic_logger::replica_logger::no_op_logger;

    /// Test that large messages get rejected and we disconnect as a consequence.
    #[allow(clippy::disallowed_methods)]
    #[tokio::test]
    async fn read_huge_message_from_network() {
        let network = Network::Bitcoin;
        let (net_tx, _net_rx) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);
        let (_adapter_tx, adapter_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let stream_config = StreamConfig {
            address,
            logger: no_op_logger(),
            magic: network.magic(),
            network_message_receiver: adapter_rx,
            socks_proxy: None,
            stream_event_sender: stream_tx,
            network_message_sender: net_tx,
        };

        let _stream_handle = handle_stream(stream_config);
        // Wait till we connect.
        assert_eq!(
            stream_rx.recv().await.unwrap(),
            StreamEvent {
                address,
                kind: StreamEventKind::Connected
            }
        );

        // Send message that exceeds size limit.
        tokio::spawn(async move {
            let (mut socket, _addr) = listener.accept().await.unwrap();
            let addr = RawNetworkMessage::new(
                network.magic(),
                NetworkMessage::Alert(vec![0; MAX_RAW_MESSAGE_SIZE + 10]),
            );
            let mut buf = Vec::new();
            let raw_addr = addr.consensus_encode(&mut buf).unwrap();
            socket.write_all(&buf[..raw_addr]).await.unwrap();
        });

        assert_eq!(
            stream_rx.recv().await.unwrap(),
            StreamEvent {
                address,
                kind: StreamEventKind::Disconnected
            }
        );
    }

    /// Test that connection initialization times out in 5 seconds, to ensure the connection attempts
    /// in the connection manager do not hang for a long period of time.
    #[tokio::test]
    async fn initialization_times_out_after_five_seconds() {
        let network = Network::Bitcoin;
        let (net_tx, _) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);
        #[allow(clippy::disallowed_methods)]
        let (_adapter_tx, adapter_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stream_tx, _) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);

        // Try to connect to a non routable IP address to force a timeout to happen. If a routable IP is used,
        // then the connection either succeeds or other errors are generated.
        // https://stackoverflow.com/questions/100841/artificially-create-a-connection-timeout-error
        // The chosen ephemeral port is random and should not affect the test.
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 0)), 55535);

        let stream_config = StreamConfig {
            address,
            logger: no_op_logger(),
            magic: network.magic(),
            network_message_receiver: adapter_rx,
            socks_proxy: None,
            stream_event_sender: stream_tx,
            network_message_sender: net_tx,
        };

        let stream_result = Stream::connect(stream_config, &no_op_logger()).await;
        let err = stream_result.unwrap_err();
        assert!(matches!(err, StreamError::Timeout));
    }

    /// Test that .
    #[tokio::test]
    async fn read_two_messages_at_size_boundary() {
        let network = Network::Bitcoin;
        let (net_tx, mut net_rx) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);
        #[allow(clippy::disallowed_methods)]
        let (_adapter_tx, adapter_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let stream_config = StreamConfig {
            address,
            logger: no_op_logger(),
            magic: network.magic(),
            network_message_receiver: adapter_rx,
            socks_proxy: None,
            stream_event_sender: stream_tx,
            network_message_sender: net_tx,
        };

        let _stream_handle = handle_stream(stream_config);
        // Wait till we connect.
        assert_eq!(
            stream_rx.recv().await.unwrap(),
            StreamEvent {
                address,
                kind: StreamEventKind::Connected
            }
        );

        // Large messgage just below limit.
        let payload_large = RawNetworkMessage::new(
            network.magic(),
            NetworkMessage::Alert(vec![0; MAX_RAW_MESSAGE_SIZE - 30]),
        );
        let mut buf_large = Vec::new();
        let _ = payload_large.consensus_encode(&mut buf_large).unwrap();

        // Message that crosses the boundary limit.
        let payload_small = RawNetworkMessage::new(
            network.magic(),
            NetworkMessage::Alert(vec![0; 31 + STREAM_BUFFER_SIZE]),
        );
        let mut buf_small = Vec::new();
        let _ = payload_small.consensus_encode(&mut buf_small).unwrap();

        buf_large.append(&mut buf_small);
        tokio::spawn(async move {
            let (mut socket, _addr) = listener.accept().await.unwrap();
            socket.write_all(&buf_large).await.unwrap();
        });

        assert_eq!(
            net_rx.recv().await.unwrap(),
            (address, payload_large.payload().clone())
        );
        assert_eq!(
            net_rx.recv().await.unwrap(),
            (address, payload_small.payload().clone())
        );
    }
}
