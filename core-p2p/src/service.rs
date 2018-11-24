use cita_handler::{CITAInEvent, CITANodeHandler, CITAOutEvent, IdentificationRequest};
use custom_proto::encode_decode::{Request, Response};
use futures::prelude::*;
use libp2p::core::{
    either,
    multiaddr::Protocol,
    muxing::StreamMuxerBox,
    nodes::raw_swarm::{ConnectedPoint, RawSwarm, RawSwarmEvent},
    nodes::Substream,
    transport::boxed::Boxed,
    upgrade, Endpoint, Multiaddr, PeerId, PublicKey, Transport,
};
use libp2p::{mplex, secio, yamux, TransportTimeout};
use std::collections::HashMap;
use std::io::Error;
use std::time::Duration;
use std::usize;
use tokio::io::{AsyncRead, AsyncWrite};

type P2PRawSwarm = RawSwarm<
    Boxed<(PeerId, StreamMuxerBox)>,
    CITAInEvent,
    CITAOutEvent<Substream<StreamMuxerBox>>,
    CITANodeHandler<Substream<StreamMuxerBox>>,
>;

#[derive(Debug)]
pub enum ServiceEvent {
    /// Closed connection to a node.
    NodeClosed {
        id: PeerId,
    },
    CustomProtocolOpen {
        id: PeerId,
        protocol: usize,
        version: u8,
    },
    CustomProtocolClosed {
        id: PeerId,
        protocol: usize,
    },
    CustomMessage {
        id: PeerId,
        protocol: usize,
        data: Response,
    },
    NodeInfo {
        id: PeerId,
        listen_address: Vec<Multiaddr>,
    },
}

/// Service hook
pub trait ServiceHandle: Sync + Send + Stream<Item = (), Error = ()> {
    /// Send service event to out
    fn out_event(&self, event: Option<ServiceEvent>);
    /// Dialing a new address
    fn new_dialer(&mut self) -> Option<Multiaddr> {
        None
    }
    /// Listening a new address
    fn new_listen(&mut self) -> Option<Multiaddr> {
        None
    }
    /// Disconnect a peer
    fn disconnect(&mut self) -> Option<PeerId> {
        None
    }
    /// Send message to specified node
    fn send_message(&mut self) -> Vec<(Option<PeerId>, usize, Request)> {
        Vec::new()
    }
}

/// Encapsulation of raw_swarm, providing external interfaces
pub struct Service<Handle> {
    swarm: P2PRawSwarm,
    local_public_key: PublicKey,
    local_peer_id: PeerId,
    listening_address: Vec<Multiaddr>,
    /// Connected node information
    connected_nodes: HashMap<PeerId, NodeInfo>,
    need_connect: Vec<Multiaddr>,
    service_handle: Handle,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    endpoint: Endpoint,
    address: Multiaddr,
}

