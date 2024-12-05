// SPDX-License-Identifier: MIT OR Apache-2.0

//! Node implementation for p2p networking and data streaming, extensible with discovery
//! strategies, sync protocols, blob sync and more.
//!
//! As soon as the local node is launched it roughly attempts the following goal:
//!
//! 1. Find as many peers as possible who are interested in the same topic
//! 2. Connect with as many peers as possible to exchange data with, eventually converging to the
//!    same state
//!
//! To achieve this goal, multiple systems are in play:
//!
//! ## Bootstrap
//!
//! Establishing a peer-to-peer network suffers from the "chicken and egg" problem where we need to
//! start _somewhere_ before we can begin to discover more and become "discoverable" for other,
//! possibly previously unknown peers.
//!
//! This process of the "first step" into a network is called "bootstrap" and can be realised with
//! different strategies:
//!
//! 1. Supply your node with a hard-coded list of well-known node addresses which are directly
//!    reachable. The node will attempt connecting them on start.
//! 2. Use techniques like mDNS or rendesvouz servers to learn about other nodes to connect to.
//!
//! The latter approach is very similar to "peer discovery" with one important difference: While
//! peer discovery helps us to learn about _more peers_ we use these discovery techniques during
//! bootstrap to find _the first_ peer to connect to.
//!
//! ## Peer Discovery
//!
//! Like "Bootstrap" we can apply similar algorithms to find more peers in the network. In
//! `p2panda-net` this is an "ambient" process, meaning that it constantly takes place in the
//! background, as soon as the node is running.
//!
//! To find more peers which potentially might be interested in the same data as the local node,
//! one or more discovery techniques can be added when creating the network, for example mDNS for
//! finding peers in the local network or a "Rendesvouz" server for finding peers on the internet.
//!
//! Additionally `p2panda-net` might find new peers through joining any gossip overlay. If another
//! peer becomes a direct neighbor in the gossip tree, we register it in our address book.
//!
//! ## Topic Discovery
//!
//! Next to "Peer Discovery" we additionally find out what peers are interested in and announce our
//! own topics of interest in the network. With this design it is possible to have peers in the
//! network being interested in different data, potentially using other applications, at the same
//! time.
//!
//! By default `p2panda-net` always enters at least one network-wide gossip overlay where peers
//! exchange information over this information. In the future we might use a random-walk traversal
//! algorithm instead to "explore" the network.
//!
//! As soon as we've identified a common interest in the same topic, we're joining the gossip
//! overlay for this topic with them and earmark this peer for a future sync session.
//!
//! ## Connectivity
//!
//! With the help of iroh, we can connect to any device whereever they are. There is a multi-step
//! process requiring additional strategies depending on the situation to connect to a peer:
//!
//! 1. If we know a peer's directly reachable address, we can just connect to them
//! 2. If this peer's direct address is not known (because they are behind a NAT or we only know
//!    their public key) we use a STUN server to find out the address
//! 3. If this peer is still not reachable (for example because they are behind a Firewall), we use
//!    a Relay (TURN-like) server to handle the connection through it
//!
//! In the last case we can't establish a direct connection and rely on additional infrastructure.
//!
//! Read the `iroh-relay` documentation to learn about how to run your own STUN and Relay server:
//! <https://github.com/n0-computer/iroh/tree/main/iroh-relay>.
//!
//! This implementation aims at being robust in situations where bad or no connectivity is
//! (temporarily) given. `p2panda-net` will automatically re-connect to peers as soon as they are
//! reachable again which, from the application perspective, shouldn't make any difference.
//!
//! ## "Live Mode"
//!
//! To send newly created messages fastly to other peers, `p2panda-net` uses broadcast gossip
//! overlays for each topic. With this "live mode" messages arrive almost instantly.
//!
//! ## Sync
//!
//! By learning about other peers who are interested in the same topic we keep track of them in an
//! internal "address book". A sync session mananger process will eventually kick in a sync session
//! with one of these peers and try to exchange _all_ data we're so far missing on that topic with
//! them.
//!
//! Sync is disabled by default and can be enabled by adding a `SyncProtocol` implementation to the
//! node.
//!
//! Sync sessions are only running once per peer per topic but can optionally be re-attempted after
//! a certain duration if a `ResyncConfiguration` was given.
//!
//! ## Gossip Buffer
//!
//! Since a node receives potentially older data from another node during a sync session,
//! `p2panda-net` uses a "gossip buffer" to make sure that we're buffering new messages which were
//! received at the same time during the gossip overlay.
//!
//! The buffer is released after the sync session has ended. Through this trick we can make sure
//! that messages are arriving "in order" (based on their timestamp or partial ordering for
//! example) to higher application layers.
//!
//! Please note that this implementation can never fully be sure that messages will arrive "out of
//! order" to the application. It is recommended to apply additional buffering if this is required.
//! In `p2panda-streams` we offer a solution which will take care of that.
//!
//! ## Blobs
//!
//! With the help of the `p2panda-blobs` crate it is possible to extend the node to support
//! efficient sync of large binary data (images, files etc.) with various storage backends.
//!
//! ## Custom Protocols
//!
//! Next to blob sync, data sync or discovery protocols it is also possible to register any other
//! low-level bi-directional communication protocol to the node when necessary.
use std::fmt::Debug;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_lite::StreamExt;
use futures_util::future::{MapErr, Shared};
use futures_util::{FutureExt, TryFutureExt};
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use iroh_gossip::proto::Config as GossipConfig;
use iroh_net::endpoint::TransportConfig;
use iroh_net::key::SecretKey;
use iroh_net::relay::{RelayMap, RelayNode};
use iroh_net::{Endpoint, NodeAddr, NodeId};
use p2panda_core::{PrivateKey, PublicKey};
use p2panda_discovery::{Discovery, DiscoveryMap};
use p2panda_sync::Topic;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, error, error_span, warn, Instrument};

use crate::addrs::DEFAULT_STUN_PORT;
use crate::config::{Config, DEFAULT_BIND_PORT};
use crate::engine::Engine;
use crate::protocols::{ProtocolHandler, ProtocolMap};
use crate::sync::{SyncConfiguration, SYNC_CONNECTION_ALPN};
use crate::{NetworkId, RelayUrl, TopicId};

/// Maximum number of streams accepted on a QUIC connection.
const MAX_STREAMS: u32 = 1024;

