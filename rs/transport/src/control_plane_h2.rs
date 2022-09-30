//! Control plane - Transport connection management.
//!
//! The control plane handles tokio/TLS related details of connection
//! management. This component establishes/accepts connections to/from subnet
//! peers. The component also manages re-establishment of severed connections.

use crate::{
    data_plane_h2::{create_connected_state_read_path, create_connected_state_write_path},
    metrics::{IntGaugeResource, STATUS_ERROR, STATUS_SUCCESS},
    types::{
        Connecting, ConnectionRole, ConnectionState, PeerStateH2, QueueSize, ServerPortState,
        TransportImplH2,
    },
    utils::{connect_to_server, get_peer_label, start_listener},
};
use ic_base_types::{NodeId, RegistryVersion};
use ic_crypto_tls_interfaces::{AllowedClients, AuthenticatedPeer, TlsStream};
use ic_interfaces_transport::{
    TransportChannelId, TransportError, TransportEvent, TransportEventHandler,
};
use ic_logger::{error, warn};
use std::{net::SocketAddr, time::Duration};
use strum::AsRefStr;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
    task::JoinHandle,
    time::sleep,
};
use tower::Service;

#[derive(Debug, AsRefStr)]
#[strum(serialize_all = "snake_case")]
enum TransportTlsHandshakeError {
    DeadlineExceeded,
    Internal(String),
    InvalidArgument,
}

/// Time to wait before retrying an unsuccessful connection attempt
const CONNECT_RETRY_SECONDS: u64 = 3;

/// Time to wait for the TLS handshake (for both client/server sides)
const TLS_HANDSHAKE_TIMEOUT_SECONDS: u64 = 30;

const CONNECT_TASK_NAME: &str = "connect";
const ACCEPT_TASK_NAME: &str = "accept";
const TRANSITION_FROM_ACCEPT_TASK_NAME: &str = "transition_from_accept";

/// Implementation for the transport control plane
impl TransportImplH2 {
    /// Stops connection to a peer
    pub(crate) fn stop_peer_connection(&self, peer_id: &NodeId) {
        self.allowed_clients.blocking_write().remove(peer_id);
        self.peer_map.blocking_write().remove(peer_id);
    }

    /// Starts connection(s) to a peer and initializes the corresponding data
    /// structures and tasks
    pub(crate) fn start_peer_connection(
        &self,
        peer_id: &NodeId,
        peer_addr: SocketAddr,
        registry_version: RegistryVersion,
    ) -> Result<(), TransportError> {
        self.allowed_clients.blocking_write().insert(*peer_id);
        *self.registry_version.blocking_write() = registry_version;
        let mut peer_map = self.peer_map.blocking_write();
        if peer_map.get(peer_id).is_some() {
            return Err(TransportError::AlreadyExists);
        }

        let peer_label = get_peer_label(&peer_addr.ip().to_string(), peer_id);
        // TODO: P2P-514
        let channel_id = TransportChannelId::from(self.config.legacy_flow_tag);

        let connecting_task = self.spawn_connect_task(channel_id, *peer_id, peer_addr);
        let connecting_state = Connecting {
            peer_addr,
            connecting_task,
        };

        let peer_state = PeerStateH2::new(
            self.log.clone(),
            channel_id,
            peer_label,
            ConnectionState::Connecting(connecting_state),
            ConnectionState::Listening,
            QueueSize::from(self.config.send_queue_size),
            self.send_queue_metrics.clone(),
            self.control_plane_metrics.clone(),
        );
        peer_map.insert(*peer_id, RwLock::new(peer_state));
        Ok(())
    }

    pub(crate) fn init_client(&self, event_handler: TransportEventHandler) {
        // Creating the listeners requres that we are within a tokio runtime context.
        let _rt_enter_guard = self.rt_handle.enter();
        let server_addr = SocketAddr::new(self.node_ip, self.config.listening_port);
        let tcp_listener = start_listener(server_addr).unwrap_or_else(|err| {
            panic!(
                "Failed to init listener: local_addr = {:?}, error = {:?}",
                server_addr, err
            )
        });

        let channel_id = TransportChannelId::from(self.config.legacy_flow_tag);
        let accept_task = self.spawn_accept_task(channel_id, tcp_listener);
        *self.accept_port.blocking_lock() = Some(ServerPortState { accept_task });
        *self.event_handler.blocking_lock() = Some(event_handler);
    }

