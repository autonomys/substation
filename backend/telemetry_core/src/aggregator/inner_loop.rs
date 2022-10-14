// Source code for the Substrate Telemetry Server.
// Copyright (C) 2021 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use super::aggregator::ConnId;
use crate::feed_message::{self, FeedMessageSerializer, FeedMessageWriter};
use crate::find_location::Locator;
use crate::state::{BatchedState, NodeId};
use bimap::BiMap;
use common::{
    internal_messages::{self, ShardNodeId},
    node_message,
    node_types::BlockHash,
    time, MultiMapUnique,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Incoming messages come via subscriptions, and end up looking like this.
#[derive(Clone, Debug)]
pub enum ToAggregator {
    FromShardWebsocket(ConnId, FromShardWebsocket),
    FromFeedWebsocket(ConnId, FromFeedWebsocket),
    SendUpdates,
    /// Hand back some metrics. The provided sender is expected not to block when
    /// a message is sent into it.
    GatherMetrics(flume::Sender<Metrics>),
}

/// An incoming shard connection can send these messages to the aggregator.
#[derive(Clone, Debug)]
pub enum FromShardWebsocket {
    /// When the socket is opened, it'll send this first
    /// so that we have a way to communicate back to it.
    Initialize {
        channel: flume::Sender<ToShardWebsocket>,
    },
    /// Tell the aggregator about a new node.
    Add {
        local_id: ShardNodeId,
        ip: std::net::IpAddr,
        node: common::node_types::NodeDetails,
        genesis_hash: common::node_types::BlockHash,
    },
    /// Update/pass through details about a node.
    Update {
        local_id: ShardNodeId,
        payload: node_message::Payload,
    },
    /// Tell the aggregator that a node has been removed when it disconnects.
    Remove { local_id: ShardNodeId },
    /// The shard is disconnected.
    Disconnected,
}

/// The aggregator can these messages back to a shard connection.
#[derive(Debug)]
pub enum ToShardWebsocket {
    /// Mute messages to the core by passing the shard-local ID of them.
    Mute {
        local_id: ShardNodeId,
        reason: internal_messages::MuteReason,
    },
}

/// An incoming feed connection can send these messages to the aggregator.
#[derive(Clone, Debug)]
pub enum FromFeedWebsocket {
    /// When the socket is opened, it'll send this first
    /// so that we have a way to communicate back to it.
    /// Unbounded so that slow feeds don't block aggregato
    /// progress.
    Initialize {
        channel: flume::Sender<ToFeedWebsocket>,
    },
    /// The feed can subscribe to a chain to receive
    /// messages relating to it.
    Subscribe { chain: BlockHash },
    /// An explicit ping message.
    Ping { value: Box<str> },
    /// The feed is disconnected.
    Disconnected,
}

/// A set of metrics returned when we ask for metrics
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    /// When in unix MS from epoch were these metrics obtained
    pub timestamp_unix_ms: u64,
    /// How many chains are feeds currently subscribed to.
    pub chains_subscribed_to: usize,
    /// Number of subscribed feeds.
    pub subscribed_feeds: usize,
    /// How many messages are currently queued up in internal channels
    /// waiting to be sent out to feeds.
    pub total_messages_to_feeds: usize,
    /// How many messages are currently queued waiting to be handled by this aggregator.
    pub current_messages_to_aggregator: usize,
    /// The total number of messages sent to the aggregator.
    pub total_messages_to_aggregator: u64,
    /// How many (non-critical) messages have been dropped by the aggregator because it was overwhelmed.
    pub dropped_messages_to_aggregator: u64,
    /// How many nodes are currently known to this aggregator.
    pub connected_nodes: usize,
    /// How many feeds are currently connected to this aggregator.
    pub connected_feeds: usize,
    /// How many shards are currently connected to this aggregator.
    pub connected_shards: usize,
}

// The frontend sends text based commands; parse them into these messages:
impl FromStr for FromFeedWebsocket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (cmd, value) = match s.find(':') {
            Some(idx) => (&s[..idx], &s[idx + 1..]),
            None => return Err(anyhow::anyhow!("Expecting format `CMD:CHAIN_NAME`")),
        };
        match cmd {
            "ping" => Ok(FromFeedWebsocket::Ping {
                value: value.into(),
            }),
            "subscribe" => Ok(FromFeedWebsocket::Subscribe {
                chain: value.parse()?,
            }),
            _ => return Err(anyhow::anyhow!("Command {} not recognised", cmd)),
        }
    }
}

