//! Execution-scoped HTTP/CONNECT egress for approval-enabled policies, and
//! for static allowlists on backends whose rules take IP literals only.
//! The OS backend allows only this listener. Policy and DNS checks run outside
//! the sandbox; environment variables alone are never the enforcement boundary.
use crate::error::{Error, Result};
use crate::policy::{NetworkMode, Policy};
use crate::protocol::{ExecRequest, NetworkDenial};
use base64::Engine;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub fn normalize_target(value: &str) -> Result<String> {
    let (host, port) = split_target(value)?;
    Ok(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

/// The address and port of an exact target whose host is an IP literal;
/// None for a hostname.
pub fn target_ip(target: &str) -> Result<Option<(IpAddr, u16)>> {
    let (host, port) = split_target(target)?;
    Ok(host.parse::<IpAddr>().ok().map(|ip| (ip, port)))
}

fn split_target(value: &str) -> Result<(String, u16)> {
    let invalid =
        || Error::InvalidPolicy(format!("expected exact host:port network target: {value}"));
    if value.len() > 300 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid());
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(invalid)?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(invalid());
        }
        (host, port)
    } else {
        let (host, port) = value.rsplit_once(':').ok_or_else(invalid)?;
        if host.is_empty()
            || host.contains(':')
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        {
            return Err(invalid());
        }
        (host, port)
    };
    let port: u16 = port.parse().map_err(|_| invalid())?;
    if port == 0 {
        return Err(invalid());
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(invalid());
    }
    Ok((host, port))
}

struct Shared {
    stop: AtomicBool,
    active: AtomicUsize,
    sockets: Mutex<Vec<TcpStream>>,
    denials: Mutex<Vec<NetworkDenial>>,
    allow: Vec<String>,
    deny: Vec<String>,
    allow_private: bool,
    /// A listed IP literal is reached even when private: the policy author
    /// wrote it, and a literal cannot be rebound the way a hostname can.
    trust_literals: bool,
    token: String,
}

impl Shared {
    fn track(&self, stream: &TcpStream) -> Result<()> {
        let mut sockets = self.sockets.lock().unwrap();
        if self.stop.load(Ordering::Acquire) {
            let _ = stream.shutdown(Shutdown::Both);
            return Err(Error::Backend("network grant expired".into()));
        }
        if sockets.len() >= 8192 {
            return Err(Error::Backend("command proxy connection budget exceeded".into()));
        }
        sockets.push(stream.try_clone()?);
        Ok(())
    }

    fn denied(&self, target: &str, reason: &str) {
        let mut events = self.denials.lock().unwrap();
        if events.len() < 64
            && !events
                .iter()
                .any(|v| v.target == target && v.reason == reason)
        {
            events.push(NetworkDenial {
                target: target.into(),
                reason: reason.into(),
            });
        }
    }
}

pub struct NetworkProxy {
    port: u16,
    shared: Arc<Shared>,
    listener: Option<JoinHandle<()>>,
}

impl NetworkProxy {
    /// Replace the sandbox's destination rules with a single loopback exit.
    /// Preserve the original policy separately in the owning backend.
    pub fn prepare(policy: &mut Policy) -> Result<Option<Self>> {
        if !policy.network.allow_approval {
            return Ok(None);
        }
        policy.validate()?;
        Self::start(policy, false).map(Some)
    }

    /// A proxy for a static allowlist with entries the OS backend cannot
    /// express as a rule of its own; `direct` says which entries it can.
    /// Hostnames stay behind the private-address guard; listed IP literals
    /// are trusted as written. None when every entry is direct or the policy
    /// is not a static allowlist.
    pub fn prepare_static(policy: &mut Policy, direct: &dyn Fn(&str) -> bool) -> Result<Option<Self>> {
        if policy.network.mode != NetworkMode::Allowlist || policy.network.allow_approval {
            return Ok(None);
        }
        for entry in &policy.network.allow {
            target_ip(entry)?;
        }
        if policy.network.allow.iter().all(|entry| direct(entry)) {
            return Ok(None);
        }
        policy.validate()?;
        Self::start(policy, true).map(Some)
    }