    /// Starts the async task to accept the incoming TcpStreams in server mode.
    fn spawn_accept_task(
        &self,
        channel_id: TransportChannelId,
        tcp_listener: TcpListener,
    ) -> JoinHandle<()> {
        let weak_self = self.weak_self.read().unwrap().clone();
        let rt_handle = self.rt_handle.clone();
        let async_tasks_gauge_vec = self.control_plane_metrics.async_tasks.clone();
        self.rt_handle.spawn(async move {
            let task_gauge = async_tasks_gauge_vec.with_label_values(&[ACCEPT_TASK_NAME]);
            let _gauge_guard = IntGaugeResource::new(task_gauge);
            loop {
                // If the TransportImpl has been deleted, abort.
                let arc_self = match weak_self.upgrade() {
                    Some(arc_self) => arc_self,
                    _ => return,
                };
                match tcp_listener.accept().await {
                    Ok((stream, _)) => {
                        arc_self.control_plane_metrics
                            .tcp_accepts
                            .with_label_values(&[STATUS_SUCCESS])
                            .inc();

                        let (local_addr, peer_addr) = match (stream.local_addr(), stream.peer_addr()) {
                            (Ok(local_addr), Ok(peer_addr)) => (local_addr, peer_addr),
                            _ => {
                                error!(
                                    arc_self.log,
                                    "ControlPlane::spawn_accept_task(): local_addr() and/or peer_addr() failed."
                                );
                                continue;
                            }
                        };

                        if let Err(err) = stream.set_nodelay(true) {
                            error!(
                                arc_self.log,
                                "ControlPlane::spawn_accept_task(): set_nodelay(true) failed: \
                                error = {:?}, local_addr = {:?}, peer_addr = {:?}",
                                err,
                                local_addr,
                                peer_addr,
                            );
                            continue;
                        }

                        rt_handle.spawn(async move {
                            let task_gauge = arc_self.control_plane_metrics.async_tasks.with_label_values(&[TRANSITION_FROM_ACCEPT_TASK_NAME]);
                            let _gauge_guard = IntGaugeResource::new(task_gauge);
                            let (peer_id, tls_stream) = match arc_self.tls_server_handshake(stream).await {
                                Ok((peer_id, tls_stream)) => {
                                    arc_self.control_plane_metrics
                                        .tls_handshakes
                                        .with_label_values(&[ConnectionRole::Server.as_ref(), STATUS_SUCCESS])
                                        .inc();
                                    (peer_id, tls_stream)
                                },
                                Err(err) => {
                                    arc_self.control_plane_metrics
                                        .tls_handshakes
                                        .with_label_values(&[ConnectionRole::Server.as_ref(), err.as_ref()])
                                       .inc();
                                    warn!(
                                        arc_self.log,
                                        "ControlPlane::spawn_accept_task(): tls_server_handshake failed: error = {:?},
                                        local_addr = {:?}, peer_addr = {:?}",
                                        err,
                                        local_addr,
                                        peer_addr,
                                    );
                                    return;
                                }
                            };

                            let peer_map = arc_self.peer_map.read().await;
                            let peer_state_mu = match peer_map.get(&peer_id) {
                                Some(peer_state) => peer_state,
                                None => return,
                            };
                            let mut peer_state = peer_state_mu.write().await;
                            if peer_state.get_connected(ConnectionRole::Server).is_some() {
                                // TODO: P2P-516
                                return;
                            }
                            let event_handler = match arc_self.event_handler.lock().await.as_ref() {
                                Some(event_handler) => event_handler.clone(),
                                None => return,
                            };
                            let connected_state = create_connected_state_read_path(
                                peer_id,
                                channel_id,
                                peer_state.peer_label.clone(),
                                peer_state.send_queue.get_reader(),
                                ConnectionRole::Server,
                                peer_addr,
                                tls_stream,
                                event_handler,
                                arc_self.data_plane_metrics.clone(),
                                arc_self.weak_self.read().unwrap().clone(),
                                arc_self.rt_handle.clone(),
                            );
                            peer_state.update(ConnectionState::ConnectedH2(connected_state), ConnectionRole::Server);
                            arc_self.control_plane_metrics
                                .tcp_accept_conn_success
                                .with_label_values(&[&channel_id.to_string()])
                                    .inc()
                        });
                    }
                    Err(err) => {
                        arc_self.control_plane_metrics
                            .tcp_accepts
                            .with_label_values(&[STATUS_ERROR])
                            .inc();
                        error!(arc_self.log, "ControlPlane::spawn_accept_task(): accept failed: error = {:?}", err);
                    }
                }
            }
        })
    }