/// The aggregator can these messages back to a feed connection.
#[derive(Clone, Debug)]
pub enum ToFeedWebsocket {
    Bytes(bytes::Bytes),
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy)]
struct Metadata {
    max_node_count: usize,
}

/// Instances of this are responsible for handling incoming and
/// outgoing messages in the main aggregator loop.
pub struct InnerLoop {
    /// The batched state of our chains and nodes lives here:
    node_state: BatchedState,
    /// We maintain a mapping between NodeId and ConnId+LocalId, so that we know
    /// which messages are about which nodes.
    node_ids: BiMap<NodeId, (ConnId, ShardNodeId)>,

    /// Keep track of how to send messages out to feeds.
    feed_channels: HashMap<ConnId, flume::Sender<ToFeedWebsocket>>,
    /// Keep track of how to send messages out to shards.
    shard_channels: HashMap<ConnId, flume::Sender<ToShardWebsocket>>,

    /// Which feeds are subscribed to a given chain?
    chain_to_feed_conn_ids: MultiMapUnique<BlockHash, ConnId>,

    /// Geo locator from ip
    locator: Locator,

    /// How big can the queue of messages coming in to the aggregator get before messages
    /// are prioritised and dropped to try and get back on track.
    max_queue_len: usize,
}

impl InnerLoop {
    /// Create a new inner loop handler with the various state it needs.
    pub fn new(
        denylist: Vec<String>,
        max_queue_len: usize,
        max_third_party_nodes: usize,
        send_node_data: bool,
        metadata_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        Ok(InnerLoop {
            node_state: BatchedState::new(
                denylist,
                max_third_party_nodes,
                send_node_data,
                metadata_path,
            )?,
            node_ids: BiMap::new(),
            feed_channels: HashMap::new(),
            shard_channels: HashMap::new(),
            chain_to_feed_conn_ids: MultiMapUnique::new(),
            locator: Locator::new(Default::default()),
            max_queue_len,
        })
    }

