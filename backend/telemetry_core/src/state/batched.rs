use super::{
    state::{State as OrdinaryState, StateChain},
    AddNodeResult, Node, NodeAddedToChain, NodeId, RemovedNode,
};
use crate::{
    aggregator::{ConnId, ToFeedWebsocket},
    feed_message::{self, FeedMessageSerializer, FeedMessageWriter},
    find_location::Location,
};
use bimap::BiMap;
use common::{
    internal_messages::{MuteReason, ShardNodeId},
    node_message::{self, AfgAuthoritySet, Finalized, SystemConnected, SystemInterval},
    node_types::{Block, BlockHash, NodeDetails},
};
use std::collections::{HashMap, HashSet};

#[derive(Default, Clone)]
struct NodeUpdates {
    system_connected: Option<SystemConnected>,
    system_interval: Option<SystemInterval>,
    block_import: Option<Block>,
    notify_finalized: Option<Finalized>,
    afg_authority_set: Option<AfgAuthoritySet>,
    location: Location,
}

/// Structure with accumulated chain updates
#[derive(Default, Clone)]
struct ChainUpdates {
    /// Current node count
    node_count: usize,
    has_chain_label_changed: bool,
    /// Current chain label
    chain_label: Box<str>,

    added_nodes: HashMap<NodeId, Node>,
    removed_nodes: HashSet<NodeId>,
    updated_nodes: HashMap<NodeId, NodeUpdates>,
}

/// Wrapper which batches updates to state.
#[derive(Clone)]
pub struct State {
    // Previous state (which is read only)
    prev: OrdinaryState,
    // Next state (which is write only)
    next: OrdinaryState,
    /// Accumulated updates for each chain
    chains: HashMap<BlockHash, ChainUpdates>,
    /// We maintain a mapping between NodeId and ConnId+LocalId, so that we know
    /// which messages are about which nodes.
    node_ids: BiMap<NodeId, (ConnId, ShardNodeId)>,
    /// Encoded node messages. (Usually send during node initialization)
    ///
    /// Basically `prev` state encoded.
    chain_nodes: HashMap<BlockHash, Vec<ToFeedWebsocket>>,
    /// Removed chains tracker
    removed_chains: HashSet<BlockHash>,
    send_node_data: bool,
}

impl State {
    pub fn new(
        denylist: impl IntoIterator<Item = String>,
        max_third_party_nodes: usize,
        send_node_data: bool,
    ) -> Self {
        Self {
            prev: OrdinaryState::new([], max_third_party_nodes),
            next: OrdinaryState::new(denylist, max_third_party_nodes),
            chains: HashMap::new(),
            node_ids: BiMap::new(),
            chain_nodes: HashMap::new(),
            removed_chains: HashSet::new(),
            send_node_data,
        }
    }