    /// Spawn a task that tries to connect to a peer (forever, or until
    /// connection is established or peer is removed)
    fn spawn_connect_task(
        &self,
        channel_id: TransportChannelId,
        peer_id: NodeId,
        peer_addr: SocketAddr,
    ) -> JoinHandle<()> {
        let node_ip = self.node_ip;
        let weak_self = self.weak_self.read().unwrap().clone();
        let async_tasks_gauge_vec = self.control_plane_metrics.async_tasks.clone();
        self.rt_handle.spawn(async move {
            let gauge = async_tasks_gauge_vec.with_label_values(&[CONNECT_TASK_NAME]);
            let _raii_gauge_vec = IntGaugeResource::new(gauge);
            let local_addr = SocketAddr::new(node_ip, 0);

            // Loop till connection is established
            let mut retries: u32 = 0;
            loop {
                retries += 1;
                // If the TransportImpl has been deleted, abort.
                let arc_self = match weak_self.upgrade() {
                    Some(arc_self) => arc_self,
                    _ => return,
                };
                // We currently retry forever, which is fine as we have per-connection
                // async task. This loop will terminate when the peer is removed from
                // valid set.
                match connect_to_server(local_addr, peer_addr).await {
                    Ok(stream) => {
                        arc_self.control_plane_metrics
                            .tcp_connects
                            .with_label_values(&[STATUS_SUCCESS])
                            .inc();

                        let peer_map = arc_self.peer_map.read().await;
                        let peer_state_mu = match peer_map.get(&peer_id) {
                            Some(peer_state) => peer_state,
                            None => continue,
                        };
                        let mut peer_state = peer_state_mu.write().await;
                        if peer_state.get_connected(ConnectionRole::Client).is_some() {
                            // TODO: P2P-516
                            continue;
                        }
                        let tls_stream = match arc_self.tls_client_handshake(peer_id, stream).await {
                            Ok(tls_stream) => {
                                arc_self.control_plane_metrics
                                .tls_handshakes
                                    .with_label_values(&[ConnectionRole::Client.as_ref(), STATUS_SUCCESS])
                                    .inc();
                                tls_stream
                            }
                            Err(err) => {
                                arc_self.control_plane_metrics
                                    .tls_handshakes
                                    .with_label_values(&[ConnectionRole::Client.as_ref(), err.as_ref()])
                                    .inc();
                                warn!(
                                    arc_self.log,
                                    "ControlPlane::spawn_connect_task(): tls_client_handshake failed: error = {:?},
                                    local_addr = {:?}, peer_addr = {:?}",
                                    err,
                                    local_addr,
                                    peer_addr,
                                );
                                continue;
                            }
                        };

                        let mut event_handler = match arc_self.event_handler.lock().await.as_ref() {
                            Some(event_handler) => event_handler.clone(),
                            None => continue,
                        };
                        let connected_state = create_connected_state_write_path(
                            peer_id,
                            channel_id,
                            peer_state.peer_label.clone(),
                            peer_state.send_queue.get_reader(),
                            ConnectionRole::Client,
                            peer_addr,
                            tls_stream,
                            event_handler.clone(),
                            arc_self.data_plane_metrics.clone(),
                            arc_self.weak_self.read().unwrap().clone(),
                            arc_self.rt_handle.clone(),

                        );
                        event_handler
                            .call(TransportEvent::PeerUp(peer_id))
                            .await
                            .expect("Can't panic on infallible");
                        peer_state.update(ConnectionState::ConnectedH2(connected_state), ConnectionRole::Client);
                        arc_self.control_plane_metrics
                            .tcp_conn_to_server_success
                            .with_label_values(&[
                                &peer_id.to_string(),
                                &channel_id.to_string(),
                            ])
                            .inc();
                        return;
                    }
                    Err(err) => {
                        arc_self.control_plane_metrics
                            .tcp_connects
                            .with_label_values(&[STATUS_ERROR])
                            .inc();
                        warn!(
                            arc_self.log,
                            "ControlPlane::spawn_connect_task(): connect_to_server failed: error = {:?}, \
                            local_addr = {:?}, peer = {:?}/{:?}, retries = {}",
                            err,
                            local_addr,
                            peer_id,
                            peer_addr,
                            retries,
                        );
                    }
                }
                sleep(Duration::from_secs(CONNECT_RETRY_SECONDS)).await;
            }
        })
    }