/// Timeout duration for receiving of at least one peer's direct address.
const DIRECT_ADDRESSES_WAIT: Duration = Duration::from_secs(5);

/// Relay server configuration mode.
#[derive(Debug, PartialEq)]
pub enum RelayMode {
    /// No relay has been specified.
    ///
    /// To connect to another peer it's direct address needs to be known, otherwise any connection
    /// attempt will fail.
    Disabled,

    /// Specify a custom relay.
    ///
    /// Relays are used to help establishing a connection in case the direct address is not known
    /// yet (via STUN). In case this process fails (for example due to a firewall), the relay is
    /// used as a fallback to tunnel traffic from one peer to another (via DERP, which is similar
    /// to TURN).
    ///
    /// Important: Peers need to use the _same_ relay address to be able to connect to each other.
    Custom(RelayNode),
}

/// Builds an overlay network for peers grouped under the same network identifier.
///
/// All peers can subscribe to multiple topics in this overlay and hook into a data stream per
/// topic where they'll send and receive data.
#[derive(Debug)]
pub struct NetworkBuilder<T> {
    bind_port: Option<u16>,
    direct_node_addresses: Vec<NodeAddr>,
    discovery: DiscoveryMap,
    gossip_config: Option<GossipConfig>,
    network_id: NetworkId,
    protocols: ProtocolMap,
    relay_mode: RelayMode,
    secret_key: Option<SecretKey>,
    sync_config: Option<SyncConfiguration<T>>,
}

impl<T> NetworkBuilder<T>
where
    T: Topic,
{
    /// Returns a new instance of `NetworkBuilder` using the given network identifier.
    ///
    /// Networks must use the same identifier if they wish to successfully connect and share
    /// data.
    pub fn new(network_id: NetworkId) -> Self {
        Self {
            bind_port: None,
            direct_node_addresses: Vec::new(),
            discovery: DiscoveryMap::default(),
            gossip_config: None,
            network_id,
            protocols: Default::default(),
            relay_mode: RelayMode::Disabled,
            secret_key: None,
            sync_config: None,
        }
    }

    /// Returns a new instance of `NetworkBuilder` using the given configuration.
    pub fn from_config(config: Config) -> Self {
        let mut network_builder = Self::new(config.network_id).bind_port(config.bind_port);

        for (public_key, addresses, relay_addr) in config.direct_node_addresses {
            network_builder = network_builder.direct_address(public_key, addresses, relay_addr)
        }

        if let Some(url) = config.relay {
            let port = url.port().unwrap_or(DEFAULT_STUN_PORT);
            network_builder = network_builder.relay(url, false, port)
        }

        network_builder
    }

    /// Sets or overwrites the local bind port for IPv4 sockets.
    ///
    /// Note that IPv6 sockets are automatically bound to the specified port + 1.
    pub fn bind_port(mut self, port: u16) -> Self {
        self.bind_port.replace(port);
        self
    }

    /// Sets or overwrites the private key.
    ///
    /// If this value is not set, the `NetworkBuilder` will generate a new, random key when
    /// building the network.
    pub fn private_key(mut self, private_key: PrivateKey) -> Self {
        self.secret_key = Some(SecretKey::from_bytes(private_key.as_bytes()));
        self
    }

    /// Sets the relay used by the local network to facilitate the establishment of direct
    /// connections.
    ///
    /// Relay nodes are STUN servers which help in establishing a peer-to-peer connection if one or
    /// both of the peers are behind a NAT. The relay node might offer proxy functionality on top
    /// (via the Tailscale DERP protocol which is very similar to TURN) if the connection attempt
    /// fails, which will serve to relay the data in that case.
    pub fn relay(mut self, url: RelayUrl, stun_only: bool, stun_port: u16) -> Self {
        self.relay_mode = RelayMode::Custom(RelayNode {
            url: url.into(),
            stun_only,
            stun_port,
        });
        self
    }

    /// Sets the direct address of a peer, identified by their public key (node id).
    ///
    /// The direct address should be reachable without the aid of a STUN or TURN-based relay node.
    /// However, if the direct connection attempt might fail (for example, because of a NAT or
    /// Firewall), the relay node of that peer can be supplied to allow connecting to it via a
    /// fallback connection.
    ///
    /// If no relay address is given but turns out to be required, we optimistically try to use our
    /// own relay node instead (if specified). This might still fail, as we can't know if the peer
    /// is using the same relay node.
    pub fn direct_address(
        mut self,
        node_id: PublicKey,
        addresses: Vec<SocketAddr>,
        relay_addr: Option<RelayUrl>,
    ) -> Self {
        let node_id = NodeId::from_bytes(node_id.as_bytes()).expect("invalid public key");
        let mut node_addr = NodeAddr::new(node_id).with_direct_addresses(addresses);
        if let Some(url) = relay_addr {
            node_addr = node_addr.with_relay_url(url.into());
        }
        self.direct_node_addresses.push(node_addr);
        self
    }

    /// Adds one or more discovery strategy, such as mDNS.
    pub fn discovery(mut self, handler: impl Discovery + 'static) -> Self {
        self.discovery.add(handler);
        self
    }

    /// Sets the sync protocol and configuration.
    ///
    /// Sync sessions will be automatically initiated with any known peers with whom we share
    /// topics of interest.
    pub fn sync(mut self, config: SyncConfiguration<T>) -> Self {
        self.sync_config = Some(config);
        self
    }

    /// Sets the gossip configuration.
    ///
    /// Configuration parameters define the behavior of the swarm membership (HyParView) and gossip
    /// broadcast (Plumtree) layers, as well as the maximum message size.
    pub fn gossip(mut self, config: GossipConfig) -> Self {
        self.gossip_config = Some(config);
        self
    }

    /// Adds additional, custom protocols for communication between two peers.
    pub fn protocol(
        mut self,
        protocol_name: &'static [u8],
        handler: impl ProtocolHandler + 'static,
    ) -> Self {
        self.protocols.insert(protocol_name, Arc::new(handler));
        self
    }

    /// Returns a handle to a newly-spawned instance of `Network`.
    ///
    /// A peer-to-peer endpoint is created and bound to a QUIC socket, after which the gossip,
    /// engine and connection handlers are instantiated. A sync handler is also instantiated if a
    /// sync protocol is provided. Direct addresses for network peers are added to the engine from
    /// the address book and core protocols are registered.
    ///
    /// After configuration and registration processes are complete, the network is spawned and an
    /// attempt is made to retrieve a direct address for a network peer so that a connection may be
    /// made. If no address is retrieved within the timeout limit, the network is shut down and an
    /// error is returned.
    pub async fn build(mut self) -> Result<Network<T>>
    where
        T: Topic + TopicId + 'static,
    {
        let secret_key = self.secret_key.unwrap_or(SecretKey::generate());

        let relay: Option<RelayNode> = match self.relay_mode {
            RelayMode::Disabled => None,
            RelayMode::Custom(ref node) => Some(node.clone()),
        };

        // Build p2p endpoint and bind the QUIC socket.
        let endpoint = {
            let mut transport_config = TransportConfig::default();
            transport_config
                .max_concurrent_bidi_streams(MAX_STREAMS.into())
                .max_concurrent_uni_streams(0u32.into());

            let relay_mode = match self.relay_mode {
                RelayMode::Disabled => iroh_net::relay::RelayMode::Disabled,
                RelayMode::Custom(node) => iroh_net::relay::RelayMode::Custom(
                    RelayMap::from_nodes(vec![node])
                        .expect("relay list can not contain duplicates"),
                ),
            };

            // @TODO: Expose finer-grained config options. Right now we only provide the option of
            // defining the IPv4 port; everything else is hard-coded.
            let bind_port = self.bind_port.unwrap_or(DEFAULT_BIND_PORT);
            let socket_address_v4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, bind_port);
            let socket_address_v6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, bind_port + 1, 0, 0);

            Endpoint::builder()
                .transport_config(transport_config)
                .secret_key(secret_key.clone())
                .relay_mode(relay_mode)
                .bind_addr_v4(socket_address_v4)
                .bind_addr_v6(socket_address_v6)
                .bind()
                .await?
        };

        let node_addr = endpoint.node_addr().await?;

        let gossip = Gossip::from_endpoint(
            endpoint.clone(),
            self.gossip_config.unwrap_or_default(),
            &node_addr.info,
        );

        let engine = Engine::new(
            secret_key.clone(),
            self.network_id,
            endpoint.clone(),
            gossip.clone(),
            self.sync_config,
        );

        let sync_handler = engine.sync_handler();

        let inner = Arc::new(NetworkInner {
            cancel_token: CancellationToken::new(),
            relay: relay.clone(),
            discovery: self.discovery,
            endpoint: endpoint.clone(),
            engine,
            gossip: gossip.clone(),
            network_id: self.network_id,
            secret_key,
        });

        self.protocols.insert(GOSSIP_ALPN, Arc::new(gossip.clone()));
        if let Some(sync_handler) = sync_handler {
            self.protocols
                .insert(SYNC_CONNECTION_ALPN, Arc::new(sync_handler));
        };
        let protocols = Arc::new(self.protocols.clone());
        let alpns = self.protocols.alpns();
        if let Err(err) = inner.endpoint.set_alpns(alpns) {
            inner.shutdown(protocols.clone()).await;
            return Err(err);
        }

        let fut = inner
            .clone()
            .spawn(protocols.clone())
            .instrument(error_span!("node", me=%node_addr.node_id.fmt_short()));
        let task = tokio::task::spawn(fut);
        let task_handle = AbortOnDropHandle::new(task)
            .map_err(Box::new(|err: JoinError| err.to_string()) as JoinErrToStr)
            .shared();

        let network = Network {
            inner,
            task: task_handle,
            protocols,
        };

        // Wait for a single direct address update, to make sure we found at least one direct
        // address.
        let wait_for_endpoints = {
            async move {
                tokio::time::timeout(DIRECT_ADDRESSES_WAIT, endpoint.direct_addresses().next())
                    .await
                    .context("waiting for endpoint")?
                    .context("no endpoints given to establish at least one connection")?;
                Ok(())
            }
        };

        if let Err(err) = wait_for_endpoints.await {
            network.shutdown().await.ok();
            return Err(err);
        }

        for mut direct_addr in self.direct_node_addresses {
            if direct_addr.relay_url().is_none() {
                // If given address does not hold any relay information we optimistically add ours
                // (if we have one). It's not guaranteed that this address will have the same relay
                // url as we have, but it's better than nothing!
                if let Some(ref relay_node) = relay {
                    direct_addr = direct_addr.with_relay_url(relay_node.url.clone());
                }
            }

            network.add_peer(direct_addr.clone()).await?;
        }

        Ok(network)
    }
}

