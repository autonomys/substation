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

use std::net::IpAddr;
use std::sync::Arc;

use maxminddb::{geoip2::City, Reader as GeoIpReader};
use rustc_hash::FxHashMap;

use common::node_types::NodeLocation;

/// The returned location is optional; it may be None if not found.
pub type Location = Option<Arc<NodeLocation>>;

/// This struct can be used to make location requests, given
/// an IPV4 address.
#[derive(Debug)]
pub struct Locator {
    city: maxminddb::Reader<&'static [u8]>,
    cache: FxHashMap<IpAddr, Arc<NodeLocation>>,
}

impl Locator {
    /// taken from here: https://github.com/P3TERX/GeoLite.mmdb/releases/tag/2022.06.07
    const CITY_DATA: &'static [u8] = include_bytes!("GeoLite2-City.mmdb");

    pub fn new(cache: FxHashMap<IpAddr, Arc<NodeLocation>>) -> Self {
        Self {
            city: GeoIpReader::from_source(Self::CITY_DATA).expect("City data is always valid"),
            cache,
        }
    }

    pub fn locate(&mut self, ip: IpAddr) -> Location {
        // Return location quickly if it's cached:
        let cached_loc = self.cache.get(&ip).cloned();
        if cached_loc.is_some() {
            return cached_loc;
        }

        let City { city, location, .. } = self.city.lookup(ip.into()).ok()?;
        let city = city
            .as_ref()?
            .names
            .as_ref()?
            .get("en")?
            .to_string()
            .into_boxed_str();
        let latitude = location.as_ref()?.latitude? as f32;
        let longitude = location?.longitude? as f32;

        let location = Arc::new(NodeLocation {
            city,
            latitude,
            longitude,
        });
        self.cache.insert(ip, Arc::clone(&location));

        Some(location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_construction() {
        Locator::new(Default::default());
    }

    #[test]
    fn locate_random_ip() {
        let ip = "12.5.56.25".parse().unwrap();
        let node_location = Locator::new(Default::default()).locate(ip).unwrap();
        assert_eq!(&*node_location.city, "El Paso");
    }
}