    /// Retries to establish a connection
    pub(crate) async fn _on_disconnect(
        &self,
        peer_id: NodeId,
        channel_id: TransportChannelId,
        connection_role: ConnectionRole,
    ) {
        warn!(
            self.log,
            "ControlPlane::retry_connection(): node_id = {:?}, channel_id = {:?}, peer_id = {:?}",
            self._node_id,
            channel_id,
            peer_id
        );
        let peer_map = self.peer_map.read().await;
        let peer_state_mu = match peer_map.get(&peer_id) {
            Some(peer_state) => peer_state,
            None => return,
        };
        let mut peer_state = peer_state_mu.write().await;
        let connected = match peer_state.get_connected(connection_role) {
            Some(connected) => connected,
            // Channel is already disconnected/reconnecting, skip reconnect processing
            None => return,
        };
        let mut event_handler = match self.event_handler.lock().await.as_ref() {
            Some(event_handler) => event_handler.clone(),
            None => return,
        };
        self.control_plane_metrics
            .retry_connection
            .with_label_values(&[&peer_id.to_string(), &channel_id.to_string()])
            .inc();

        let socket_addr = connected.peer_addr;
        let connection_state = if connection_role == ConnectionRole::Server {
            // We are the server, wait for the peer to connect
            warn!(
                self.log,
                "ControlPlane::process_disconnect(): waiting for peer to reconnect: \
                 node_id = {:?}, channel_id = {:?}, peer_id = {:?}",
                self._node_id,
                channel_id,
                peer_id
            );
            ConnectionState::Listening
        } else {
            // reconnect if we have a listener
            let connecting_task = self.spawn_connect_task(channel_id, peer_id, socket_addr);
            let connecting_state = Connecting {
                peer_addr: socket_addr,
                connecting_task,
            };
            warn!(
                self.log,
                "ControlPlane::process_disconnect(): spawning reconnect task: node = {:?}/{:?}, \
                channel_id = {:?}, peer = {:?}/{:?}, peer_port = {:?}",
                self._node_id,
                self.node_ip,
                channel_id,
                peer_id,
                socket_addr.ip(),
                socket_addr.port(),
            );
            ConnectionState::Connecting(connecting_state)
        };
        event_handler
            .call(TransportEvent::PeerDown(peer_id))
            .await
            .expect("Can't panic on infallible");
        peer_state.update(connection_state, connection_role);
    }

    /// Performs the server side TLS hand shake processing
    async fn tls_server_handshake(
        &self,
        stream: TcpStream,
    ) -> Result<(NodeId, TlsStream), TransportTlsHandshakeError> {
        let registry_version = *self.registry_version.read().await;
        let current_allowed_clients = self.allowed_clients.read().await.clone();
        let allowed_clients = AllowedClients::new_with_nodes(current_allowed_clients)
            .map_err(|_| TransportTlsHandshakeError::InvalidArgument)?;
        let (tls_stream, authenticated_peer) = match tokio::time::timeout(
            Duration::from_secs(TLS_HANDSHAKE_TIMEOUT_SECONDS),
            self.crypto
                .perform_tls_server_handshake(stream, allowed_clients, registry_version),
        )
        .await
        {
            Err(_) => Err(TransportTlsHandshakeError::DeadlineExceeded),
            Ok(Ok((tls_stream, authenticated_peer))) => Ok((tls_stream, authenticated_peer)),
            Ok(Err(err)) => Err(TransportTlsHandshakeError::Internal(format!("{:?}", err))),
        }?;
        let AuthenticatedPeer::Node(peer_id) = authenticated_peer;
        Ok((peer_id, tls_stream))
    }

    /// Performs the client side TLS hand shake processing
    async fn tls_client_handshake(
        &self,
        peer_id: NodeId,
        stream: TcpStream,
    ) -> Result<TlsStream, TransportTlsHandshakeError> {
        let registry_version = *self.registry_version.read().await;
        match tokio::time::timeout(
            Duration::from_secs(TLS_HANDSHAKE_TIMEOUT_SECONDS),
            self.crypto
                .perform_tls_client_handshake(stream, peer_id, registry_version),
        )
        .await
        {
            Err(_) => Err(TransportTlsHandshakeError::DeadlineExceeded),
            Ok(Ok(tls_stream)) => Ok(tls_stream),
            Ok(Err(err)) => Err(TransportTlsHandshakeError::Internal(format!("{:?}", err))),
        }
    }
}
