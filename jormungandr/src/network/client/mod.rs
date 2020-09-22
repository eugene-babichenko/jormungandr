mod connect;

use super::{
    buffer_sizes,
    convert::{Decode, Encode},
    grpc::{
        self,
        client::{BlockSubscription, FragmentSubscription, GossipSubscription},
    },
    p2p::{
        comm::{OutboundSubscription, PeerComms},
        Address,
    },
    subscription::{BlockAnnouncementProcessor, FragmentProcessor, GossipProcessor},
    Channels, GlobalStateR,
};
use crate::{
    intercom::{self, BlockMsg, ClientMsg},
    utils::async_msg::MessageBox,
};
use chain_network::data as net_data;
use chain_network::data::block::{BlockEvent, BlockIds, ChainPullRequest};

use futures::prelude::*;
use futures::ready;
use slog::Logger;

use std::pin::Pin;
use std::task::{Context, Poll};

pub use self::connect::{connect, ConnectError, ConnectFuture, ConnectHandle};

#[must_use = "Client must be polled"]
pub struct Client {
    inner: grpc::Client,
    logger: Logger,
    global_state: GlobalStateR,
    inbound: InboundSubscriptions,
    block_solicitations: OutboundSubscription<BlockIds>,
    chain_pulls: OutboundSubscription<ChainPullRequest>,
    block_sink: BlockAnnouncementProcessor,
    fragment_sink: FragmentProcessor,
    gossip_sink: GossipProcessor,
    client_box: MessageBox<ClientMsg>,
    incoming_block_announcement: Option<net_data::Header>,
    incoming_solicitation: Option<ClientMsg>,
    shutting_down: bool,
}

struct ClientBuilder {
    pub logger: Logger,
    pub channels: Channels,
}

impl Client {
    pub fn logger(&self) -> &Logger {
        &self.logger
    }
}

impl Client {
    fn new(
        inner: grpc::Client,
        builder: ClientBuilder,
        global_state: GlobalStateR,
        inbound: InboundSubscriptions,
        comms: &mut PeerComms,
    ) -> Self {
        let logger = builder
            .logger
            .new(o!("peer_address" => inbound.peer_address.to_string()));

        let block_sink = BlockAnnouncementProcessor::new(
            builder.channels.block_box,
            inbound.peer_address.clone(),
            global_state.clone(),
            logger.new(o!("stream" => "block_events", "direction" => "in")),
        );
        let fragment_sink = FragmentProcessor::new(
            builder.channels.transaction_box,
            inbound.peer_address.clone(),
            global_state.clone(),
            logger.new(o!("stream" => "fragments", "direction" => "in")),
        );
        let gossip_sink = GossipProcessor::new(
            inbound.peer_address.clone(),
            global_state.clone(),
            logger.new(o!("stream" => "gossip", "direction" => "in")),
        );

        Client {
            inner,
            logger,
            global_state,
            inbound,
            block_solicitations: comms.subscribe_to_block_solicitations(),
            chain_pulls: comms.subscribe_to_chain_pulls(),
            block_sink,
            fragment_sink,
            gossip_sink,
            client_box: builder.channels.client_box,
            incoming_block_announcement: None,
            incoming_solicitation: None,
            shutting_down: false,
        }
    }
}

struct InboundSubscriptions {
    pub peer_address: Address,
    pub block_events: BlockSubscription,
    pub fragments: FragmentSubscription,
    pub gossip: GossipSubscription,
}

#[derive(Copy, Clone)]
enum ProcessingOutcome {
    Continue,
    Disconnect,
}

struct Progress(pub Poll<ProcessingOutcome>);

impl Progress {
    fn begin(async_outcome: Poll<Result<ProcessingOutcome, ()>>) -> Self {
        use self::ProcessingOutcome::*;

        Progress(async_outcome.map(|res| res.unwrap_or(Disconnect)))
    }

    fn and_proceed_with<F>(&mut self, poll_fn: F)
    where
        F: FnOnce() -> Poll<Result<ProcessingOutcome, ()>>,
    {
        use self::ProcessingOutcome::*;
        use Poll::*;

        let async_outcome = match self.0 {
            Pending | Ready(Continue) => poll_fn(),
            Ready(Disconnect) => return,
        };

        if let Ready(outcome) = async_outcome {
            match outcome {
                Ok(outcome) => {
                    self.0 = Ready(outcome);
                }
                Err(()) => {
                    self.0 = Ready(Disconnect);
                }
            }
        }
    }
}