#[derive(Debug)]
struct NetworkInner<T> {
    cancel_token: CancellationToken,
    relay: Option<RelayNode>,
    discovery: DiscoveryMap,
    endpoint: Endpoint,
    engine: Engine<T>,
    #[allow(dead_code)]
    gossip: Gossip,
    network_id: NetworkId,
    #[allow(dead_code)]
    secret_key: SecretKey,
}

impl<T> NetworkInner<T>
where
    T: Topic + TopicId + 'static,
{
    /// Spawns a network.
    ///
    /// Local network sockets are bound and a task is started to listen for direct addresses
    /// changes for the local endpoint. Inbound connection attempts to these endpoints are passed
    /// to a handler.
    ///
    /// Any registered discovery services are subscribed to so that the identifiers and addresses
    /// of peers operating on the same network may be learned. Discovered peers are added to the
    /// local address book so they may be involved in connection and gossip activites.
    async fn spawn(self: Arc<Self>, protocols: Arc<ProtocolMap>) {
        let (ipv4, ipv6) = self.endpoint.bound_sockets();
        debug!(
            "listening at: {}{}",
            ipv4,
            ipv6.map(|addr| format!(" and {addr}")).unwrap_or_default()
        );

        let mut join_set = JoinSet::<Result<()>>::new();

        // Spawn a task that updates the gossip endpoints and discovery services.
        {
            let inner = self.clone();
            join_set.spawn(async move {
                let mut addrs_stream = inner.endpoint.direct_addresses();
                let mut my_node_addr = NodeAddr::new(inner.endpoint.node_id());
                if let Some(node) = &inner.relay {
                    my_node_addr = my_node_addr.with_relay_url(node.url.to_owned());
                }

                loop {
                    tokio::select! {
                        // Learn about our direct addresses and changes to them.
                        Some(endpoints) = addrs_stream.next() => {
                            let direct_addresses = endpoints.iter().map(|endpoint| endpoint.addr).collect();
                            my_node_addr.info.direct_addresses = direct_addresses;
                            if let Err(err) = inner.discovery.update_local_address(&my_node_addr) {
                                warn!("failed to update direct addresses for discovery: {err:?}");
                            }
                        },
                        else => break,
                    }
                }

                Ok(())
            });
        }

        // Subscribe to all discovery channels where we might find new peers.
        let mut discovery_stream = self
            .discovery
            .subscribe(self.network_id)
            .expect("discovery map needs to be given");

        loop {
            tokio::select! {
                // Do not let tokio select futures randomly but with top-to-bottom priority.
                biased;
                // Exit loop when shutdown was signalled somewhere else.
                _ = self.cancel_token.cancelled() => {
                    break;
                },
                // Handle incoming p2p connections.
                Some(incoming) = self.endpoint.accept() => {
                    // @TODO: This is the point at which we can reject the connection if limits
                    // have been reached.
                    let connecting = match incoming.accept() {
                        Ok(connecting) => connecting,
                        Err(err) => {
                            warn!("incoming connection failed: {err:#}");
                            // This may be caused by retransmitted datagrams so we continue.
                            continue;
                        },
                    };
                    let protocols = protocols.clone();
                    join_set.spawn(async move {
                        handle_connection(connecting, protocols).await;
                        Ok(())
                    });
                },
                // Handle discovered peers.
                Some(event) = discovery_stream.next() => {
                    match event {
                        Ok(event) => {
                            if let Err(err) = self.engine.add_peer(event.node_addr).await {
                                error!("engine failed on add_peer: {err:?}");
                                break;
                            }
                        }
                        Err(err) => {
                            error!("discovery service failed: {err:?}");
                            break;
                        },
                    }
                },
                // Handle task terminations and quit on panics.
                res = join_set.join_next(), if !join_set.is_empty() => {
                    match res {
                        Some(Err(outer)) => {
                            if outer.is_panic() {
                                error!("task panicked: {outer:?}");
                                break;
                            } else if outer.is_cancelled() {
                                debug!("task cancelled: {outer:?}");
                            } else {
                                error!("task failed: {outer:?}");
                                break;
                            }
                        }
                        Some(Ok(Err(inner))) => {
                            debug!("task errored: {inner:?}");
                        }
                        _ => {}
                    }
                },
                else => break,
            }
        }

        self.shutdown(protocols).await;

        // Abort remaining tasks.
        join_set.shutdown().await;
    }

    /// Closes all connections and shuts down the network engine.
    async fn shutdown(&self, protocols: Arc<ProtocolMap>) {
        // We ignore all errors during shutdown.
        debug!("close all connections and shutdown the node");
        let _ = tokio::join!(
            // Closing the Endpoint is the equivalent of calling `Connection::close` on all
            // connections: Operations will immediately fail with `ConnectionError::LocallyClosed`.
            // All streams are interrupted, this is not graceful.
            self.endpoint
                .clone()
                .close(1u32.into(), b"provider terminating"),
            self.engine.shutdown(),
            protocols.shutdown(),
        );
    }
}