impl<Handle> Service<Handle>
where
    Handle: ServiceHandle,
{
    /// Send message to specified node
    pub fn send_custom_message(&mut self, node: &PeerId, protocol: usize, data: Request) {
        if let Some(mut connected) = self.swarm.peer(node.clone()).as_connected() {
            connected.send_event(CITAInEvent::SendCustomMessage { protocol, data });
        } else {
            debug!("Try to send message to {:?}, but not connected", node);
        }
    }

    /// Send message to all node
    pub fn broadcast(&mut self, protocol: usize, data: Request) {
        self.swarm
            .broadcast_event(&CITAInEvent::SendCustomMessage { protocol, data });
    }

    /// Start listening on a multiaddr.
    #[inline]
    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<Multiaddr, Multiaddr> {
        match self.swarm.listen_on(addr) {
            Ok(mut addr) => {
                addr.append(Protocol::P2p(self.local_peer_id.clone().into()));
                Ok(addr)
            }
            Err(addr) => Err(addr),
        }
    }

    /// Start dialing an address.
    #[inline]
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), Multiaddr> {
        self.swarm.dial(addr, CITANodeHandler::new())
    }

    /// Disconnect a peer
    #[inline]
    pub fn drop_node(&mut self, id: PeerId) {
        self.connected_nodes.remove(&id);
        if let Some(connected) = self.swarm.peer(id).as_connected() {
            connected.close();
        }
    }

    /// Service handle process
    #[inline]
    fn handle_hook(&mut self) {
        while let Some(address) = self.service_handle.new_dialer() {
            if let Err(address) = self.swarm.dial(address, CITANodeHandler::new()) {
                self.need_connect.push(address);
            }
        }
        while let Some(address) = self.service_handle.new_listen() {
            if let Ok(address) = self.swarm.listen_on(address) {
                self.listening_address.push(address);
            }
        }
        while let Some(id) = self.service_handle.disconnect() {
            self.drop_node(id);
        }
        self.service_handle
            .send_message()
            .into_iter()
            .for_each(|(id, protocol, data)| match id {
                Some(id) => self.send_custom_message(&id, protocol, data),
                None => self.broadcast(protocol, data),
            });
    }

    /// Identify respond
    fn respond_to_identify_request<Substream>(
        &mut self,
        requester: &PeerId,
        responder: IdentificationRequest<Substream>,
    ) where
        Substream: AsyncRead + AsyncWrite + Send + 'static,
    {
        if let Some(info) = self.connected_nodes.get(&requester) {
            responder.respond(
                self.local_public_key.clone(),
                self.listening_address.clone(),
                &info.address,
            )
        }
    }

    fn add_observed_addr(&mut self, address: &Multiaddr) {
        for mut addr in self.swarm.nat_traversal(&address) {
            // Ignore addresses we already know about.
            if self.listening_address.iter().any(|a| a == &addr) {
                continue;
            }

            self.listening_address.push(addr.clone());
            addr.append(Protocol::P2p(self.local_peer_id.clone().into()));
        }
    }

    /// All Custom Event to throw process
    fn event_handle<Substream>(
        &mut self,
        peer_id: PeerId,
        event: CITAOutEvent<Substream>,
    ) -> Option<ServiceEvent>
    where
        Substream: AsyncRead + AsyncWrite + Send + 'static,
    {
        match event {
            CITAOutEvent::CustomProtocolOpen { protocol, version } => {
                Some(ServiceEvent::CustomProtocolOpen {
                    id: peer_id,
                    protocol,
                    version,
                })
            }
            CITAOutEvent::CustomMessage { protocol, data } => Some(ServiceEvent::CustomMessage {
                id: peer_id,
                protocol,
                data,
            }),
            CITAOutEvent::CustomProtocolClosed { protocol, .. } => {
                Some(ServiceEvent::CustomProtocolClosed {
                    id: peer_id,
                    protocol,
                })
            }
            CITAOutEvent::Useless => {
                self.connected_nodes.remove(&peer_id);
                Some(ServiceEvent::NodeClosed { id: peer_id })
            }
            CITAOutEvent::PingStart => {
                debug!("ping start");
                None
            }
            CITAOutEvent::PingSuccess(time) => {
                debug!("ping success on {:?}", time);
                None
            }
            CITAOutEvent::IdentificationRequest(request) => {
                self.respond_to_identify_request(&peer_id, request);
                None
            }
            CITAOutEvent::Identified {
                info,
                observed_addr,
            } => {
                self.add_observed_addr(&observed_addr);
                Some(ServiceEvent::NodeInfo {
                    id: peer_id,
                    listen_address: info.listen_addrs,
                })
            }
        }
    }

    /// Poll raw swarm, throw corresponding event
    fn poll_swarm(&mut self) -> Poll<Option<ServiceEvent>, Error> {
        loop {
            let (id, event) = match self.swarm.poll() {
                Async::Ready(event) => match event {
                    RawSwarmEvent::Connected { peer_id, endpoint } => {
                        let (address, endpoint) = match endpoint {
                            ConnectedPoint::Dialer { address } => (address, Endpoint::Dialer),
                            ConnectedPoint::Listener { send_back_addr, .. } => {
                                (send_back_addr, Endpoint::Listener)
                            }
                        };
                        self.connected_nodes
                            .insert(peer_id, NodeInfo { address, endpoint });
                        continue;
                    }
                    RawSwarmEvent::NodeEvent { peer_id, event } => (peer_id, event),
                    RawSwarmEvent::NodeClosed { peer_id, .. } => {
                        self.connected_nodes.remove(&peer_id);
                        return Ok(Async::Ready(Some(ServiceEvent::NodeClosed { id: peer_id })));
                    }
                    RawSwarmEvent::NodeError { peer_id, error, .. } => {
                        error!("node error: {:?}", error);
                        self.connected_nodes.remove(&peer_id);
                        return Ok(Async::Ready(Some(ServiceEvent::NodeClosed { id: peer_id })));
                    }
                    RawSwarmEvent::DialError {
                        multiaddr, error, ..
                    }
                    | RawSwarmEvent::UnknownPeerDialError {
                        multiaddr, error, ..
                    } => {
                        self.need_connect.push(multiaddr);
                        error!("dial error: {:?}", error);
                        continue;
                    }
                    RawSwarmEvent::IncomingConnection(incoming) => {
                        incoming.accept(CITANodeHandler::new());
                        continue;
                    }
                    RawSwarmEvent::IncomingConnectionError {
                        send_back_addr,
                        error,
                        ..
                    } => {
                        error!("node {} incoming error: {:?}", send_back_addr, error);
                        continue;
                    }
                    RawSwarmEvent::Replaced { peer_id, .. } => {
                        if let Some(info) = self.connected_nodes.remove(&peer_id) {
                            match info.endpoint {
                                Endpoint::Listener => {}
                                Endpoint::Dialer => self.need_connect.push(info.address),
                            }
                        }
                        continue;
                    }
                    RawSwarmEvent::ListenerClosed {
                        listen_addr,
                        result,
                        ..
                    } => {
                        error!("listener {} closed, result: {:?}", listen_addr, result);
                        continue;
                    }
                },
                Async::NotReady => return Ok(Async::NotReady),
            };

            if let Some(event) = self.event_handle(id, event) {
                return Ok(Async::Ready(Some(event)));
            }
        }
    }
}