impl Client {
    fn process_block_event(&mut self, cx: &mut Context<'_>) -> Poll<Result<ProcessingOutcome, ()>> {
        use self::ProcessingOutcome::*;
        // Drive sending of a message to block task to clear the buffered
        // announcement before polling more events from the block subscription
        // stream.
        let logger = self.logger().clone();
        let mut block_sink = Pin::new(&mut self.block_sink);
        ready!(block_sink.as_mut().poll_ready(cx))
            .map_err(|e| debug!(logger, "failed getting block sink"; "reason" => %e))?;
        if let Some(header) = self.incoming_block_announcement.take() {
            block_sink.start_send(header).map_err(|_| ())?;
        } else {
            match block_sink.as_mut().poll_flush(cx) {
                Poll::Pending => {
                    // Ignoring possible Pending return here: due to the following
                    // ready!() invocations, this function cannot return Continue
                    // while no progress has been made.
                    Ok(())
                }
                Poll::Ready(Ok(())) => Ok(()),
                Poll::Ready(Err(_)) => Err(()),
            }?;
        }

        // Drive sending of a message to the client request task to clear
        // the buffered solicitation before polling more events from the
        // block subscription stream.
        let mut client_box = Pin::new(&mut self.client_box);
        let logger = &self.logger;
        ready!(client_box.as_mut().poll_ready(cx)).map_err(|e| {
            error!(
                logger,
                "processing of incoming client requests failed";
                "reason" => %e,
            );
        })?;
        if let Some(msg) = self.incoming_solicitation.take() {
            client_box.start_send(msg).map_err(|e| {
                error!(
                    self.logger,
                    "failed to send client request for processing";
                    "reason" => %e,
                );
            })?;
        } else {
            match client_box.as_mut().poll_flush(cx) {
                Poll::Pending => {
                    // Ignoring possible Pending return here: due to the following
                    // ready!() invocation, this function cannot return Continue
                    // while no progress has been made.
                    Ok(())
                }
                Poll::Ready(Ok(())) => Ok(()),
                Poll::Ready(Err(e)) => {
                    error!(
                        self.logger,
                        "processing of incoming client requests failed";
                        "reason" => %e,
                    );
                    Err(())
                }
            }?;
        }

        let block_events = Pin::new(&mut self.inbound.block_events);
        let maybe_event = ready!(block_events.poll_next(cx));
        let event = match maybe_event {
            Some(Ok(event)) => event,
            None => {
                debug!(self.logger, "block event subscription ended by the peer");
                return Ok(Disconnect).into();
            }
            Some(Err(e)) => {
                debug!(
                    self.logger,
                    "block subscription stream failure";
                    "error" => ?e,
                );
                return Err(()).into();
            }
        };
        match event {
            BlockEvent::Announce(header) => {
                debug_assert!(self.incoming_block_announcement.is_none());
                self.incoming_block_announcement = Some(header);
            }
            BlockEvent::Solicit(block_ids) => {
                self.upload_blocks(block_ids)?;
            }
            BlockEvent::Missing(req) => {
                self.push_missing_headers(req)?;
            }
        }
        Ok(Continue).into()
    }

    fn upload_blocks(&mut self, block_ids: BlockIds) -> Result<(), ()> {
        let logger = self.logger.new(o!("solicitation" => "UploadBlocks"));
        debug!(logger, "peer requests {} blocks", block_ids.len());
        let block_ids = block_ids.decode().map_err(|e| {
            info!(
                logger,
                "failed to decode block IDs from solicitation request";
                "reason" => %e,
            );
        })?;
        let (reply_handle, future) =
            intercom::stream_reply(buffer_sizes::outbound::BLOCKS, logger.clone());
        debug_assert!(self.incoming_solicitation.is_none());
        self.incoming_solicitation = Some(ClientMsg::GetBlocks(block_ids, reply_handle));
        let mut client = self.inner.clone();
        self.global_state.spawn(async move {
            let stream = match future.await {
                Ok(stream) => stream.upload().map(|item| item.encode()),
                Err(e) => {
                    info!(
                        logger,
                        "cannot serve peer's solicitation";
                        "reason" => %e,
                    );
                    return;
                }
            };
            match client.upload_blocks(stream).await {
                Ok(()) => {
                    debug!(logger, "finished uploading blocks");
                }
                Err(e) => {
                    info!(
                        logger,
                        "UploadBlocks request failed";
                        "error" => ?e,
                    );
                }
            }
        });
        Ok(())
    }

