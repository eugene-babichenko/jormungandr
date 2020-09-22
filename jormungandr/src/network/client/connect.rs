use super::super::{
    grpc,
    p2p::{comm::PeerComms, Address},
    Channels, ConnectionState,
};
use super::{Client, ClientBuilder, InboundSubscriptions};
use crate::blockcfg::HeaderHash;
use chain_core::mempack::{self, ReadBuf, Readable};
use chain_network::error::{self as net_error, HandshakeError};
use chain_network::grpc::legacy;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::ready;

use std::convert::TryInto;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Initiates a client connection, returning a connection handle and
/// the connection future that must be polled to complete the connection.
///
/// Note that this is the only function in this module that is tied to the
/// gRPC protocol, all other code is generic in terms of network-core traits.
/// This is intentional, to facilitate extension to different protocols
/// in the future.
pub fn connect(state: ConnectionState, channels: Channels) -> (ConnectHandle, ConnectFuture) {
    let (sender, receiver) = oneshot::channel();
    let peer = state.peer();
    let legacy_node_id = state.global.config.legacy_node_id;
    let logger = state.logger.clone();
    let cf = async move {
        let mut grpc_client = if let Some(node_id) = legacy_node_id {
            let node_id: legacy::NodeId = node_id.as_ref().try_into().unwrap();
            debug!(
                logger,
                "connecting with legacy node id {}",
                hex::encode(node_id.as_bytes())
            );
            grpc::connect_legacy(&peer, node_id).await
        } else {
            debug!(logger, "connecting");
            grpc::connect(&peer).await
        }
        .map_err(ConnectError::Transport)?;
        let block0 = grpc_client
            .handshake()
            .await
            .map_err(ConnectError::Handshake)?;
        let mut buf = ReadBuf::from(block0.as_bytes());
        let block0_hash = HeaderHash::read(&mut buf).map_err(ConnectError::DecodeBlock0)?;
        let expected = state.global.block0_hash;
        match_block0(expected, block0_hash)?;
        let mut comms = PeerComms::new();
        let (block_sub, fragment_sub, gossip_sub) = future::try_join3(
            grpc_client
                .clone()
                .block_subscription(comms.subscribe_to_block_announcements()),
            grpc_client
                .clone()
                .fragment_subscription(comms.subscribe_to_fragments()),
            grpc_client
                .clone()
                .gossip_subscription(comms.subscribe_to_gossip()),
        )
        .await
        .map_err(ConnectError::Subscription)?;
        let inbound = InboundSubscriptions {
            peer_address: Address::new(peer.connection).unwrap(),
            block_events: block_sub,
            fragments: fragment_sub,
            gossip: gossip_sub,
        };
        let builder = ClientBuilder { channels, logger };
        let client = Client::new(
            grpc_client,
            builder,
            state.global.clone(),
            inbound,
            &mut comms,
        );
        Ok((client, comms))
    };
    let handle = ConnectHandle { receiver };
    let future = ConnectFuture {
        sender: Some(sender),
        task: cf.boxed(),
    };
    (handle, future)
}

/// Handle used to monitor the P2P client in process of
/// establishing a connection and subscription streams.
///
/// If the handle is dropped before the connection is established,
/// the client connection is canceled.
pub struct ConnectHandle {
    receiver: oneshot::Receiver<PeerComms>,
}

/// An error type to signal that the connection was not established.
/// The reason should be logged already, so this error type should not be
/// used for reporting.
pub type ConnectCanceled = oneshot::Canceled;

impl ConnectHandle {
    /// Checks if the client has connected and established subscriptions,
    /// and if so, returns the communication handles.
    ///
    /// This method does not use a task context and does not schedule a wakeup.
    pub fn try_complete(&mut self) -> Result<Option<PeerComms>, ConnectCanceled> {
        self.receiver.try_recv()
    }
}

/// The future that drives P2P client to establish a connection.
#[must_use = "futures do nothing unless polled"]
pub struct ConnectFuture {
    sender: Option<oneshot::Sender<PeerComms>>,
    task: BoxFuture<'static, Result<(Client, PeerComms), ConnectError>>,
}

#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    #[error("connection has been canceled")]
    Canceled,
    #[error(transparent)]
    Transport(tonic::transport::Error),
    #[error("protocol handshake failed: {0}")]
    Handshake(#[source] HandshakeError),
    #[error("failed to decode genesis block in response")]
    DecodeBlock0(#[source] mempack::ReadError),
    #[error(
        "genesis block hash {peer_responded} reported by the peer is not the expected {expected}"
    )]
    Block0Mismatch {
        expected: HeaderHash,
        peer_responded: HeaderHash,
    },
    #[error("subscription request failed")]
    Subscription(#[source] net_error::Error),
}

impl Future for ConnectFuture {
    type Output = Result<Client, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // First, check if the connection is cancelled
        if let Poll::Ready(()) = self
            .sender
            .as_mut()
            .expect("polled a future after it has been resolved")
            .poll_canceled(cx)
        {
            return Err(ConnectError::Canceled).into();
        }

        let (client, comms) = ready!(Pin::new(&mut self.task).poll(cx))?;

        match self.sender.take().unwrap().send(comms) {
            Ok(()) => Ok(client).into(),
            Err(_) => Err(ConnectError::Canceled).into(),
        }
    }
}

fn match_block0(expected: HeaderHash, peer_responded: HeaderHash) -> Result<(), ConnectError> {
    if expected == peer_responded {
        Ok(())
    } else {
        Err(ConnectError::Block0Mismatch {
            expected,
            peer_responded,
        })
    }
}
