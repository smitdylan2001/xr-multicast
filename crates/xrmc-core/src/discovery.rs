//! Finding headsets on the LAN (port of `src/main/discovery.js`, plus mDNS).
//!
//! * a subnet scan: every host of this computer's /24 network(s), probed on the
//!   adb TCP port with a short timeout and bounded concurrency;
//! * mDNS: Android 11+ advertises wireless debugging as `_adb-tls-connect._tcp`
//!   and `adb tcpip` devices may advertise `_adb._tcp`.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// An IPv4 address of this computer on a real (non-loopback, non-link-local) interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalInterface {
    pub iface: String,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

/// IPv4 addresses of this computer on real interfaces.
pub fn local_ipv4s() -> Vec<LocalInterface> {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("discovery: cannot list network interfaces: {e}");
            return Vec::new();
        }
    };
    addrs
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(LocalInterface {
                iface: i.name,
                address: v4.ip,
                netmask: v4.netmask,
            }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .filter(|i| !i.address.is_loopback() && !i.address.is_link_local())
        .collect()
}

/// Hosts to probe for the given interfaces.
///
/// Networks bigger than /24 are clamped to the /24 around this computer so a
/// scan stays quick (~254 probes per interface). The network, broadcast and
/// own addresses are skipped; loopback and link-local interfaces are ignored.
pub fn candidate_hosts_for(interfaces: &[LocalInterface]) -> Vec<Ipv4Addr> {
    let mut seen = BTreeSet::new();
    let mut hosts = Vec::new();
    for i in interfaces {
        if i.address.is_loopback() || i.address.is_link_local() {
            continue;
        }
        let mask = u32::from(i.netmask).max(0xFFFF_FF00);
        let me = u32::from(i.address);
        let network = me & mask;
        let broadcast = network | !mask;
        for h in network.saturating_add(1)..broadcast {
            if h != me && seen.insert(h) {
                hosts.push(Ipv4Addr::from(h));
            }
        }
    }
    hosts
}

/// Hosts to probe on this computer's networks.
pub fn candidate_hosts() -> Vec<Ipv4Addr> {
    candidate_hosts_for(&local_ipv4s())
}

/// Can a TCP connection to `host:port` be opened within `timeout`?
pub async fn probe(host: &str, port: u16, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

/// Options for [`scan_hosts`] / [`scan_for_adb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Per-host connect timeout.
    pub timeout: Duration,
    /// Probes in flight at once.
    pub concurrency: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            timeout: Duration::from_millis(400),
            concurrency: 96,
        }
    }
}

/// Probe `hosts` on `port`; returns the ones that accepted a connection, in address order.
pub async fn scan_hosts(hosts: Vec<Ipv4Addr>, port: u16, opts: ScanOptions) -> Vec<Ipv4Addr> {
    let permits = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for host in hosts {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break;
        };
        let timeout = opts.timeout;
        tasks.spawn(async move {
            let open = probe(&host.to_string(), port, timeout).await;
            drop(permit);
            open.then_some(host)
        });
    }
    let mut open = Vec::new();
    while let Some(r) = tasks.join_next().await {
        if let Ok(Some(h)) = r {
            open.push(h);
        }
    }
    open.sort();
    open
}

/// Find hosts on the local subnet(s) with the adb TCP port open.
pub async fn scan_for_adb(port: u16, opts: ScanOptions) -> Vec<Ipv4Addr> {
    scan_hosts(candidate_hosts(), port, opts).await
}

/// mDNS service type of Android 11+ wireless debugging.
pub const ADB_TLS_CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";
/// mDNS service type some devices advertise in `adb tcpip` mode.
pub const ADB_SERVICE: &str = "_adb._tcp.local.";

/// An adb service found via mDNS (same shape as the Electron app's `adb mdns services` parse).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MdnsService {
    /// Instance name, e.g. `adb-2G0YC5ZG8L0123-AbCdEf`.
    pub name: String,
    /// Service type without the domain, e.g. `_adb-tls-connect._tcp`.
    pub service: String,
    pub ip: String,
    pub port: u16,
}

/// Split an mDNS full name (`<instance>.<_service._tcp>.local.`) into instance and service.
pub fn split_mdns_fullname(fullname: &str) -> Option<(String, String)> {
    let i = fullname.find("._adb")?;
    let instance = &fullname[..i];
    let service = fullname[i + 1..]
        .trim_end_matches('.')
        .trim_end_matches(".local");
    Some((instance.to_string(), service.to_string()))
}

/// Errors from mDNS discovery.
#[derive(Debug, thiserror::Error)]
#[error("mDNS: {0}")]
pub struct MdnsError(String);

