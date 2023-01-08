use alloc::{format, string::String, vec::Vec};
use core::fmt::Display;
use core::str::FromStr;

use macaddr::MacAddr6;
use serde::{Deserialize, Serialize};

use std::net::{Ipv4Addr, SocketAddrV4};

use crate::Bijection;

pub const UNASSIGNED_GROUP_ID: u8 = 187;
pub const STATIC_IP_USERCLASS: &str = "pixie-static-ip";

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum DhcpMode {
    /// Unknown clients will be assigned IPs in a /24 subnet of 10.{UNASSIGNED_GROUP_ID}.0.0.
    Static,
    /// Unknown clients are assumed to receive an IP address by another DHCP server.
    /// The specified IP must belong to the network on which the other DHCP server gives IPs,
    /// and the DHCP interface must have an IP on this network.
    Proxy(Ipv4Addr),
}

/// Registered clients will always be assigned an IP in the form
/// 10.{group_id}.{column_id}.{row_id} if they specify the STATIC_IP_USERCLASS user class;
/// this can be done for example by setting the UserClass= option in the [DHCPv4] section of
/// systemd-networkd config files.
/// Note that for this to work, the specified network interface must have an IP on the 10.0.0.0/8
/// subnet.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize, Clone)]
pub struct DhcpConfig {
    /// IP behaviour of unregistered clients and while running pixie itself.
    pub mode: DhcpMode,
    /// Name of the interface on which clients are reachable.
    pub interface: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpConfig {
    pub max_payload: usize,
    pub listen_on: SocketAddrV4,
    pub password: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsConfig {
    pub chunks_port: u16,
    pub hint_port: u16,
    pub bits_per_second: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Reboot,
    Register,
    Push,
    Pull,
    Wait,
}

impl FromStr for ActionKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "reboot" => Ok(ActionKind::Reboot),
            "register" => Ok(ActionKind::Register),
            "push" => Ok(ActionKind::Push),
            "pull" => Ok(ActionKind::Pull),
            "wait" => Ok(ActionKind::Wait),
            _ => Err(format!("unknown action kind: {}", s)),
        }
    }
}

impl Display for ActionKind {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        match self {
            ActionKind::Reboot => write!(fmt, "reboot"),
            ActionKind::Register => write!(fmt, "register"),
            ActionKind::Push => write!(fmt, "push"),
            ActionKind::Pull => write!(fmt, "pull"),
            ActionKind::Wait => write!(fmt, "wait"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct BootConfig {
    pub unregistered: ActionKind,
    pub default: ActionKind,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Config {
    pub dhcp: DhcpConfig,
    pub http: HttpConfig,
    pub hosts: HostsConfig,
    pub boot: BootConfig,
    pub groups: Bijection<String, u8>,
    pub images: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Unit {
    pub mac: MacAddr6,
    pub group: u8,
    pub row: u8,
    pub col: u8,
    pub curr_action: Option<ActionKind>,
    pub curr_progress: Option<(usize, usize)>,
    pub next_action: ActionKind,
    pub image: String,
}

impl Unit {
    pub fn static_ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(10, self.group, self.row, self.col)
    }
}
