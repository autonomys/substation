// Source code for the Substrate Telemetry Server.
// Copyright (C) 2022 Parity Technologies (UK) Ltd.
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

use super::counter::{Counter, CounterValue};
use crate::feed_message::ChainStats;

#[derive(Default, Clone)]
pub struct ChainStatsCollator {
    version: Counter<String>,
}

impl ChainStatsCollator {
    pub fn add_or_remove_node(
        &mut self,
        details: &common::node_types::NodeDetails,
        op: CounterValue,
    ) {
        self.version.modify(Some(&*details.version), op);
    }

    pub fn generate(&self) -> ChainStats {
        ChainStats {
            version: self.version.generate_ranking_top(10),
        }
    }
}