/// Browse for adb services (`_adb-tls-connect._tcp` and `_adb._tcp`) for `window`,
/// returning every IPv4 endpoint that resolved.
pub async fn discover_adb_services(window: Duration) -> Result<Vec<MdnsService>, MdnsError> {
    let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| MdnsError(e.to_string()))?;
    let mut receivers = Vec::new();
    for ty in [ADB_TLS_CONNECT_SERVICE, ADB_SERVICE] {
        receivers.push(daemon.browse(ty).map_err(|e| MdnsError(e.to_string()))?);
    }
    let found = Arc::new(std::sync::Mutex::new(BTreeSet::new()));
    let mut tasks = JoinSet::new();
    for rx in receivers {
        let found = found.clone();
        tasks.spawn(async move {
            while let Ok(event) = rx.recv_async().await {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                    let Some((name, service)) = split_mdns_fullname(&info.fullname) else {
                        continue;
                    };
                    let mut set = found.lock().unwrap_or_else(|e| e.into_inner());
                    for ip in info.get_addresses_v4() {
                        set.insert(MdnsService {
                            name: name.clone(),
                            service: service.clone(),
                            ip: ip.to_string(),
                            port: info.port,
                        });
                    }
                }
            }
        });
    }
    tokio::time::sleep(window).await;
    for ty in [ADB_TLS_CONNECT_SERVICE, ADB_SERVICE] {
        let _ = daemon.stop_browse(ty);
    }
    let _ = daemon.shutdown();
    tasks.abort_all();
    let set = found.lock().unwrap_or_else(|e| e.into_inner());
    Ok(set.iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(addr: [u8; 4], mask: [u8; 4]) -> LocalInterface {
        LocalInterface {
            iface: "eth0".into(),
            address: Ipv4Addr::from(addr),
            netmask: Ipv4Addr::from(mask),
        }
    }

    #[test]
    fn a_24_gives_253_hosts_without_self() {
        let hosts = candidate_hosts_for(&[iface([192, 168, 1, 10], [255, 255, 255, 0])]);
        assert_eq!(hosts.len(), 253);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(*hosts.last().unwrap(), Ipv4Addr::new(192, 168, 1, 254));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 10)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 255)));
    }

    #[test]
    fn big_networks_are_clamped_to_the_surrounding_24() {
        let hosts = candidate_hosts_for(&[iface([10, 1, 2, 3], [255, 0, 0, 0])]);
        assert_eq!(hosts.len(), 253);
        assert!(hosts.iter().all(|h| h.octets()[..3] == [10, 1, 2]));
    }

    #[test]
    fn small_networks_stay_small() {
        let hosts = candidate_hosts_for(&[iface([192, 168, 5, 9], [255, 255, 255, 248])]);
        // 192.168.5.8/29: .9 .. .14 usable, minus self.
        assert_eq!(
            hosts,
            [10, 11, 12, 13, 14].map(|o| Ipv4Addr::new(192, 168, 5, o))
        );
        assert!(candidate_hosts_for(&[iface([10, 0, 0, 1], [255, 255, 255, 255])]).is_empty());
    }

    #[test]
    fn loopback_link_local_and_duplicates_are_skipped() {
        let hosts = candidate_hosts_for(&[
            iface([127, 0, 0, 1], [255, 0, 0, 0]),
            iface([169, 254, 3, 4], [255, 255, 0, 0]),
            iface([192, 168, 1, 10], [255, 255, 255, 0]),
            iface([192, 168, 1, 11], [255, 255, 255, 0]),
        ]);
        // Each interface skips only its own address; the union covers both.
        assert_eq!(hosts.len(), 254);
    }

    #[test]
    fn mdns_names_split_into_instance_and_service() {
        assert_eq!(
            split_mdns_fullname("adb-2G0YC5ZG8L0123-AbCdEf._adb-tls-connect._tcp.local."),
            Some((
                "adb-2G0YC5ZG8L0123-AbCdEf".into(),
                "_adb-tls-connect._tcp".into()
            ))
        );
        assert_eq!(
            split_mdns_fullname("quest._adb._tcp.local."),
            Some(("quest".into(), "_adb._tcp".into()))
        );
        assert_eq!(split_mdns_fullname("printer._ipp._tcp.local."), None);
    }

    #[tokio::test]
    async fn probe_finds_a_listening_port_and_scan_reports_it() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe("127.0.0.1", port, Duration::from_secs(2)).await);
        let open = scan_hosts(vec![Ipv4Addr::LOCALHOST], port, ScanOptions::default()).await;
        assert_eq!(open, [Ipv4Addr::LOCALHOST]);
        drop(listener);
        assert!(!probe("not a host name", port, Duration::from_millis(500)).await);
    }

    #[tokio::test]
    #[ignore = "scans the real LAN"]
    async fn scan_the_lan_for_adb() {
        let open = scan_for_adb(5555, ScanOptions::default()).await;
        eprintln!("adb on: {open:?}");
    }

    #[tokio::test]
    #[ignore = "uses multicast on the real network"]
    async fn browse_mdns_for_adb() {
        let found = discover_adb_services(Duration::from_secs(3)).await.unwrap();
        eprintln!("mDNS: {found:?}");
    }
}
