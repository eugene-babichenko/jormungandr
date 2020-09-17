use crate::intercom::NotifierMsg as Message;
use crate::utils::async_msg::{MessageBox, MessageQueue};
use crate::utils::task::TokioServiceInfo;
use chain_impl_mockchain::header::HeaderId;
use futures::{select, SinkExt, StreamExt};
use jormungandr_lib::interfaces::notifier::JsonMessage;
use slog::Logger;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

const MAX_CONNECTIONS_DEFAULT: usize = 255;

// error codes in 4000-4999 are reserved for private use.
// I couldn't find an error code for max connections, so I'll use the first one for now
// maybe using the standard error code for Again is the right thing to do
const MAX_CONNECTIONS_ERROR_CLOSE_CODE: u16 = 4000;
const MAX_CONNECTIONS_ERROR_REASON: &str = "MAX CONNECTIONS reached";

pub struct Notifier {
    connection_counter: Arc<AtomicUsize>,
    max_connections: usize,
    tip_sender: Arc<watch::Sender<SerializedMessage<NewTip>>>,
    tip_receiver: watch::Receiver<SerializedMessage<NewTip>>,
    block_sender: Arc<broadcast::Sender<SerializedMessage<NewBlock>>>,
}

#[derive(Clone)]
pub struct NotifierContext(pub MessageBox<Message>);

impl NotifierContext {
    pub async fn new_connection(&mut self, ws: warp::ws::WebSocket) {
        &mut self.0.send(Message::NewConnection(ws)).await;
    }
}

impl Notifier {
    pub fn new(max_connections: Option<usize>, current_tip: HeaderId) -> Notifier {
        let (tip_sender, tip_receiver) = watch::channel(SerializedMessage::new_tip(current_tip));
        let (block_sender, _block_receiver) = broadcast::channel(16);

        Notifier {
            connection_counter: Arc::new(AtomicUsize::new(0)),
            max_connections: max_connections.unwrap_or(MAX_CONNECTIONS_DEFAULT),
            tip_sender: Arc::new(tip_sender),
            tip_receiver,
            block_sender: Arc::new(block_sender),
        }
    }

    pub async fn start(&self, info: TokioServiceInfo, queue: MessageQueue<Message>) {
        let info = Arc::new(info);

        queue
            .for_each(move |input| {
                let tip_sender = Arc::clone(&self.tip_sender);
                let block_sender = Arc::clone(&self.block_sender);
                let logger = info.logger().clone();

                match input {
                    Message::NewBlock(block_id) => {
                        info.spawn("notifier broadcast block", async move {
                            if let Err(_err) =
                                block_sender.send(SerializedMessage::new_block(block_id))
                            {
                                ()
                            }
                        });
                    }
                    Message::NewTip(block_id) => {
                        info.spawn("notifier broadcast new tip", async move {
                            if let Err(_err) =
                                tip_sender.broadcast(SerializedMessage::new_tip(block_id))
                            {
                                error!(logger, "notifier failed to broadcast tip {}", block_id);
                            }
                        });
                    }
                    Message::NewConnection(ws) => {
                        trace!(logger, "processing notifier new connection");
                        let info2 = Arc::clone(&info);

                        let connection_counter = Arc::clone(&self.connection_counter);
                        let max_connections = self.max_connections;
                        let tip_receiver = self.tip_receiver.clone();

                        info.spawn("notifier process new messages", async move {
                            Self::new_connection(
                                info2,
                                max_connections,
                                connection_counter,
                                tip_receiver,
                                block_sender,
                                ws,
                            )
                            .await;
                        });
                    }
                }

                futures::future::ready(())
            })
            .await;
    }

    async fn new_connection(
        info: Arc<TokioServiceInfo>,
        max_connections: usize,
        connection_counter: Arc<AtomicUsize>,
        tip_receiver: watch::Receiver<SerializedMessage<NewTip>>,
        block_sender: Arc<broadcast::Sender<SerializedMessage<NewBlock>>>,
        mut ws: warp::ws::WebSocket,
    ) {
        let counter = connection_counter.load(Ordering::Acquire);

        if counter < max_connections {
            connection_counter.store(counter + 1, Ordering::Release);

            let mut tip_receiver = tip_receiver.fuse();
            let mut block_receiver = block_sender.subscribe().fuse();

            info.spawn(
                "notifier connection",
                (move || async move {
                    loop {
                        select! {
                            msg = tip_receiver.next() => {
                                if let Some(msg) = msg {
                                    if let Err(_disconnected) = ws.send(msg.into_inner()).await {
                                        break;
                                    }
                                }
                            },
                            msg = block_receiver.next() => {
                                // if this is an Err it means this receiver is lagging, in which case it will
                                // drop messages, I think ignoring that case and continuing with the rest is
                                // fine
                                if let Some(Ok(msg)) = msg {
                                    if let Err(_disconnected) = ws.send(msg.into_inner()).await {
                                        break;
                                    }
                                }
                            },
                            complete => break,
                        };
                    }

                    futures::future::ready(())
                })()
                .await,
            );
        } else {
            let close_msg = warp::ws::Message::close_with(
                MAX_CONNECTIONS_ERROR_CLOSE_CODE,
                MAX_CONNECTIONS_ERROR_REASON,
            );
            if ws.send(close_msg).await.is_ok() {
                let _ = ws.close().await;
            }
        }
    }
}

#[derive(Clone, Debug)]
enum NewTip {}

#[derive(Clone, Debug)]
enum NewBlock {}

#[derive(Debug, Clone)]
struct SerializedMessage<T> {
    msg: warp::ws::Message,
    marker: PhantomData<T>,
}

impl<T> SerializedMessage<T> {
    fn into_inner(self) -> warp::ws::Message {
        self.msg
    }
}

impl SerializedMessage<NewTip> {
    fn new_tip(msg: HeaderId) -> Self {
        Self {
            msg: warp::ws::Message::text(JsonMessage::NewTip(msg.into())),
            marker: PhantomData,
        }
    }
}

impl SerializedMessage<NewBlock> {
    fn new_block(msg: HeaderId) -> Self {
        Self {
            msg: warp::ws::Message::text(JsonMessage::NewBlock(msg.into())),
            marker: PhantomData,
        }
    }
}