    /// Start handling and responding to incoming messages.
    pub async fn handle<E>(mut self, mut rx_from_external: E)
    where
        E: Stream<Item = ToAggregator> + Unpin,
    {
        let max_queue_len = self.max_queue_len;
        let (metered_tx, metered_rx) = flume::unbounded();

        // Keep count of the number of dropped/total messages for the sake of metric reporting
        let dropped_messages = Arc::new(AtomicU64::new(0));
        let total_messages = Arc::new(AtomicU64::new(0));

        // Actually handle all of our messages, but before we get here, we
        // check the length of the queue below to decide whether or not to
        // pass the message on to this.
        let dropped_messages2 = Arc::clone(&dropped_messages);
        let total_messages2 = Arc::clone(&total_messages);
        tokio::spawn(async move {
            while let Ok(msg) = metered_rx.recv_async().await {
                match msg {
                    ToAggregator::FromFeedWebsocket(feed_conn_id, msg) => {
                        self.handle_from_feed(feed_conn_id, msg)
                    }
                    ToAggregator::FromShardWebsocket(shard_conn_id, msg) => {
                        self.handle_from_shard(shard_conn_id, msg)
                    }
                    ToAggregator::SendUpdates => self.send_updates(),
                    ToAggregator::GatherMetrics(tx) => self.handle_gather_metrics(
                        tx,
                        metered_rx.len(),
                        dropped_messages2.load(Ordering::Relaxed),
                        total_messages2.load(Ordering::Relaxed),
                    ),
                }
            }
        });

        while let Some(msg) = rx_from_external.next().await {
            total_messages.fetch_add(1, Ordering::Relaxed);

            // ignore node updates if we have too many messages to handle, in an attempt
            // to reduce the queue length back to something reasonable, lest it get out of
            // control and start consuming a load of memory.
            if metered_tx.len() > max_queue_len {
                if matches!(
                    msg,
                    ToAggregator::FromShardWebsocket(.., FromShardWebsocket::Update { .. })
                ) {
                    // Note: this wraps on overflow (which is probably the best
                    // behaviour for graphing it anyway)
                    dropped_messages.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }

            if let Err(e) = metered_tx.send(msg) {
                log::error!("Cannot send message into aggregator: {}", e);
                break;
            }
        }
    }

    fn send_updates(&mut self) {
        for (genesis_hash, feeds) in self.node_state.drain_chain_updates().collect::<Vec<_>>() {
            for feed in feeds {
                self.finalize_and_broadcast_to_chain_feeds(&genesis_hash, feed);
            }
        }
        let feed_for_all = self.node_state.drain_updates_for_all_feeds();
        self.finalize_and_broadcast_to_all_feeds(feed_for_all);

        self.node_state.update_added_nodes_messages();
    }

    /// Gather and return some metrics.
    fn handle_gather_metrics(
        &mut self,
        rx: flume::Sender<Metrics>,
        current_messages_to_aggregator: usize,
        dropped_messages_to_aggregator: u64,
        total_messages_to_aggregator: u64,
    ) {
        let timestamp_unix_ms = time::now();
        let connected_nodes = self.node_ids.len();
        let subscribed_feeds = self.chain_to_feed_conn_ids.num_values();
        let chains_subscribed_to = self.chain_to_feed_conn_ids.num_keys();
        let connected_shards = self.shard_channels.len();
        let connected_feeds = self.feed_channels.len();
        let total_messages_to_feeds: usize = self.feed_channels.values().map(|c| c.len()).sum();

        // Ignore error sending; assume the receiver stopped caring and dropped the channel:
        let _ = rx.send(Metrics {
            timestamp_unix_ms,
            chains_subscribed_to,
            subscribed_feeds,
            total_messages_to_feeds,
            current_messages_to_aggregator,
            total_messages_to_aggregator,
            dropped_messages_to_aggregator,
            connected_nodes,
            connected_feeds,
            connected_shards,
        });
    }

    /// Handle messages coming from shards.
    fn handle_from_shard(&mut self, shard_conn_id: ConnId, msg: FromShardWebsocket) {
        match msg {
            FromShardWebsocket::Initialize { channel } => {
                self.shard_channels.insert(shard_conn_id, channel);
            }
            FromShardWebsocket::Add {
                local_id,
                ip,
                node,
                genesis_hash,
            } => match self
                .node_state
                .add_node(genesis_hash, shard_conn_id, local_id, node)
            {
                Err(reason) => {
                    if let Some(shard_conn) = self.shard_channels.get_mut(&shard_conn_id) {
                        let _ = shard_conn.send(ToShardWebsocket::Mute { local_id, reason });
                    }
                }
                Ok(node_id) => {
                    self.node_state
                        .update_node_location(node_id, self.locator.locate(ip));
                }
            },
            FromShardWebsocket::Remove { local_id } => {
                self.node_state.remove_node(shard_conn_id, local_id)
            }
            FromShardWebsocket::Update { local_id, payload } => {
                self.node_state
                    .update_node(shard_conn_id, local_id, payload)
            }

            FromShardWebsocket::Disconnected => self.node_state.disconnect_node(shard_conn_id),
        }
    }

    /// Handle messages coming from feeds.
    fn handle_from_feed(&mut self, feed_conn_id: ConnId, msg: FromFeedWebsocket) {
        match msg {
            FromFeedWebsocket::Initialize { channel } => {
                self.feed_channels.insert(feed_conn_id, channel.clone());

                // Tell the new feed subscription some basic things to get it going:
                let mut feed_serializer = FeedMessageSerializer::new();
                feed_serializer.push(feed_message::Version(32));
                for chain in self.node_state.iter_chains() {
                    feed_serializer.push(feed_message::AddedChain(
                        chain.label(),
                        chain.genesis_hash(),
                        chain.node_count(),
                        chain.highest_node_count(),
                    ));
                }

                // Send this to the channel that subscribed:
                if let Some(bytes) = feed_serializer.into_finalized() {
                    let _ = channel.send(ToFeedWebsocket::Bytes(bytes));
                }
            }
            FromFeedWebsocket::Ping { value } => {
                let feed_channel = match self.feed_channels.get_mut(&feed_conn_id) {
                    Some(chan) => chan,
                    None => return,
                };

                // Pong!
                let mut feed_serializer = FeedMessageSerializer::new();
                feed_serializer.push(feed_message::Pong(&value));
                if let Some(bytes) = feed_serializer.into_finalized() {
                    let _ = feed_channel.send(ToFeedWebsocket::Bytes(bytes));
                }
            }
            FromFeedWebsocket::Subscribe { chain } => {
                let feed_channel = match self.feed_channels.get_mut(&feed_conn_id) {
                    Some(chan) => chan,
                    None => return,
                };

                // Unsubscribe from previous chain if subscribed to one:
                let old_genesis_hash = self.chain_to_feed_conn_ids.remove_value(&feed_conn_id);

                // Get old chain if there was one:
                let node_state = &self.node_state;
                let old_chain =
                    old_genesis_hash.and_then(|hash| node_state.get_chain_by_genesis_hash(&hash));

                // Get new chain, ignoring the rest if it doesn't exist.
                let new_chain = match self.node_state.get_chain_by_genesis_hash(&chain) {
                    Some(chain) => chain,
                    None => return,
                };

                // Send messages to the feed about this subscription:
                let mut feed_serializer = FeedMessageSerializer::new();
                if let Some(old_chain) = old_chain {
                    feed_serializer.push(feed_message::UnsubscribedFrom(old_chain.genesis_hash()));
                }
                feed_serializer.push(feed_message::SubscribedTo(new_chain.genesis_hash()));
                feed_serializer.push(feed_message::TimeSync(time::now()));
                feed_serializer.push(feed_message::BestBlock(
                    new_chain.best_block().height,
                    new_chain.timestamp(),
                    new_chain.average_block_time(),
                ));
                feed_serializer.push(feed_message::BestFinalized(
                    new_chain.finalized_block().height,
                    new_chain.finalized_block().hash,
                ));
                feed_serializer.push(feed_message::ChainStatsUpdate(new_chain.stats()));
                if let Some(bytes) = feed_serializer.into_finalized() {
                    let _ = feed_channel.send(ToFeedWebsocket::Bytes(bytes));
                }

                let new_genesis_hash = new_chain.genesis_hash();
                if let Some(encoded_nodes) = self.node_state.added_nodes_messages(&new_genesis_hash)
                {
                    for msg in encoded_nodes {
                        let _ = feed_channel.send(msg.clone());
                    }
                }

                // Actually make a note of the new chain subscription:
                self.chain_to_feed_conn_ids
                    .insert(new_genesis_hash, feed_conn_id);
            }
            FromFeedWebsocket::Disconnected => {
                // The feed has disconnected; clean up references to it:
                self.chain_to_feed_conn_ids.remove_value(&feed_conn_id);
                self.feed_channels.remove(&feed_conn_id);
            }
        }
    }

    /// Finalize a [`FeedMessageSerializer`] and broadcast the result to feeds for the chain.
    fn finalize_and_broadcast_to_chain_feeds(
        &mut self,
        genesis_hash: &BlockHash,
        serializer: FeedMessageSerializer,
    ) {
        if let Some(bytes) = serializer.into_finalized() {
            self.broadcast_to_chain_feeds(genesis_hash, ToFeedWebsocket::Bytes(bytes));
        }
    }

    /// Send a message to all chain feeds.
    fn broadcast_to_chain_feeds(&mut self, genesis_hash: &BlockHash, message: ToFeedWebsocket) {
        if let Some(feeds) = self.chain_to_feed_conn_ids.get_values(genesis_hash) {
            for &feed_id in feeds {
                if let Some(chan) = self.feed_channels.get_mut(&feed_id) {
                    let _ = chan.send(message.clone());
                }
            }
        }
    }

    /// Finalize a [`FeedMessageSerializer`] and broadcast the result to all feeds
    fn finalize_and_broadcast_to_all_feeds(&mut self, serializer: FeedMessageSerializer) {
        if let Some(bytes) = serializer.into_finalized() {
            self.broadcast_to_all_feeds(ToFeedWebsocket::Bytes(bytes));
        }
    }

    /// Send a message to everybody.
    fn broadcast_to_all_feeds(&mut self, message: ToFeedWebsocket) {
        for chan in self.feed_channels.values_mut() {
            let _ = chan.send(message.clone());
        }
    }
}
