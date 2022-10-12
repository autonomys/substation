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

use super::inner_loop;
use common::id_type;
use futures::{Sink, SinkExt};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

id_type! {
    /// A unique Id is assigned per websocket connection (or more accurately,
    /// per feed socket and per shard socket). This can be combined with the
    /// [`LocalId`] of messages to give us a global ID.
    pub struct ConnId(u64)
}

#[derive(Clone)]
pub struct Aggregator(Arc<AggregatorInternal>);

/// Options to configure the aggregator loop(s)
#[derive(Debug, Clone)]
pub struct AggregatorOpts {
    /// Any node from these chains is muted
    pub denylist: Vec<String>,
    /// If our incoming message queue exceeds this length, we start
    /// dropping non-essential messages.
    pub max_queue_len: usize,
    /// How many nodes from third party chains are allowed to connect
    /// before we prevent connections from them.
    pub max_third_party_nodes: usize,
    /// Send updates periodically
    pub update_every: Duration,
    /// Should we send node data?
    pub send_node_data: bool,
    pub metadata_path: Option<PathBuf>,
}

struct AggregatorInternal {
    /// Shards that connect are each assigned a unique connection ID.
    /// This helps us know who to send messages back to (especially in
    /// conjunction with the `ShardNodeId` that messages will come with).
    shard_conn_id: AtomicU64,
    /// Feeds that connect have their own unique connection ID, too.
    feed_conn_id: AtomicU64,
    /// Send messages in to the aggregator from the outside via this. This is
    /// stored here so that anybody holding an `Aggregator` handle can
    /// make use of it.
    tx_to_aggregator: flume::Sender<inner_loop::ToAggregator>,
}

impl Aggregator {
    /// Spawn a new Aggregator. This connects to the telemetry backend
    pub async fn spawn(
        AggregatorOpts {
            denylist,
            max_queue_len,
            max_third_party_nodes,
            update_every,
            send_node_data,
            metadata_path,
        }: AggregatorOpts,
    ) -> anyhow::Result<Aggregator> {
        let (tx_to_aggregator, rx_from_external) = flume::unbounded();

        tokio::task::spawn({
            let tx_to_aggregator = tx_to_aggregator.clone();
            let mut timer = tokio::time::interval(update_every);
            // First tick is instant
            timer.tick().await;

            async move {
                while let Ok(()) = tx_to_aggregator.send(inner_loop::ToAggregator::SendUpdates) {
                    timer.tick().await;
                }
            }
        });

        // Handle any incoming messages in our handler loop:
        tokio::spawn(Aggregator::handle_messages(
            rx_from_external,
            max_queue_len,
            denylist,
            max_third_party_nodes,
            send_node_data,
            metadata_path,
        ));

        // Return a handle to our aggregator:
        Ok(Aggregator(Arc::new(AggregatorInternal {
            shard_conn_id: AtomicU64::new(1),
            feed_conn_id: AtomicU64::new(1),
            tx_to_aggregator,
        })))
    }

    /// This is spawned into a separate task and handles any messages coming
    /// in to the aggregator. If nobody is holding the tx side of the channel
    /// any more, this task will gracefully end.
    async fn handle_messages(
        rx_from_external: flume::Receiver<inner_loop::ToAggregator>,
        max_queue_len: usize,
        denylist: Vec<String>,
        max_third_party_nodes: usize,
        send_node_data: bool,
        metadata_path: Option<PathBuf>,
    ) {
        match inner_loop::InnerLoop::new(
            denylist,
            max_queue_len,
            max_third_party_nodes,
            send_node_data,
            metadata_path,
        ) {
            Ok(ok) => ok,
            Err(err) => {
                log::error!("Inner loop failed to construct: {err}");
                return;
            }
        }
        .handle(rx_from_external.into_stream())
        .await
    }

    /// Gather metrics from our aggregator loop
    pub async fn gather_metrics(&self) -> anyhow::Result<inner_loop::Metrics> {
        let (tx, rx) = flume::unbounded();
        let msg = inner_loop::ToAggregator::GatherMetrics(tx);

        self.0.tx_to_aggregator.send_async(msg).await?;

        let metrics = rx.recv_async().await?;
        Ok(metrics)
    }

    /// Return a sink that a shard can send messages into to be handled by the aggregator.
    pub fn subscribe_shard(
        &self,
    ) -> impl Sink<inner_loop::FromShardWebsocket, Error = anyhow::Error> + Send + Sync + Unpin + 'static
    {
        // Assign a unique aggregator-local ID to each connection that subscribes, and pass
        // that along with every message to the aggregator loop:
        let shard_conn_id = self
            .0
            .shard_conn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tx_to_aggregator = self.0.tx_to_aggregator.clone();

        // Calling `send` on this Sink requires Unpin. There may be a nicer way than this,
        // but pinning by boxing is the easy solution for now:
        Box::pin(tx_to_aggregator.into_sink().with(move |msg| async move {
            Ok(inner_loop::ToAggregator::FromShardWebsocket(
                shard_conn_id.into(),
                msg,
            ))
        }))
    }

    /// Return a sink that a feed can send messages into to be handled by the aggregator.
    pub fn subscribe_feed(
        &self,
    ) -> (
        u64,
        impl Sink<inner_loop::FromFeedWebsocket, Error = anyhow::Error> + Send + Sync + Unpin + 'static,
    ) {
        // Assign a unique aggregator-local ID to each connection that subscribes, and pass
        // that along with every message to the aggregator loop:
        let feed_conn_id = self
            .0
            .feed_conn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tx_to_aggregator = self.0.tx_to_aggregator.clone();

        // Calling `send` on this Sink requires Unpin. There may be a nicer way than this,
        // but pinning by boxing is the easy solution for now:
        (
            feed_conn_id,
            Box::pin(tx_to_aggregator.into_sink().with(move |msg| async move {
                Ok(inner_loop::ToAggregator::FromFeedWebsocket(
                    feed_conn_id.into(),
                    msg,
                ))
            })),
        )
    }
}