    fn start(policy: &mut Policy, trust_literals: bool) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            sockets: Mutex::new(Vec::new()),
            denials: Mutex::new(Vec::new()),
            allow: policy
                .network
                .allow
                .iter()
                .map(|v| normalize_target(v))
                .collect::<Result<_>>()?,
            deny: policy
                .network
                .deny
                .iter()
                .map(|v| normalize_target(v))
                .collect::<Result<_>>()?,
            allow_private: policy.network.allow_private,
            trust_literals,
            token: uuid::Uuid::new_v4().simple().to_string(),
        });
        let state = Arc::clone(&shared);
        let handle = std::thread::spawn(move || {
            let mut connections: Vec<JoinHandle<()>> = Vec::new();
            while !state.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if state.active.fetch_add(1, Ordering::AcqRel) >= 32 {
                            state.active.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        let state = Arc::clone(&state);
                        if state.track(&stream).is_err() {
                            state.active.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        connections.push(std::thread::spawn(move || {
                            let _ = serve(stream, &state);
                            state.active.fetch_sub(1, Ordering::AcqRel);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
                let mut i = 0;
                while i < connections.len() {
                    if connections[i].is_finished() {
                        let _ = connections.swap_remove(i).join();
                    } else {
                        i += 1;
                    }
                }
            }
            for handle in connections {
                let _ = handle.join();
            }
        });
        policy.network.mode = NetworkMode::Allowlist;
        policy.network.allow = vec![format!("127.0.0.1:{port}")];
        // This derived policy is only handed to the OS backend, not re-proxied.
        policy.network.allow_approval = false;
        policy.network.deny.clear();
        policy.network.allow_private = false;
        Ok(Self {
            port,
            shared,
            listener: Some(handle),
        })
    }

    pub fn environment(&self, request: &mut ExecRequest) {
        let env = request.env.get_or_insert_with(Vec::new);
        let url = format!(
            "http://deltabox:{}@127.0.0.1:{}",
            self.shared.token, self.port
        );
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.retain(|(k, _)| k != key);
            env.push((key.into(), url.clone()));
        }
        for key in ["NO_PROXY", "no_proxy"] {
            env.retain(|(k, _)| k != key);
            env.push((key.into(), String::new()));
        }
    }

    pub fn denials(&self) -> Vec<NetworkDenial> {
        self.shared.denials.lock().unwrap().clone()
    }
}

impl Drop for NetworkProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        for socket in self.shared.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

/// Copy until EOF, an error, or the proxy stops. Reads time out briefly so a
/// stop is noticed even where `shutdown` does not wake a blocked read, as on
/// Windows; otherwise dropping the proxy could wait on a tunnel forever.
fn relay(from: &mut TcpStream, to: &mut TcpStream, shared: &Shared) {
    let _ = from.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buffer = [0u8; 16384];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if to.write_all(&buffer[..count]).is_err() {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn serve(mut client: TcpStream, shared: &Arc<Shared>) -> Result<()> {
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    // Read exactly one header block, preserving any tunnel payload in the socket.
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 32768 {
            return Err(Error::Backend("proxy headers too large".into()));
        }
        let mut byte = [0];
        client.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let head =
        std::str::from_utf8(&header).map_err(|_| Error::Backend("invalid proxy headers".into()))?;
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("deltabox:{}", shared.token))
    );
    if !head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .any(|(key, value)| key.eq_ignore_ascii_case("proxy-authorization") && value.trim() == auth)
    {
        client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let first = head.lines().next().unwrap_or_default();
    let parts: Vec<_> = first.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(Error::Backend("invalid proxy request".into()));
    }
    let tunnel = parts[0].eq_ignore_ascii_case("CONNECT");
    let (target, path) = if tunnel {
        (parts[1].to_string(), String::new())
    } else {
        let rest = parts[1]
            .strip_prefix("http://")
            .ok_or_else(|| Error::Backend("proxy expects HTTP or CONNECT".into()))?;
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..split];
        let target = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        let path = if split == rest.len() {
            "/".into()
        } else if rest.as_bytes()[split] == b'?' {
            format!("/{}", &rest[split..])
        } else {
            rest[split..].into()
        };
        (target, path)
    };
    let target = normalize_target(&target)?;
    let (host, port) = split_target(&target)?;
    let reason = if shared.deny.contains(&target) {
        Some("policy-denied")
    } else if !shared.allow.contains(&target) {
        Some("not-authorized")
    } else {
        None
    };
    if let Some(reason) = reason {
        shared.denied(&target, reason);
        client.write_all(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let addresses: Vec<_> = (host.as_str(), port).to_socket_addrs()?.collect();
    let trusted_literal = shared.trust_literals && host.parse::<IpAddr>().is_ok();
    if !shared.allow_private && !trusted_literal && addresses.iter().any(|addr| !public_ip(addr.ip())) {
        shared.denied(&target, "private-address");
        client.write_all(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    // Connect to the checked address, never resolve the hostname a second time.
    let mut upstream = None;
    for address in addresses {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            upstream = Some(stream);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        client.write_all(
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    };
    shared.track(&upstream)?;
    if tunnel {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    } else {
        // Each proxy connection carries one HTTP request. Forward the origin
        // authority ourselves and close it after the response, so a second
        // absolute request cannot reuse a previously authorized connection.
        write!(
            upstream,
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            parts[0], path, target
        )?;
        for line in head.lines().skip(1) {
            if line.is_empty() {
                continue;
            }
            let (key, _) = line
                .split_once(':')
                .ok_or_else(|| Error::Backend("invalid HTTP header".into()))?;
            if [
                "proxy-authorization",
                "proxy-connection",
                "connection",
                "host",
            ]
            .iter()
            .any(|v| key.eq_ignore_ascii_case(v))
            {
                continue;
            }
            write!(upstream, "{line}\r\n")?;
        }
        upstream.write_all(b"\r\n")?;
    }
    client.set_write_timeout(None)?;
    let mut downstream = client.try_clone()?;
    let mut upload = upstream.try_clone()?;
    let state = Arc::clone(shared);
    let forward = std::thread::spawn(move || {
        relay(&mut client, &mut upload, &state);
        let _ = upload.shutdown(Shutdown::Write);
    });
    relay(&mut upstream, &mut downstream, shared);
    let _ = downstream.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = forward.join();
    Ok(())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && a != 0
                && a < 240
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            // Global unicast only; excludes loopback, ULA, link-local,
            // multicast, unspecified, and IPv4 translation ranges.
            (ip.segments()[0] & 0xe000) == 0x2000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proxy(allow: Vec<String>, deny: Vec<String>, allow_private: bool) -> NetworkProxy {
        let mut policy = Policy::default_agent();
        policy.network.mode = NetworkMode::Allowlist;
        policy.network.allow_approval = true;
        policy.network.allow = allow;
        policy.network.deny = deny;
        policy.network.allow_private = allow_private;
        NetworkProxy::prepare(&mut policy).unwrap().unwrap()
    }

    fn connect(proxy: &NetworkProxy, target: &str, authenticated: bool) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let auth = if authenticated { format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(format!("deltabox:{}", proxy.shared.token))) } else { String::new() };
        write!(stream, "CONNECT {target} HTTP/1.1\r\n{auth}\r\n").unwrap();
        stream
    }

    fn response(stream: &mut TcpStream) -> String {
        let mut out = Vec::new();
        while !out.ends_with(b"\r\n\r\n") {
            let mut b = [0]; stream.read_exact(&mut b).unwrap(); out.push(b[0]);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn proxy_auth_denials_private_guard_and_deny_precedence() {
        let proxy = proxy(vec!["localhost:443".into(), "blocked.example:443".into()], vec!["blocked.example:443".into()], false);
        assert!(response(&mut connect(&proxy, "unknown.example:443", false)).contains("407"));
        for (target, reason) in [("unknown.example:443", "not-authorized"), ("localhost:443", "private-address"), ("blocked.example:443", "policy-denied")] {
            assert!(response(&mut connect(&proxy, target, true)).contains("403"));
            assert!(proxy.denials().iter().any(|d| d.target == target && d.reason == reason));
        }
        assert_eq!(proxy.denials().len(), 3);
    }

    #[test]
    fn tunnel_expires_and_old_credentials_cannot_reach_a_new_execution() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let target = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
        let proxy = proxy(vec![target.clone()], Vec::new(), true);
        let port = proxy.port;
        let mut client = connect(&proxy, &target, true);
        assert!(response(&mut client).contains("200"));
        let (mut server, _) = origin.accept().unwrap();
        client.write_all(b"ping").unwrap();
        let mut data = [0; 4]; server.read_exact(&mut data).unwrap(); assert_eq!(&data, b"ping");
        drop(proxy);
        assert!(client.read(&mut data).map_or(true, |n| n == 0));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
        let denied = self::proxy(Vec::new(), Vec::new(), true);
        assert!(response(&mut connect(&denied, &target, true)).contains("403"));
    }

    #[test]
    fn static_allowlists_proxy_hostnames_and_trust_listed_literals() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = origin.local_addr().unwrap().port();
        let literal = |entry: &str| target_ip(entry).ok().flatten().is_some();
        let mut policy = Policy::default_agent();
        policy.network.mode = NetworkMode::Allowlist;
        policy.network.allow = vec![format!("127.0.0.1:{port}")];
        assert!(NetworkProxy::prepare_static(&mut policy, &literal).unwrap().is_none(), "direct entries alone need no proxy");
        policy.network.allow.push(format!("localhost:{port}"));
        let proxy = NetworkProxy::prepare_static(&mut policy, &literal).unwrap().unwrap();
        assert_eq!(policy.network.allow, vec![format!("127.0.0.1:{}", proxy.port)], "the OS backend sees only the proxy");
        assert!(response(&mut connect(&proxy, &format!("localhost:{port}"), true)).contains("403"));
        assert!(proxy.denials().iter().any(|d| d.reason == "private-address"));
        assert!(response(&mut connect(&proxy, &format!("127.0.0.1:{port}"), true)).contains("200"));
        assert!(origin.accept().is_ok());
        assert!(response(&mut connect(&proxy, &format!("127.0.0.1:{}", port.wrapping_add(1).max(1)), true)).contains("403"));
        policy.network.allow_approval = true;
        assert!(NetworkProxy::prepare_static(&mut policy, &literal).unwrap().is_none(), "approval mode uses prepare");
    }

    #[test]
    fn exact_targets_and_private_addresses() {
        assert_eq!(
            normalize_target("EXAMPLE.com.:0443").unwrap(),
            "example.com:443"
        );
        for v in [
            "*.example.com:443",
            "example.com",
            "example.com:0",
            "x:443/path",
            "user@x:443",
            "x:443\n",
            "[garbage]:80",
        ] {
            assert!(normalize_target(v).is_err(), "{v}");
        }
        for v in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
        ] {
            assert!(!public_ip(v.parse().unwrap()), "{v}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
    }
}
