//! Who may talk to this PC (the Security tab): allowed networks and allowed
//! PCs. On top of the pinned identities: these decide who may connect at all.

use std::net::IpAddr;
use std::str::FromStr;

/// A network such as `192.168.1.0/24`; a bare address means just that one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subnet {
    addr: IpAddr,
    prefix: u8,
}

fn bits(ip: IpAddr) -> (u128, u8) {
    match ip.to_canonical() {
        IpAddr::V4(a) => (u32::from(a) as u128, 32),
        IpAddr::V6(a) => (u128::from(a), 128),
    }
}

impl Subnet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ((net, width), (addr, other)) = (bits(self.addr), bits(ip));
        if width != other {
            return false;
        }
        let shift = (width - self.prefix.min(width)) as u32;
        if shift >= 128 {
            return true;
        }
        net >> shift == addr >> shift
    }
}

impl FromStr for Subnet {
    type Err = String;
    fn from_str(s: &str) -> Result<Subnet, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| format!("'{s}' is not a network like 192.168.1.0/24"))?;
        let width = bits(addr).1;
        let prefix = match prefix {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= width).ok_or(format!("'{s}': the number after / must be 0..={width}"))?,
            None => width,
        };
        Ok(Subnet { addr: addr.to_canonical(), prefix })
    }
}

/// The Security tab's rules. Empty lists don't restrict.
#[derive(Clone, Debug, Default)]
pub struct Access {
    pub networks: Vec<Subnet>,
    /// PC names (any case) or addresses / networks.
    pub allowed: Vec<String>,
}

impl Access {
    /// May `name` at `ip` talk to us? Otherwise why not.
    pub fn check(&self, name: &str, ip: IpAddr) -> Result<(), String> {
        let ip = ip.to_canonical();
        if !self.networks.is_empty() && !self.networks.iter().any(|n| n.contains(ip)) {
            return Err(format!("{name} ({ip}) is outside the allowed networks"));
        }
        let listed = |entry: &String| {
            entry.trim().eq_ignore_ascii_case(name) || entry.parse::<Subnet>().is_ok_and(|n| n.contains(ip))
        };
        if !self.allowed.is_empty() && !self.allowed.iter().any(listed) {
            return Err(format!("{name} ({ip}) is not in the allowed PCs"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn networks_and_single_addresses() {
        let lan: Subnet = "192.168.1.0/24".parse().unwrap();
        assert!(lan.contains(ip("192.168.1.175")));
        assert!(!lan.contains(ip("192.168.2.1")));
        // IPv4 seen through a dual-stack socket.
        assert!(lan.contains(ip("::ffff:192.168.1.9")));
        let one: Subnet = "10.0.0.5".parse().unwrap();
        assert!(one.contains(ip("10.0.0.5")) && !one.contains(ip("10.0.0.6")));
        assert!("0.0.0.0/0".parse::<Subnet>().unwrap().contains(ip("8.8.8.8")));
        assert!("192.168.1.0/33".parse::<Subnet>().is_err());
        assert!("flash".parse::<Subnet>().is_err());
    }

    #[test]
    fn access_rules() {
        let open = Access::default();
        assert!(open.check("anyone", ip("1.2.3.4")).is_ok());
        let a = Access { networks: vec!["192.168.1.0/24".parse().unwrap()], allowed: vec!["FlashUnreal".into(), "192.168.1.50".into()] };
        assert!(a.check("flashunreal", ip("192.168.1.175")).is_ok(), "by name, any case");
        assert!(a.check("laptop", ip("192.168.1.50")).is_ok(), "by address");
        assert!(a.check("laptop", ip("192.168.1.51")).unwrap_err().contains("not in the allowed PCs"));
        assert!(a.check("flashunreal", ip("10.0.0.1")).unwrap_err().contains("outside the allowed networks"));
    }
}
