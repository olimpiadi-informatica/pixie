use crate::{Action, Bijection};
use alloc::{string::String, vec::Vec};
use macaddr::MacAddr6;
use serde::{Deserialize, Serialize};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum DhcpMode {
    /// Unknown clients will be assigned IPs in the specified range.
    Static(Ipv4Addr, Ipv4Addr),
    /// Unknown clients are assumed to receive an IP address by another DHCP server.
    /// The specified IP must belong to the network on which the other DHCP server gives IPs,
    /// and the DHCP interface must have an IP on this network.
    Proxy(Ipv4Addr),
}

/// Registered clients will always be assigned an IP in the form
/// 10.{group_id}.{column_id}.{row_id}.
/// Note that for this to work, the specified network interface must have an IP on the 10.0.0.0/8
/// subnet; BEWARE that dnsmasq can be picky about the order of IP addresses.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize, Clone)]
pub struct HostsConfig {
    /// Listen on address
    pub listen_on: Ipv4Addr,
    /// DHCP server.
    pub dhcp: DhcpMode,
    /// Hosts file to use for DHCP hostnames.
    pub hostsfile: Option<PathBuf>,

    pub broadcast_speed: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpConfig {
    pub listen_on: SocketAddrV4,
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Config {
    pub hosts: HostsConfig,
    pub http: HttpConfig,
    pub groups: Bijection<String, u8>,
    pub images: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Unit {
    pub mac: MacAddr6,
    pub group: u8,
    pub row: u8,
    pub col: u8,
    pub curr_action: Option<Action>,
    pub curr_progress: Option<(usize, usize)>,
    pub next_action: Action,
    pub image: String,
    #[serde(default)]
    pub last_ping_timestamp: u64,
    #[serde(default)]
    pub last_ping_comment: Vec<u8>,
}

impl Unit {
    pub fn static_ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(10, self.group, self.row, self.col)
    }
}