    pub fn iter_chains(&self) -> impl Iterator<Item = StateChain<'_>> {
        self.prev.iter_chains()
    }
    pub fn get_chain_by_genesis_hash(&self, genesis_hash: &BlockHash) -> Option<StateChain<'_>> {
        self.prev.get_chain_by_genesis_hash(genesis_hash)
    }

    /// Drain updates for all feeds and return serializer.
    pub fn drain_updates_for_all_feeds(&mut self) -> FeedMessageSerializer {
        let mut feed = FeedMessageSerializer::new();
        for (genesis_hash, chain_updates) in &mut self.chains {
            let ChainUpdates {
                node_count,
                has_chain_label_changed,
                chain_label,
                ..
            } = chain_updates;

            if *has_chain_label_changed {
                feed.push(feed_message::RemovedChain(*genesis_hash));
                *has_chain_label_changed = false;
            }

            feed.push(feed_message::AddedChain(
                chain_label,
                *genesis_hash,
                *node_count,
            ));
        }
        for genesis_hash in std::mem::take(&mut self.removed_chains) {
            feed.push(feed_message::RemovedChain(genesis_hash))
        }
        feed
    }

    /// Method which would return updates for each chain with its genesis hash
    pub fn drain_chain_updates(
        &'_ mut self,
    ) -> impl Iterator<Item = (BlockHash, FeedMessageSerializer)> + '_ {
        self.prev.clone_from(&self.next);
        self.chains
            .iter_mut()
            .filter(|(_, updates)| updates.node_count != 0)
            .map(|(genesis_hash, updates)| {
                let mut feed = FeedMessageSerializer::new();

                for removed_node in std::mem::take(&mut updates.removed_nodes) {
                    feed.push(feed_message::RemovedNode(
                        removed_node.get_chain_node_id().into(),
                    ));
                }
                for (added_node_id, node) in std::mem::take(&mut updates.added_nodes) {
                    feed.push(feed_message::AddedNode(
                        added_node_id.get_chain_node_id().into(),
                        &node,
                    ));
                }
                for (node_id, updates) in std::mem::take(&mut updates.updated_nodes) {
                    use node_message::Payload::*;

                    if let Some(loc) = updates.location {
                        feed.push(feed_message::LocatedNode(
                            node_id.get_chain_node_id().into(),
                            loc.latitude,
                            loc.longitude,
                            &loc.city,
                        ))
                    }

                    // TODO: decouple updating and serializing in a nice way.
                    if let Some(connected) = updates.system_connected {
                        self.next.update_node(
                            node_id.clone(),
                            &SystemConnected(connected),
                            &mut feed,
                        );
                    }
                    if let Some(interval) = updates.system_interval {
                        self.next.update_node(
                            node_id.clone(),
                            &SystemInterval(interval),
                            &mut feed,
                        );
                    }
                    if let Some(import) = updates.block_import {
                        self.next
                            .update_node(node_id.clone(), &BlockImport(import), &mut feed);
                    }
                    if let Some(finalized) = updates.notify_finalized {
                        self.next.update_node(
                            node_id.clone(),
                            &NotifyFinalized(finalized),
                            &mut feed,
                        );
                    }
                    if let Some(authority) = updates.afg_authority_set {
                        self.next
                            .update_node(node_id, &AfgAuthoritySet(authority), &mut feed);
                    }
                }
                (*genesis_hash, feed)
            })
    }

    pub fn add_node(
        &mut self,
        genesis_hash: BlockHash,
        shard_conn_id: ConnId,
        local_id: ShardNodeId,
        node: NodeDetails,
    ) -> Result<NodeId, MuteReason> {
        let NodeAddedToChain {
            id: node_id,
            new_chain_label,
            node,
            chain_node_count,
            has_chain_label_changed,
            ..
        } = match self.next.add_node(genesis_hash, node) {
            AddNodeResult::NodeAddedToChain(details) => details,
            AddNodeResult::ChainOverQuota => return Err(MuteReason::Overquota),
            AddNodeResult::ChainOnDenyList => return Err(MuteReason::ChainNotAllowed),
        };
        self.removed_chains.remove(&genesis_hash);

        // Record ID <-> (shardId,localId) for future messages:
        self.node_ids.insert(node_id, (shard_conn_id, local_id));

        let updates = self.chains.entry(genesis_hash).or_default();

        if self.send_node_data {
            updates.removed_nodes.remove(&node_id);
            updates.added_nodes.insert(node_id, node.clone());
        }

        updates.has_chain_label_changed = has_chain_label_changed;
        updates.node_count = chain_node_count;
        updates.chain_label = new_chain_label.to_owned().into_boxed_str();

        Ok(node_id)
    }

    pub fn update_node(
        &mut self,
        shard_conn_id: ConnId,
        local_id: ShardNodeId,
        payload: node_message::Payload,
    ) {
        let node_id = match self.node_ids.get_by_right(&(shard_conn_id, local_id)) {
            Some(id) => *id,
            None => {
                log::error!(
                    "Cannot find ID for node with shard/connectionId of {:?}/{:?}",
                    shard_conn_id,
                    local_id
                );
                return;
            }
        };

        if !self.send_node_data {
            return;
        }

        let updates = if let Some(chain) = self.next.get_chain_by_node_id(node_id) {
            self.chains
                .entry(chain.genesis_hash())
                .or_default()
                .updated_nodes
                .entry(node_id)
                .or_default()
        } else {
            return;
        };

        use node_message::Payload::*;

        match payload {
            SystemConnected(connected) => updates.system_connected = Some(connected),
            SystemInterval(interval) => updates.system_interval = Some(interval),
            BlockImport(import) => updates.block_import = Some(import),
            NotifyFinalized(finalized) => updates.notify_finalized = Some(finalized),
            AfgAuthoritySet(authority) => updates.afg_authority_set = Some(authority),
        }
    }

    pub fn remove_node(&mut self, shard_conn_id: ConnId, local_id: ShardNodeId) {
        let node_id = match self.node_ids.remove_by_right(&(shard_conn_id, local_id)) {
            Some((node_id, _)) => node_id,
            None => {
                log::error!(
                    "Cannot find ID for node with shard/connectionId of {:?}/{:?}",
                    shard_conn_id,
                    local_id
                );
                return;
            }
        };

        self.remove_nodes(Some(node_id));
    }

    pub fn disconnect_node(&mut self, shard_conn_id: ConnId) {
        let node_ids_to_remove: Vec<NodeId> = self
            .node_ids
            .iter()
            .filter(|(_, &(this_shard_conn_id, _))| shard_conn_id == this_shard_conn_id)
            .map(|(&node_id, _)| node_id)
            .collect();
        self.remove_nodes(node_ids_to_remove);
    }

    fn remove_nodes(&mut self, node_ids: impl IntoIterator<Item = NodeId>) {
        // Group by chain to simplify the handling of feed messages:
        let mut node_ids_per_chain = HashMap::<BlockHash, Vec<NodeId>>::new();
        for node_id in node_ids.into_iter() {
            if let Some(chain) = self.next.get_chain_by_node_id(node_id) {
                node_ids_per_chain
                    .entry(chain.genesis_hash())
                    .or_default()
                    .push(node_id);
            }
        }

        for (chain_label, node_ids) in node_ids_per_chain {
            let updates = if let Some(updates) = self.chains.get_mut(&chain_label) {
                updates
            } else {
                continue;
            };
            if updates.node_count == node_ids.len() {
                drop(updates);
                self.chains.remove(&chain_label);
                self.removed_chains.insert(chain_label);
                continue;
            }

            for node_id in node_ids {
                self.node_ids.remove_by_left(&node_id);

                let RemovedNode {
                    chain_node_count,
                    new_chain_label,
                    ..
                } = match self.next.remove_node(node_id) {
                    Some(details) => details,
                    None => {
                        log::error!("Could not find node {node_id:?}");
                        continue;
                    }
                };

                updates.chain_label = new_chain_label.clone();
                updates.node_count = chain_node_count;
                if self.send_node_data {
                    updates.added_nodes.remove(&node_id);
                    updates.updated_nodes.remove(&node_id);
                    updates.removed_nodes.insert(node_id);
                }
            }
        }
    }

    pub fn update_node_location(&mut self, node_id: NodeId, location: Location) {
        self.next.update_node_location(node_id, location.clone());

        if self.send_node_data {
            if let Some(loc) = location {
                if let Some(chain) = self.next.get_chain_by_node_id(node_id) {
                    let updates = self
                        .chains
                        .entry(chain.genesis_hash())
                        .or_default()
                        .updated_nodes
                        .entry(node_id)
                        .or_default();
                    updates.location = Some(loc);
                }
            }
        }
    }

    pub fn update_added_nodes_messages(&mut self) {
        use rayon::prelude::*;

        if !self.send_node_data {
            return;
        }

        self.chain_nodes.clear();

        // If many (eg 10k) nodes are connected, serializing all of their info takes time.
        // So, parallelise this with Rayon, but we still send out messages for each node in order
        // (which is helpful for the UI as it tries to maintain a sorted list of nodes). The chunk
        // size is the max number of node info we fit into 1 message; smaller messages allow the UI
        // to react a little faster and not have to wait for a larger update to come in. A chunk size
        // of 64 means each message is ~32k.
        for chain in self.prev.iter_chains() {
            let all_feed_messages: Vec<_> = chain
                .nodes_slice()
                .par_iter()
                .enumerate()
                .chunks(64)
                .filter_map(|nodes| {
                    let mut feed_serializer = FeedMessageSerializer::new();
                    for (node_id, node) in nodes
                        .iter()
                        .filter_map(|&(idx, n)| n.as_ref().map(|n| (idx, n)))
                    {
                        feed_serializer.push(feed_message::AddedNode(node_id, node));
                        feed_serializer.push(feed_message::FinalizedBlock(
                            node_id,
                            node.finalized().height,
                            node.finalized().hash,
                        ));
                        if node.stale() {
                            feed_serializer.push(feed_message::StaleNode(node_id));
                        }
                    }
                    feed_serializer.into_finalized()
                })
                .map(ToFeedWebsocket::Bytes)
                .collect();

            self.chain_nodes
                .insert(chain.genesis_hash(), all_feed_messages);
        }
    }

    pub fn added_nodes_messages(&self, genesis_hash: &BlockHash) -> Option<&[ToFeedWebsocket]> {
        self.chain_nodes.get(genesis_hash).map(AsRef::as_ref)
    }
}