/// Running peer-to-peer node.
///
/// The primary feature of the `Network` is the ability to subscribe to one or more topics and
/// exchange messages over those topics with remote peers. Replication can be conducted exclusively
/// in "live-mode" or may include the synchronisation of past state, thereby ensuring eventual
/// consistency among all peers for a given topic. Replication and discovery strategies are defined
/// in the `NetworkBuilder`.
///
/// In addition to topic subscription, `Network` offers a way to access information about the local
/// network such as the node ID and direct addresses. It also provides a convenient means to add the
/// address of a remote peer and to query the addresses of all known peers.
#[derive(Clone, Debug)]
pub struct Network<T> {
    inner: Arc<NetworkInner<T>>,
    #[allow(dead_code)]
    protocols: Arc<ProtocolMap>,
    // `Network` needs to be `Clone + Send` and we need to `task.await` in its `shutdown()` impl.
    // - `Shared` allows us to `task.await` from all `Network` clones
    //   - Acts like an `Arc` around the inner future
    // - `MapErr` is needed to map the `JoinError` to a `String`, since `JoinError` is `!Clone`
    // - `AbortOnDropHandle` ensures the `task` is cancelled when all `Network`s are dropped
    task: Shared<MapErr<AbortOnDropHandle<()>, JoinErrToStr>>,
}

impl<T> Network<T>
where
    T: Topic + TopicId + 'static,
{
    /// Adds a peer to the address book.
    pub async fn add_peer(&self, node_addr: NodeAddr) -> Result<()> {
        self.inner.engine.add_peer(node_addr).await
    }

    /// Returns the addresses of all known peers.
    pub async fn known_peers(&self) -> Result<Vec<NodeAddr>> {
        self.inner.engine.known_peers().await
    }

    /// Returns the direct addresses of this node.
    pub async fn direct_addresses(&self) -> Option<Vec<SocketAddr>> {
        self.inner
            .endpoint
            .direct_addresses()
            .next()
            .await
            .map(|addrs| addrs.into_iter().map(|direct| direct.addr).collect())
    }

    /// Returns a handle to the network endpoint.
    ///
    /// The `Endpoint` exposes low-level networking functionality such as the ability to connect to
    /// specific peers, accept connections, query local socket addresses and more.
    ///
    /// This level of control is unlikely to be required in most cases but has been exposed for the
    /// convenience of advanced users.
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// Returns the public key of the node.
    pub fn node_id(&self) -> PublicKey {
        PublicKey::from_bytes(self.inner.endpoint.node_id().as_bytes())
            .expect("public key already checked")
    }

    /// Terminates all internal tasks and shuts down the node.
    pub async fn shutdown(self) -> Result<()> {
        // Trigger shutdown of the main run task by activating the cancel token.
        self.inner.cancel_token.cancel();

        // Wait for the main task to terminate.
        self.task.await.map_err(|err| anyhow!(err))?;

        Ok(())
    }

    /// Subscribes to a topic and returns a bi-directional stream that can be read from and written
    /// to, along with a oneshot receiver to be informed when the gossip overlay has been joined.
    pub async fn subscribe(
        &self,
        topic: T,
    ) -> Result<(
        mpsc::Sender<ToNetwork>,
        mpsc::Receiver<FromNetwork>,
        oneshot::Receiver<()>,
    )> {
        let (to_network_tx, to_network_rx) = mpsc::channel::<ToNetwork>(128);
        let (from_network_tx, from_network_rx) = mpsc::channel::<FromNetwork>(128);
        let (gossip_ready_tx, gossip_ready_rx) = oneshot::channel();

        self.inner
            .engine
            .subscribe(topic, from_network_tx, to_network_rx, gossip_ready_tx)
            .await?;

        Ok((to_network_tx, from_network_rx, gossip_ready_rx))
    }
}

