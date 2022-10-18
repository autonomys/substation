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
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    path::PathBuf,
};

#[derive(Default, Clone)]
struct NodeUpdates {
    system_connected: Option<SystemConnected>,
    system_interval: Option<SystemInterval>,
    block_import: Option<Block>,
    notify_finalized: Option<Finalized>,
    afg_authority_set: Option<AfgAuthoritySet>,
    location: Location,
}

#[derive(Default, Clone, Copy, Deserialize, Serialize)]
struct ChainMetadata {
    highest_node_count: usize,
}

#[derive(Default, Clone, Deserialize, Serialize)]
struct Metadata {
    chains: HashMap<BlockHash, ChainMetadata>,
}

impl Metadata {
    fn update<'a>(
        &mut self,
        chains: impl IntoIterator<Item = (&'a BlockHash, &'a ChainUpdates)>,
    ) -> bool {
        let mut updated = false;
        for (hash, chain) in chains {
            match self.chains.entry(*hash) {
                Entry::Vacant(entry) => {
                    updated = true;
                    entry.insert(ChainMetadata {
                        highest_node_count: chain.highest_node_count,
                    });
                }
                Entry::Occupied(mut entry) => {
                    if entry.get().highest_node_count != chain.highest_node_count {
                        updated = true;
                        entry.insert(ChainMetadata {
                            highest_node_count: chain.highest_node_count,
                        });
                    }
                }
            }
        }
        updated
    }
}

/// Structure with accumulated chain updates
#[derive(Default, Clone)]
struct ChainUpdates {
    /// Current node count
    node_count: usize,
    highest_node_count: usize,
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
    // Node state
    state: OrdinaryState,
    /// Accumulated updates for each chain
    chains: HashMap<BlockHash, ChainUpdates>,
    /// We maintain a mapping between NodeId and ConnId+LocalId, so that we know
    /// which messages are about which nodes.
    node_ids: BiMap<NodeId, (ConnId, ShardNodeId)>,
    /// Encoded node messages. (Usually send during node initialization)
    chain_nodes: HashMap<BlockHash, Vec<ToFeedWebsocket>>,
    /// Removed chains tracker
    removed_chains: HashSet<BlockHash>,
    send_node_data: bool,
    metadata: Metadata,
    metadata_path: Option<PathBuf>,
}

impl State {
    pub fn new(
        denylist: impl IntoIterator<Item = String>,
        max_third_party_nodes: usize,
        send_node_data: bool,
        metadata_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let metadata = if let Some(path) = &metadata_path {
            if path.exists() {
                let metadata_str = std::fs::read_to_string(path)?;
                serde_json::from_str(&metadata_str)?
            } else {
                Metadata::default()
            }
        } else {
            Default::default()
        };

        // Update max node count
        let chains = metadata
            .chains
            .iter()
            .map(|(hash, ChainMetadata { highest_node_count })| {
                (
                    *hash,
                    ChainUpdates {
                        highest_node_count: *highest_node_count,
                        ..Default::default()
                    },
                )
            })
            .collect();
        Ok(Self {
            state: OrdinaryState::new(denylist, max_third_party_nodes),
            chains,
            node_ids: BiMap::new(),
            chain_nodes: HashMap::new(),
            removed_chains: HashSet::new(),
            send_node_data,
            metadata,
            metadata_path,
        })
    }