impl<Handle> Stream for Service<Handle>
where
    Handle: ServiceHandle,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<()>, Self::Error> {
        match self.poll_swarm()? {
            Async::Ready(value) => {
                self.service_handle.out_event(value);
                return Ok(Async::Ready(Some(())));
            }
            Async::NotReady => (),
        }

        let _ = self.service_handle.poll();
        self.handle_hook();

        Ok(Async::NotReady)
    }
}

/// Create a new service
pub fn build_service<Handle: ServiceHandle>(
    local_private_key: secio::SecioKeyPair,
    service_handle: Handle,
) -> Service<Handle> {
    let local_public_key = local_private_key.clone().to_public_key();
    let local_peer_id = local_public_key.clone().into_peer_id();
    let swarm = build_swarm(local_private_key);
    Service {
        swarm,
        local_public_key,
        local_peer_id,
        listening_address: Vec::new(),
        connected_nodes: HashMap::new(),
        need_connect: Vec::new(),
        service_handle,
    }
}

fn build_swarm(local_private_key: secio::SecioKeyPair) -> P2PRawSwarm {
    let transport = build_transport(local_private_key);

    RawSwarm::new(transport)
}

fn build_transport(local_private_key: secio::SecioKeyPair) -> Boxed<(PeerId, StreamMuxerBox)> {
    let mut mplex_config = mplex::MplexConfig::new();
    mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    mplex_config.max_buffer_len(usize::MAX);

    let base = libp2p::CommonTransport::new()
        .with_upgrade(secio::SecioConfig::new(local_private_key))
        .and_then(move |out, endpoint| {
            let upgrade = upgrade::or(
                upgrade::map(yamux::Config::default(), either::EitherOutput::First),
                upgrade::map(mplex_config, either::EitherOutput::Second),
            );
            let peer_id = out.remote_key.into_peer_id();
            let upgrade = upgrade::map(upgrade, move |muxer| (peer_id, muxer));
            upgrade::apply(out.stream, upgrade, endpoint.into())
        }).map(|(id, muxer), _| (id, StreamMuxerBox::new(muxer)));

    TransportTimeout::new(base, Duration::from_secs(20)).boxed()
}