/// An event to be broadcast to the network.
#[derive(Clone, Debug)]
pub enum ToNetwork {
    Message { bytes: Vec<u8> },
}

/// An event received from the network.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FromNetwork {
    GossipMessage {
        bytes: Vec<u8>,
        delivered_from: PublicKey,
    },
    SyncMessage {
        header: Vec<u8>,
        payload: Option<Vec<u8>>,
        delivered_from: PublicKey,
    },
}

/// Handle an inbound connection on the local network endpoint.
///
/// The connection is accepted if the handshake is successful and the peer is operating with
/// a supported ALPN protocol.
async fn handle_connection(
    mut connecting: iroh_net::endpoint::Connecting,
    protocols: Arc<ProtocolMap>,
) {
    let alpn = match connecting.alpn().await {
        Ok(alpn) => alpn,
        Err(err) => {
            warn!("ignoring connection: invalid handshake: {:?}", err);
            return;
        }
    };
    let Some(handler) = protocols.get(&alpn) else {
        warn!("ignoring connection: unsupported alpn protocol");
        return;
    };
    if let Err(err) = handler.accept(connecting).await {
        warn!("handling incoming connection ended with error: {err}");
    }
}

/// Helper to construct shared `AbortOnDropHandle` coming from tokio crate.
pub(crate) type JoinErrToStr =
    Box<dyn Fn(tokio::task::JoinError) -> String + Send + Sync + 'static>;

#[cfg(test)]
pub(crate) mod sync_protocols {
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures_lite::{AsyncRead, AsyncWrite, StreamExt};
    use futures_util::{Sink, SinkExt};
    use p2panda_sync::cbor::{into_cbor_sink, into_cbor_stream};
    use p2panda_sync::{FromSync, SyncError, SyncProtocol};
    use serde::{Deserialize, Serialize};
    use tracing::debug;

    use super::tests::TestTopic;

    #[derive(Debug, Serialize, Deserialize)]
    enum DummyProtocolMessage {
        Topic(TestTopic),
        Done,
    }

    /// A sync implementation which fulfills basic protocol requirements but nothing more
    #[derive(Debug)]
    pub struct DummyProtocol {}

    #[async_trait]
    impl<'a> SyncProtocol<'a, TestTopic> for DummyProtocol {
        fn name(&self) -> &'static str {
            static DUMMY_PROTOCOL_NAME: &str = "dummy_protocol";
            DUMMY_PROTOCOL_NAME
        }
        async fn initiate(
            self: Arc<Self>,
            topic: TestTopic,
            tx: Box<&'a mut (dyn AsyncWrite + Send + Unpin)>,
            rx: Box<&'a mut (dyn AsyncRead + Send + Unpin)>,
            mut app_tx: Box<
                &'a mut (dyn Sink<FromSync<TestTopic>, Error = SyncError> + Send + Unpin),
            >,
        ) -> Result<(), SyncError> {
            debug!("DummyProtocol: initiate sync session");

            let mut sink = into_cbor_sink(tx);
            let mut stream = into_cbor_stream(rx);

            sink.send(DummyProtocolMessage::Topic(topic.clone()))
                .await?;
            sink.send(DummyProtocolMessage::Done).await?;
            app_tx.send(FromSync::HandshakeSuccess(topic)).await?;

            while let Some(result) = stream.next().await {
                let message: DummyProtocolMessage = result?;
                debug!("message received: {:?}", message);

                match &message {
                    DummyProtocolMessage::Topic(_) => panic!(),
                    DummyProtocolMessage::Done => break,
                }
            }

            sink.flush().await?;
            app_tx.flush().await?;

            Ok(())
        }