    pub fn iter_chains(&self) -> impl Iterator<Item = StateChain<'_>> {
        self.state.iter_chains()
    }
    pub fn get_chain_by_genesis_hash(&self, genesis_hash: &BlockHash) -> Option<StateChain<'_>> {
        self.state.get_chain_by_genesis_hash(genesis_hash)
    }

    pub fn get_chain_max_node_count(&self, genesis_hash: &BlockHash) -> Option<usize> {
        self.metadata
            .chains
            .get(genesis_hash)
            .map(|meta| meta.highest_node_count)
    }

    /// Drain updates for all feeds and return serializer.
    pub fn drain_updates_for_all_feeds(&mut self) -> FeedMessageSerializer {
        if self.metadata.update(&self.chains) {
            if let Some(path) = &self.metadata_path {
                if let Err(err) = serde_json::to_vec(&self.metadata)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| std::fs::write(path, bytes).map_err(anyhow::Error::from))
                {
                    log::error!("Failed to save metadata: {err}");
                }
            }
        }

        let mut feed = FeedMessageSerializer::new();
        for (genesis_hash, chain_updates) in &mut self.chains {
            let ChainUpdates {
                node_count,
                highest_node_count,
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
                *highest_node_count,
            ));
        }
        for genesis_hash in self.removed_chains.drain() {
            feed.push(feed_message::RemovedChain(genesis_hash))
        }
        feed
    }

    const MSGS_PER_WS_MSG: usize = 64;

    /// Method which would return updates for each chain with its genesis hash
    pub fn drain_chain_updates(
        &'_ mut self,
    ) -> impl Iterator<Item = (BlockHash, Vec<FeedMessageSerializer>)> + '_ {
        self.chains
            .iter_mut()
            .filter(|(_, updates)| updates.node_count != 0)
            .map(|(genesis_hash, updates)| {
                let mut vec = vec![];

                for removed_nodes in &updates.removed_nodes.drain().chunks(Self::MSGS_PER_WS_MSG) {
                    let mut feed = FeedMessageSerializer::new();
                    for removed_node in removed_nodes {
                        feed.push(feed_message::RemovedNode(
                            removed_node.get_chain_node_id().into(),
                        ));
                    }
                    vec.push(feed);
                }

                for added_nodes in &updates.added_nodes.drain().chunks(Self::MSGS_PER_WS_MSG) {
                    let mut feed = FeedMessageSerializer::new();
                    for (added_node_id, node) in added_nodes {
                        feed.push(feed_message::AddedNode(
                            added_node_id.get_chain_node_id().into(),
                            &node,
                        ));
                    }
                    vec.push(feed);
                }

                for updated_nodes in &updates.updated_nodes.drain().chunks(Self::MSGS_PER_WS_MSG) {
                    let mut feed = FeedMessageSerializer::new();
                    for (node_id, updates) in updated_nodes {
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
                            self.state.update_node(
                                node_id.clone(),
                                &SystemConnected(connected),
                                &mut feed,
                            );
                        }
                        if let Some(interval) = updates.system_interval {
                            self.state.update_node(
                                node_id.clone(),
                                &SystemInterval(interval),
                                &mut feed,
                            );
                        }
                        if let Some(import) = updates.block_import {
                            self.state.update_node(
                                node_id.clone(),
                                &BlockImport(import),
                                &mut feed,
                            );
                        }
                        if let Some(finalized) = updates.notify_finalized {
                            self.state.update_node(
                                node_id.clone(),
                                &NotifyFinalized(finalized),
                                &mut feed,
                            );
                        }
                        if let Some(authority) = updates.afg_authority_set {
                            self.state
                                .update_node(node_id, &AfgAuthoritySet(authority), &mut feed);
                        }
                    }
                    vec.push(feed)
                }

                (*genesis_hash, vec)
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
        } = match self.state.add_node(genesis_hash, node) {
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
        updates.highest_node_count = updates.highest_node_count.max(chain_node_count);
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

        let updates = if let Some(chain) = self.state.get_chain_by_node_id(node_id) {
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
            if let Some(chain) = self.state.get_chain_by_node_id(node_id) {
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
                } = match self.state.remove_node(node_id) {
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
        self.state.update_node_location(node_id, location.clone());

        if self.send_node_data {
            if let Some(loc) = location {
                if let Some(chain) = self.state.get_chain_by_node_id(node_id) {
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
        for chain in self.state.iter_chains() {
            let all_feed_messages: Vec<_> = chain
                .nodes_slice()
                .par_iter()
                .enumerate()
                .chunks(Self::MSGS_PER_WS_MSG)
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
                .map(ToFeedWebsocket::new)
                .collect();

            self.chain_nodes
                .insert(chain.genesis_hash(), all_feed_messages);
        }
    }

    pub fn added_nodes_messages(&self, genesis_hash: &BlockHash) -> Option<&[ToFeedWebsocket]> {
        self.chain_nodes.get(genesis_hash).map(AsRef::as_ref)
    }
}