    fn push_missing_headers(&mut self, req: ChainPullRequest) -> Result<(), ()> {
        let logger = self.logger.new(o!("solicitation" => "PushHeaders"));
        let from = req.from.decode().map_err(|e| {
            info!(
                logger,
                "failed to decode checkpoint block IDs from header pull request";
                "reason" => %e,
            );
        })?;
        let to = req.to.decode().map_err(|e| {
            info!(
                logger,
                "failed to decode tip block ID from header pull request";
                "reason" => %e,
            );
        })?;
        debug!(
            logger,
            "peer requests missing part of the chain";
            "checkpoints" => ?from,
            "to" => ?to,
        );
        let (reply_handle, future) =
            intercom::stream_reply(buffer_sizes::outbound::HEADERS, logger.clone());
        debug_assert!(self.incoming_solicitation.is_none());
        self.incoming_solicitation = Some(ClientMsg::GetHeadersRange(from, to, reply_handle));
        let mut client = self.inner.clone();
        let logger = self.logger.clone();
        self.global_state.spawn(async move {
            let stream = match future.await {
                Ok(stream) => stream.upload().map(|item| item.encode()),
                Err(e) => {
                    info!(
                        logger,
                        "cannot serve peer's solicitation";
                        "reason" => %e,
                    );
                    return;
                }
            };
            match client.push_headers(stream).await {
                Ok(()) => {
                    debug!(logger, "finished pushing headers");
                }
                Err(e) => {
                    info!(
                        logger,
                        "PushHeaders request failed";
                        "error" => ?e,
                    );
                }
            }
        });
        Ok(())
    }

    fn pull_headers(&mut self, req: ChainPullRequest) {
        let mut block_box = self.block_sink.message_box();
        let logger = self.logger.new(o!("request" => "PullHeaders"));
        let logger1 = logger.clone();
        let (handle, sink, _) =
            intercom::stream_request(buffer_sizes::inbound::HEADERS, logger.clone());
        // TODO: make sure that back pressure on the number of requests
        // in flight prevents unlimited spawning of these tasks.
        // https://github.com/input-output-hk/jormungandr/issues/1034
        self.global_state.spawn(async move {
            let res = block_box.send(BlockMsg::ChainHeaders(handle)).await;
            if let Err(e) = res {
                error!(
                    logger,
                    "failed to enqueue request for processing";
                    "reason" => %e,
                );
            }
        });
        let mut client = self.inner.clone();
        self.global_state.spawn(async move {
            match client.pull_headers(req.from, req.to).await {
                Err(e) => {
                    info!(
                        logger1,
                        "request failed";
                        "reason" => %e,
                    );
                }
                Ok(stream) => {
                    let stream = stream.and_then(|item| async { item.decode() });
                    let res = stream.forward(sink.sink_err_into()).await;
                    if let Err(e) = res {
                        info!(
                            logger1,
                            "response stream failed";
                            "reason" => %e,
                        );
                    }
                }
            }
        });
    }

    fn solicit_blocks(&mut self, block_ids: BlockIds) {
        let mut block_box = self.block_sink.message_box();
        let logger = self.logger.new(o!("request" => "GetBlocks"));
        let req_err_logger = logger.clone();
        let res_logger = logger.clone();
        let (handle, sink, _) =
            intercom::stream_request(buffer_sizes::inbound::BLOCKS, logger.clone());
        // TODO: make sure that back pressure on the number of requests
        // in flight prevents unlimited spawning of these tasks.
        // https://github.com/input-output-hk/jormungandr/issues/1034
        self.global_state.spawn(async move {
            let res = block_box.send(BlockMsg::NetworkBlocks(handle)).await;
            if let Err(e) = res {
                error!(
                    logger,
                    "failed to enqueue request for processing";
                    "reason" => %e,
                );
            }
        });
        let mut client = self.inner.clone();
        self.global_state.spawn(async move {
            match client.get_blocks(block_ids).await {
                Err(e) => {
                    info!(
                        req_err_logger,
                        "request failed";
                        "reason" => %e,
                    );
                }
                Ok(stream) => {
                    let stream = stream.and_then(|item| async { item.decode() });
                    let res = stream.forward(sink.sink_err_into()).await;
                    if let Err(e) = res {
                        info!(
                            res_logger,
                            "response stream failed";
                            "reason" => %e,
                        );
                    }
                }
            }
        });
    }