        async fn accept(
            self: Arc<Self>,
            tx: Box<&'a mut (dyn AsyncWrite + Send + Unpin)>,
            rx: Box<&'a mut (dyn AsyncRead + Send + Unpin)>,
            mut app_tx: Box<
                &'a mut (dyn Sink<FromSync<TestTopic>, Error = SyncError> + Send + Unpin),
            >,
        ) -> Result<(), SyncError> {
            debug!("DummyProtocol: accept sync session");

            let mut sink = into_cbor_sink(tx);
            let mut stream = into_cbor_stream(rx);

            while let Some(result) = stream.next().await {
                let message: DummyProtocolMessage = result?;
                debug!("message received: {:?}", message);

                match &message {
                    DummyProtocolMessage::Topic(topic) => {
                        app_tx
                            .send(FromSync::HandshakeSuccess(topic.clone()))
                            .await?
                    }
                    DummyProtocolMessage::Done => break,
                }
            }

            sink.send(DummyProtocolMessage::Done).await?;

            sink.flush().await?;
            app_tx.flush().await?;

            Ok(())
        }
    }

    // The protocol message types.
    #[derive(Serialize, Deserialize)]
    enum Message {
        Topic(TestTopic),
        Ping,
        Pong,
    }

    #[derive(Debug, Clone)]
    pub struct PingPongProtocol {}

    /// A ping-pong sync protocol
    #[async_trait]
    impl<'a> SyncProtocol<'a, TestTopic> for PingPongProtocol {
        fn name(&self) -> &'static str {
            static SIMPLE_PROTOCOL_NAME: &str = "simple_protocol";
            SIMPLE_PROTOCOL_NAME
        }

        async fn initiate(
            self: Arc<Self>,
            topic: TestTopic,
            tx: Box<&'a mut (dyn AsyncWrite + Send + Unpin)>,
            rx: Box<&'a mut (dyn AsyncRead + Send + Unpin)>,
            mut app_tx: Box<
                &'a mut (dyn Sink<FromSync<TestTopic>, Error = SyncError> + Send + Unpin),
            >,
        ) -> Result<(), SyncError> {
            debug!("initiate sync session");
            let mut sink = into_cbor_sink(tx);
            let mut stream = into_cbor_stream(rx);

            sink.send(Message::Topic(topic.clone())).await?;
            sink.send(Message::Ping).await?;
            debug!("ping message sent");

            app_tx.send(FromSync::HandshakeSuccess(topic)).await?;

            while let Some(result) = stream.next().await {
                let message = result?;

                match message {
                    Message::Topic(_) => panic!(),
                    Message::Ping => {
                        return Err(SyncError::UnexpectedBehaviour(
                            "unexpected Ping message received".to_string(),
                        ));
                    }
                    Message::Pong => {
                        debug!("pong message received");
                        app_tx
                            .send(FromSync::Data {
                                header: "PONG".as_bytes().to_owned(),
                                payload: None,
                            })
                            .await
                            .unwrap();
                        break;
                    }
                }
            }

            // Flush all bytes so that no messages are lost.
            sink.flush().await?;
            app_tx.flush().await?;

            Ok(())
        }

        async fn accept(
            self: Arc<Self>,
            tx: Box<&'a mut (dyn AsyncWrite + Send + Unpin)>,
            rx: Box<&'a mut (dyn AsyncRead + Send + Unpin)>,
            mut app_tx: Box<
                &'a mut (dyn Sink<FromSync<TestTopic>, Error = SyncError> + Send + Unpin),
            >,
        ) -> Result<(), SyncError> {
            debug!("accept sync session");
            let mut sink = into_cbor_sink(tx);
            let mut stream = into_cbor_stream(rx);

            while let Some(result) = stream.next().await {
                let message = result?;

                match message {
                    Message::Topic(topic) => app_tx.send(FromSync::HandshakeSuccess(topic)).await?,
                    Message::Ping => {
                        debug!("ping message received");
                        app_tx
                            .send(FromSync::Data {
                                header: "PING".as_bytes().to_owned(),
                                payload: None,
                            })
                            .await
                            .unwrap();

                        sink.send(Message::Pong).await?;
                        debug!("pong message sent");
                        break;
                    }
                    Message::Pong => {
                        return Err(SyncError::UnexpectedBehaviour(
                            "unexpected Pong message received".to_string(),
                        ));
                    }
                }
            }

            sink.flush().await?;
            app_tx.flush().await?;

            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use async_trait::async_trait;
    use iroh_net::relay::{RelayNode, RelayUrl as IrohRelayUrl};
    use p2panda_core::{Body, Hash, Header, PrivateKey, PublicKey};
    use p2panda_store::{MemoryStore, OperationStore};
    use p2panda_sync::log_sync::LogSyncProtocol;
    use p2panda_sync::{Topic, TopicMap};
    use serde::{Deserialize, Serialize};
    use tokio::task::JoinHandle;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    use crate::addrs::DEFAULT_STUN_PORT;
    use crate::bytes::ToBytes;
    use crate::config::Config;
    use crate::network::sync_protocols::PingPongProtocol;
    use crate::sync::SyncConfiguration;
    use crate::{NetworkBuilder, RelayMode, RelayUrl, TopicId};

    use super::{FromNetwork, Network, ToNetwork};

    fn setup_logging() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(EnvFilter::from_default_env())
            .try_init()
            .ok();
    }

    fn create_operation<E: Clone + Serialize>(
        private_key: &PrivateKey,
        body: &Body,
        seq_num: u64,
        timestamp: u64,
        backlink: Option<Hash>,
        extensions: Option<E>,
    ) -> (Hash, Header<E>, Vec<u8>) {
        let mut header = Header {
            version: 1,
            public_key: private_key.public_key(),
            signature: None,
            payload_size: body.size(),
            payload_hash: Some(body.hash()),
            timestamp,
            seq_num,
            backlink,
            previous: vec![],
            extensions,
        };
        header.sign(private_key);
        let header_bytes = header.to_bytes();
        (header.hash(), header, header_bytes)
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
    pub struct TestTopic(String, [u8; 32]);

    impl TestTopic {
        pub fn new(name: &str) -> Self {
            Self(name.to_owned(), [0; 32])
        }
    }

    impl Topic for TestTopic {}

    impl TopicId for TestTopic {
        fn id(&self) -> [u8; 32] {
            self.1
        }
    }

    #[tokio::test]
    async fn config() {
        let direct_node_public_key = PrivateKey::new().public_key();
        let relay_address: RelayUrl = "https://example.net".parse().unwrap();

        let config = Config {
            bind_port: 2024,
            network_id: [1; 32],
            private_key: Some(PathBuf::new().join("secret-key.txt")),
            direct_node_addresses: vec![(
                direct_node_public_key,
                vec!["0.0.0.0:2026".parse().unwrap()],
                None,
            )],
            relay: Some(relay_address.clone()),
        };

        let builder = NetworkBuilder::<TestTopic>::from_config(config);

        assert_eq!(builder.bind_port, Some(2024));
        assert_eq!(builder.network_id, [1; 32]);
        assert!(builder.secret_key.is_none());
        assert_eq!(builder.direct_node_addresses.len(), 1);
        let relay_node = RelayNode {
            url: IrohRelayUrl::from(relay_address),
            stun_only: false,
            stun_port: DEFAULT_STUN_PORT,
        };
        assert_eq!(builder.relay_mode, RelayMode::Custom(relay_node));
    }

    #[tokio::test]
    async fn join_gossip_overlay() {
        setup_logging();

        let network_id = [1; 32];
        let topic = TestTopic::new("chat");

        let node_1 = NetworkBuilder::new(network_id).build().await.unwrap();
        let node_2 = NetworkBuilder::new(network_id).build().await.unwrap();

        let node_1_addr = node_1.endpoint().node_addr().await.unwrap();
        let node_2_addr = node_2.endpoint().node_addr().await.unwrap();

        node_1.add_peer(node_2_addr).await.unwrap();
        node_2.add_peer(node_1_addr).await.unwrap();

        // Subscribe to the same topic from both nodes
        let (tx_1, _rx_1, ready_1) = node_1.subscribe(topic.clone()).await.unwrap();
        let (_tx_2, mut rx_2, ready_2) = node_2.subscribe(topic).await.unwrap();

        // Ensure the gossip-overlay has been joined by both nodes for the given topic
        assert!(ready_2.await.is_ok());
        assert!(ready_1.await.is_ok());

        // Broadcast a message and make sure it's received by the other node
        tx_1.send(ToNetwork::Message {
            bytes: "Hello, Node".to_bytes(),
        })
        .await
        .unwrap();

        let rx_2_msg = rx_2.recv().await.unwrap();
        assert_eq!(
            rx_2_msg,
            FromNetwork::GossipMessage {
                bytes: "Hello, Node".to_bytes(),
                delivered_from: node_1.node_id(),
            }
        );

        node_1.shutdown().await.unwrap();
        node_2.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ping_pong() {
        setup_logging();

        let network_id = [1; 32];
        let topic = TestTopic::new("ping_pong");
        let ping_pong = PingPongProtocol {};
        let sync_config = SyncConfiguration::new(ping_pong);

        let node_1 = NetworkBuilder::new(network_id)
            .sync(sync_config.clone())
            .build()
            .await
            .unwrap();
        let node_2 = NetworkBuilder::new(network_id)
            .sync(sync_config)
            .build()
            .await
            .unwrap();

        let node_1_addr = node_1.endpoint().node_addr().await.unwrap();
        let node_2_addr = node_2.endpoint().node_addr().await.unwrap();

        node_1.add_peer(node_2_addr).await.unwrap();
        node_2.add_peer(node_1_addr).await.unwrap();

        // Subscribe to the same topic from both nodes which should kick off sync
        let topic_clone = topic.clone();
        let handle1 = tokio::spawn(async move {
            let (_tx, _rx, _ready) = node_1.subscribe(topic_clone).await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            node_1.shutdown().await.unwrap();
        });
        let handle2 = tokio::spawn(async move {
            let (_tx, _rx, _ready) = node_2.subscribe(topic).await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            node_2.shutdown().await.unwrap();
        });

        let (result1, result2) = tokio::join!(handle1, handle2);
        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    type Logs<T> = HashMap<PublicKey, Vec<T>>;

    #[derive(Clone, Debug)]
    struct LogIdTopicMap<T>(HashMap<T, Logs<u64>>);

    impl<T> LogIdTopicMap<T>
    where
        T: Topic,
    {
        pub fn new() -> Self {
            LogIdTopicMap(HashMap::new())
        }

        fn insert(&mut self, topic: T, logs: Logs<u64>) -> Option<Logs<u64>> {
            self.0.insert(topic, logs)
        }
    }

    #[async_trait]
    impl<T> TopicMap<T, Logs<u64>> for LogIdTopicMap<T>
    where
        T: Topic,
    {
        async fn get(&self, topic: &T) -> Option<Logs<u64>> {
            self.0.get(topic).cloned()
        }
    }

    #[tokio::test]
    async fn e2e_log_height_sync() {
        setup_logging();

        const NETWORK_ID: [u8; 32] = [1; 32];

        let peer_a_private_key = PrivateKey::new();
        let peer_b_private_key = PrivateKey::new();

        let topic = TestTopic::new("event_logs");
        let log_id = 0;
        let logs = HashMap::from([(peer_a_private_key.public_key(), vec![log_id])]);

        let mut topic_map = LogIdTopicMap::new();
        topic_map.insert(topic.clone(), logs);

        // Construct a store and log height protocol for peer a.
        let store_a = MemoryStore::default();
        let protocol_a = LogSyncProtocol::new(topic_map.clone(), store_a);
        let sync_config_a = SyncConfiguration::new(protocol_a);

        // Create some operations.
        let body = Body::new("Hello, Sloth!".as_bytes());
        let (hash_0, header_0, header_bytes_0) =
            create_operation(&peer_a_private_key, &body, 0, 0, None, None);
        let (hash_1, header_1, header_bytes_1) =
            create_operation(&peer_a_private_key, &body, 1, 100, Some(hash_0), None);
        let (hash_2, header_2, header_bytes_2) =
            create_operation(&peer_a_private_key, &body, 2, 200, Some(hash_1), None);

        // Create store for peer b and populate with operations.
        let mut store_b = MemoryStore::default();
        store_b
            .insert_operation(hash_0, &header_0, Some(&body), &header_bytes_0, &log_id)
            .await
            .unwrap();
        store_b
            .insert_operation(hash_1, &header_1, Some(&body), &header_bytes_1, &log_id)
            .await
            .unwrap();
        store_b
            .insert_operation(hash_2, &header_2, Some(&body), &header_bytes_2, &log_id)
            .await
            .unwrap();

        // Construct log height protocol for peer b.
        let protocol_b = LogSyncProtocol::new(topic_map, store_b);
        let sync_config_b = SyncConfiguration::new(protocol_b);

        // Build peer a's node
        let node_a = NetworkBuilder::new(NETWORK_ID)
            .sync(sync_config_a)
            .private_key(peer_a_private_key)
            .build()
            .await
            .unwrap();

        // Build peer b's node
        let node_b = NetworkBuilder::new(NETWORK_ID)
            .sync(sync_config_b)
            .private_key(peer_b_private_key.clone())
            .build()
            .await
            .unwrap();

        let node_a_addr = node_a.endpoint().node_addr().await.unwrap();
        let node_b_addr = node_b.endpoint().node_addr().await.unwrap();

        node_a.add_peer(node_b_addr).await.unwrap();
        node_b.add_peer(node_a_addr).await.unwrap();

        // Subscribe to the same topic from both nodes which should kick off sync.
        let topic_clone = topic.clone();
        let handle1 = tokio::spawn(async move {
            let (_tx, mut from_sync_rx, ready) = node_a.subscribe(topic_clone).await.unwrap();

            // Wait until the gossip overlay has been joined for TOPIC_ID.
            assert!(ready.await.is_ok());

            let mut from_sync_messages = Vec::new();
            while let Some(message) = from_sync_rx.recv().await {
                from_sync_messages.push(message);
                if from_sync_messages.len() == 3 {
                    break;
                }
            }

            // Construct the messages we expect to receive on the from_sync channel based on the
            // operations we created earlier.
            let peer_a_expected_messages = vec![
                FromNetwork::SyncMessage {
                    header: header_bytes_0.to_vec(),
                    payload: Some(body.to_bytes()),
                    delivered_from: peer_b_private_key.public_key(),
                },
                FromNetwork::SyncMessage {
                    header: header_bytes_1.to_vec(),
                    payload: Some(body.to_bytes()),
                    delivered_from: peer_b_private_key.public_key(),
                },
                FromNetwork::SyncMessage {
                    header: header_bytes_2.to_vec(),
                    payload: Some(body.to_bytes()),
                    delivered_from: peer_b_private_key.public_key(),
                },
            ];

            // Assert we receive the expected messages
            assert_eq!(from_sync_messages, peer_a_expected_messages);

            node_a.shutdown().await.unwrap();
        });

        let handle2 = tokio::spawn(async move {
            let (_tx, _from_sync_rx, ready) = node_b.subscribe(topic).await.unwrap();

            // Wait until the gossip overlay has been joined for TOPIC_ID
            assert!(ready.await.is_ok());

            // Sleep for a moment to ensure sync has time to complete
            tokio::time::sleep(Duration::from_secs(2)).await;

            node_b.shutdown().await.unwrap();
        });

        // Wait on both to complete
        let (result1, result2) = tokio::join!(handle1, handle2);

        assert!(result1.is_ok());
        assert!(result2.is_ok())
    }

    #[tokio::test]
    async fn multi_hop_join_gossip_overlay() {
        setup_logging();

        let network_id = [1; 32];
        let chat_topic = TestTopic::new("chat");

        let node_1 = NetworkBuilder::new(network_id).build().await.unwrap();
        let node_2 = NetworkBuilder::new(network_id).build().await.unwrap();
        let node_3 = NetworkBuilder::new(network_id).build().await.unwrap();

        let node_1_addr = node_1.endpoint().node_addr().await.unwrap();
        let node_2_addr = node_2.endpoint().node_addr().await.unwrap();

        node_1.add_peer(node_2_addr.clone()).await.unwrap();
        node_2.add_peer(node_1_addr).await.unwrap();
        node_3.add_peer(node_2_addr).await.unwrap();

        // Subscribe to the same topic from all nodes
        let (tx_1, _rx_1, ready_1) = node_1.subscribe(chat_topic.clone()).await.unwrap();
        let (_tx_2, mut rx_2, ready_2) = node_2.subscribe(chat_topic.clone()).await.unwrap();
        let (_tx_3, mut rx_3, ready_3) = node_3.subscribe(chat_topic).await.unwrap();

        // Ensure the gossip-overlay has been joined by all three nodes for the given topic
        assert!(ready_3.await.is_ok());
        assert!(ready_2.await.is_ok());
        assert!(ready_1.await.is_ok());

        // Broadcast a message and make sure it's received by the other nodes
        tx_1.send(ToNetwork::Message {
            bytes: "Hello, Node".to_bytes(),
        })
        .await
        .unwrap();

        let rx_2_msg = rx_2.recv().await.unwrap();
        assert_eq!(
            rx_2_msg,
            FromNetwork::GossipMessage {
                bytes: "Hello, Node".to_bytes(),
                // Node 2 receives the message and it is delivered by node 1
                delivered_from: node_1.node_id(),
            }
        );

        let rx_3_msg = rx_3.recv().await.unwrap();
        assert_eq!(
            rx_3_msg,
            FromNetwork::GossipMessage {
                bytes: "Hello, Node".to_bytes(),
                // Node 3 receives the message and it is also delivered by node 1
                delivered_from: node_1.node_id(),
            }
        );

        node_1.shutdown().await.unwrap();
        node_2.shutdown().await.unwrap();
        node_3.shutdown().await.unwrap();
    }

    fn run_node<T: TopicId + Topic + 'static>(node: Network<T>, topic: T) -> JoinHandle<()> {
        tokio::spawn(async move {
            let (_tx, mut rx, ready) = node.subscribe(topic).await.unwrap();

            // Await the ready signal so we know the gossip overlay has been joined.
            assert!(ready.await.is_ok());

            // Await at least one message received via sync.
            loop {
                let msg = rx.recv().await.unwrap();
                println!("{msg:?}");
                match msg {
                    FromNetwork::SyncMessage { .. } => break,
                    _ => (),
                }
            }

            // Give other nodes enough time to complete sync sessions.
            tokio::time::sleep(Duration::from_secs(3)).await;
            node.shutdown().await.unwrap();
        })
    }
    #[tokio::test]
    async fn multi_hop_topic_discovery_and_sync() {
        setup_logging();

        let network_id = [1; 32];
        let topic = TestTopic::new("chat");
        let sync_config = SyncConfiguration::new(PingPongProtocol {});

        // Create 4 nodes.
        let node_1 = NetworkBuilder::new(network_id)
            .sync(sync_config.clone())
            .build()
            .await
            .unwrap();
        let node_2 = NetworkBuilder::new(network_id)
            .sync(sync_config.clone())
            .build()
            .await
            .unwrap();
        let node_3 = NetworkBuilder::new(network_id)
            .sync(sync_config.clone())
            .build()
            .await
            .unwrap();
        let node_4 = NetworkBuilder::new(network_id)
            .sync(sync_config.clone())
            .build()
            .await
            .unwrap();

        let node_1_addr = node_1.endpoint().node_addr().await.unwrap();
        let node_2_addr = node_2.endpoint().node_addr().await.unwrap();
        let node_3_addr = node_3.endpoint().node_addr().await.unwrap();

        // All peers know about only one other peer.
        node_1.add_peer(node_2_addr.clone()).await.unwrap();
        node_2.add_peer(node_1_addr).await.unwrap();
        node_3.add_peer(node_2_addr.clone()).await.unwrap();
        node_4.add_peer(node_3_addr.clone()).await.unwrap();

        // Run all nodes. We are testing that peers gracefully handle starting a sync session while
        // not knowing the other peer's address yet. Eventually all peers complete at least one
        // sync session.
        let handle1 = run_node(node_1, topic.clone());
        let handle2 = run_node(node_2, topic.clone());
        let handle3 = run_node(node_3, topic.clone());
        let handle4 = run_node(node_4, topic.clone());

        let (result1, result2, result3, result4) = tokio::join!(handle1, handle2, handle3, handle4);

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!(result3.is_ok());
        assert!(result4.is_ok());
    }
}
