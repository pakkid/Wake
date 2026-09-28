//! Wake-on-LAN magic packets.
//!
//! A magic packet is 6 bytes of `0xFF` followed by the target MAC repeated 16
//! times (102 bytes), sent as a UDP broadcast. The NIC matches the pattern
//! anywhere in the frame, so the UDP port only matters to firewalls; 9
//! ("discard") is the convention.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::str::FromStr;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::net::UdpSocket;

pub const PACKET_LEN: usize = 6 + 16 * 6;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("'{0}' is not a MAC address (expected six hex pairs, e.g. aa:bb:cc:dd:ee:ff)")]
pub struct InvalidMac(pub String);

impl FromStr for MacAddr {
    type Err = InvalidMac;

    /// Accepts `aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff`, `aabb.ccdd.eeff` and
    /// `aabbccddeeff`, in either case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || InvalidMac(s.to_string());
        let trimmed = s.trim();
        let hex: String = trimmed
            .chars()
            .filter(|c| !matches!(c, ':' | '-' | '.'))
            .collect();
        if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(err());
        }
        // Separators, if present, must sit on byte (or Cisco-style word) boundaries.
        let seps: Vec<(usize, char)> = trimmed
            .char_indices()
            .filter(|(_, c)| matches!(c, ':' | '-' | '.'))
            .collect();
        let valid_layout = match seps.len() {
            0 => true,
            5 => {
                seps.iter().all(|&(_, c)| c == seps[0].1 && c != '.')
                    && seps
                        .iter()
                        .enumerate()
                        .all(|(i, &(pos, _))| pos == 2 + i * 3)
            }
            2 => {
                seps.iter().all(|&(_, c)| c == '.')
                    && seps
                        .iter()
                        .enumerate()
                        .all(|(i, &(pos, _))| pos == 4 + i * 5)
            }
            _ => false,
        };
        if !valid_layout {
            return Err(err());
        }
        let mut bytes = [0u8; 6];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| err())?;
        }
        Ok(MacAddr(bytes))
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MacAddr({self})")
    }
}

pub fn magic_packet(mac: &MacAddr) -> [u8; PACKET_LEN] {
    let mut packet = [0xFFu8; PACKET_LEN];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac.0);
    }
    packet
}

/// Anything that can wake the PC. The real implementation broadcasts over UDP;
/// tests substitute a recorder.
pub trait WolSender: Send + Sync {
    fn send(&self, mac: MacAddr) -> BoxFuture<'_, std::io::Result<()>>;
}

pub struct UdpWol {
    target: SocketAddr,
    repeats: u32,
    gap: Duration,
}

impl UdpWol {
    pub fn new(broadcast: Ipv4Addr, port: u16) -> Self {
        Self {
            target: SocketAddr::V4(SocketAddrV4::new(broadcast, port)),
            repeats: 3,
            gap: Duration::from_millis(100),
        }
    }

    pub fn target(&self) -> SocketAddr {
        self.target
    }
}

impl WolSender for UdpWol {
    fn send(&self, mac: MacAddr) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            let packet = magic_packet(&mac);
            let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
            socket.set_broadcast(true)?;
            // UDP is lossy and a sleeping NIC only needs one copy, so send a few.
            for i in 0..self.repeats {
                if i > 0 {
                    tokio::time::sleep(self.gap).await;
                }
                let sent = socket.send_to(&packet, self.target).await?;
                if sent != PACKET_LEN {
                    return Err(std::io::Error::other(format!(
                        "short send: {sent} of {PACKET_LEN} bytes"
                    )));
                }
            }
            tracing::info!(%mac, target = %self.target, repeats = self.repeats, "sent magic packet");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: MacAddr = MacAddr([0x00, 0x11, 0x22, 0xaa, 0xbb, 0xcc]);

    #[test]
    fn parses_common_formats() {
        for s in [
            "00:11:22:aa:bb:cc",
            "00-11-22-AA-BB-CC",
            "0011.22aa.bbcc",
            "001122aabbcc",
            "  00:11:22:AA:bb:cc ",
        ] {
            assert_eq!(s.parse::<MacAddr>(), Ok(MAC), "{s}");
        }
    }

    #[test]
    fn rejects_bad_macs() {
        for s in [
            "",
            "00:11:22:aa:bb",
            "00:11:22:aa:bb:cc:dd",
            "00:11:22:aa:bb:zz",
            "0:11:22:aa:bb:cc0",
            "00:11-22:aa:bb:cc",
            "00.11.22.aa.bb.cc",
            "001122aabbc",
        ] {
            assert!(s.parse::<MacAddr>().is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn displays_lowercase_colons() {
        assert_eq!(MAC.to_string(), "00:11:22:aa:bb:cc");
    }

    #[test]
    fn magic_packet_layout() {
        let p = magic_packet(&MAC);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for rep in p[6..].chunks(6) {
            assert_eq!(rep, &MAC.0);
        }
    }

    #[tokio::test]
    async fn sends_packets_over_udp() {
        let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = rx.local_addr().unwrap().port();
        let wol = UdpWol {
            target: SocketAddr::from(([127, 0, 0, 1], port)),
            repeats: 2,
            gap: Duration::from_millis(1),
        };
        wol.send(MAC).await.unwrap();
        let mut buf = [0u8; 256];
        for _ in 0..2 {
            let n = tokio::time::timeout(Duration::from_secs(2), rx.recv(&mut buf))
                .await
                .expect("packet arrives")
                .unwrap();
            assert_eq!(&buf[..n], &magic_packet(&MAC)[..]);
        }
    }
}