    fn process_fragments(&mut self, cx: &mut Context<'_>) -> Poll<Result<ProcessingOutcome, ()>> {
        use self::ProcessingOutcome::*;

        let mut fragment_sink = Pin::new(&mut self.fragment_sink);
        ready!(fragment_sink.as_mut().poll_ready(cx)).map_err(|_| ())?;

        match Pin::new(&mut self.inbound.fragments).poll_next(cx) {
            Poll::Pending => {
                if let Poll::Ready(Err(_)) = fragment_sink.as_mut().poll_flush(cx) {
                    return Err(()).into();
                }
                Poll::Pending
            }
            Poll::Ready(Some(Ok(fragment))) => {
                fragment_sink
                    .as_mut()
                    .start_send(fragment)
                    .map_err(|_| ())?;
                Ok(Continue).into()
            }
            Poll::Ready(None) => {
                debug!(self.logger, "fragment subscription ended by the peer");
                Ok(Disconnect).into()
            }
            Poll::Ready(Some(Err(e))) => {
                debug!(
                    self.logger,
                    "fragment subscription stream failure";
                    "error" => ?e,
                );
                Err(()).into()
            }
        }
    }

    fn process_gossip(&mut self, cx: &mut Context<'_>) -> Poll<Result<ProcessingOutcome, ()>> {
        use self::ProcessingOutcome::*;

        let mut gossip_sink = Pin::new(&mut self.gossip_sink);
        ready!(gossip_sink.as_mut().poll_ready(cx)).map_err(|_| ())?;

        match Pin::new(&mut self.inbound.gossip).poll_next(cx) {
            Poll::Pending => {
                if let Poll::Ready(Err(_)) = gossip_sink.as_mut().poll_flush(cx) {
                    return Err(()).into();
                }
                Poll::Pending
            }
            Poll::Ready(Some(Ok(gossip))) => {
                gossip_sink.as_mut().start_send(gossip).map_err(|_| ())?;
                Ok(Continue).into()
            }
            Poll::Ready(None) => {
                debug!(self.logger, "gossip subscription ended by the peer");
                Ok(Disconnect).into()
            }
            Poll::Ready(Some(Err(e))) => {
                debug!(
                    self.logger,
                    "gossip subscription stream failure";
                    "error" => ?e,
                );
                Err(()).into()
            }
        }
    }

    fn poll_shut_down(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        ready!(Pin::new(&mut self.block_sink).poll_close(cx)).unwrap_or(());
        ready!(Pin::new(&mut self.fragment_sink).poll_close(cx)).unwrap_or(());
        ready!(Pin::new(&mut self.gossip_sink).poll_close(cx)).unwrap_or(());
        ready!(Pin::new(&mut self.client_box).poll_close(cx)).unwrap_or_else(|e| {
            warn!(
                self.logger,
                "failed to close communication channel to the client task";
                "reason" => %e,
            );
        });
        Poll::Ready(())
    }
}

impl Future for Client {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        use self::ProcessingOutcome::*;

        if self.shutting_down {
            return self.poll_shut_down(cx);
        }

        loop {
            let mut progress = Progress::begin(self.process_block_event(cx));

            progress.and_proceed_with(|| self.process_fragments(cx));
            progress.and_proceed_with(|| self.process_gossip(cx));

            // Block solicitations and chain pulls are special:
            // they are handled with client requests on the client side,
            // but on the server side, they are fed into the block event stream.
            progress.and_proceed_with(|| {
                Pin::new(&mut self.block_solicitations)
                    .poll_next(cx)
                    .map(|maybe_item| match maybe_item {
                        Some(block_ids) => {
                            self.solicit_blocks(block_ids);
                            Ok(Continue)
                        }
                        None => {
                            debug!(self.logger, "outbound block solicitation stream closed");
                            Ok(Disconnect)
                        }
                    })
            });
            progress.and_proceed_with(|| {
                Pin::new(&mut self.chain_pulls)
                    .poll_next(cx)
                    .map(|maybe_item| match maybe_item {
                        Some(req) => {
                            self.pull_headers(req);
                            Ok(Continue)
                        }
                        None => {
                            debug!(self.logger, "outbound header pull stream closed");
                            Ok(Disconnect)
                        }
                    })
            });

            match progress {
                Progress(Poll::Pending) => return Poll::Pending,
                Progress(Poll::Ready(Continue)) => continue,
                Progress(Poll::Ready(Disconnect)) => {
                    info!(self.logger, "disconnecting client");
                    return ().into();
                }
            }
        }
    }
}
