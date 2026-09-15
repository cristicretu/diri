//! Test-only Linux netem control, confined to a fresh user/network namespace.
//! Wire layouts: Linux UAPI linux/rtnetlink.h and linux/pkt_sched.h.
//! https://github.com/torvalds/linux/blob/master/include/uapi/linux/pkt_sched.h
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

fn attribute(bytes: &mut Vec<u8>, kind: u16, value: &[u8]) {
    bytes.extend(((value.len() + 4) as u16).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes());
    bytes.extend(value);
    bytes.resize(bytes.len().next_multiple_of(4), 0);
}

fn request(kind: u16, flags: u16, body: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    // SAFETY: socket returns a fresh descriptor, owned exactly once below.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut packet = Vec::new();
    packet.extend(((16 + body.len()) as u32).to_ne_bytes());
    packet.extend(kind.to_ne_bytes());
    packet.extend((flags | 1).to_ne_bytes()); // REQUEST
    packet.extend(1_u32.to_ne_bytes());
    packet.extend(0_u32.to_ne_bytes());
    packet.extend(body);
    // SAFETY: zero is valid for sockaddr_nl; only family needs a nonzero value.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as u16;
    let result = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            packet.as_ptr().cast(),
            packet.len(),
            0,
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut messages = Vec::new();
    loop {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 5000) } <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "netlink reply timeout",
            ));
        }
        let mut bytes = [0_u8; 32768];
        let count =
            unsafe { libc::recv(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut rest = &bytes[..count as usize];
        while rest.len() >= 16 {
            let len = u32::from_ne_bytes(rest[..4].try_into().unwrap()) as usize;
            if len < 16 || len > rest.len() {
                return Err(io::Error::other("invalid netlink length"));
            }
            let kind = u16::from_ne_bytes(rest[4..6].try_into().unwrap());
            if kind == 3 {
                return Ok(messages);
            } // DONE
            if kind == 2 {
                // ERROR/ACK
                if len < 20 {
                    return Err(io::Error::other("short netlink ACK"));
                }
                let error = i32::from_ne_bytes(rest[16..20].try_into().unwrap());
                return if error == 0 {
                    Ok(messages)
                } else {
                    Err(io::Error::from_raw_os_error(-error))
                };
            }
            messages.push(rest[16..len].to_vec());
            rest = rest.get(len.next_multiple_of(4)..).unwrap_or_default();
        }
    }
}

fn tc_message() -> Vec<u8> {
    let mut body = vec![0_u8; 20];
    body[4..8].copy_from_slice(&1_i32.to_ne_bytes()); // isolated loopback
    body[8..12].copy_from_slice(&(1_u32 << 16).to_ne_bytes());
    body[12..16].copy_from_slice(&u32::MAX.to_ne_bytes()); // TC_H_ROOT
    body
}

pub fn configure(delay_ms: u32, jitter_ms: u32, loss_percent: u32) {
    // Fail before any mutation unless both namespaces are isolated and lo is
    // the only interface. Never shape the host or a developer's real SSH link.
    for ns in ["net", "user"] {
        let inherited = std::env::var(format!("DIRI_NETEM_PARENT_{}", ns.to_uppercase()))
            .expect("namespace test launcher");
        assert_ne!(
            fs::read_link(format!("/proc/self/ns/{ns}"))
                .unwrap()
                .to_string_lossy(),
            inherited,
            "refusing to shape the caller's namespace"
        );
    }
    let interfaces: Vec<_> = fs::read_to_string("/proc/net/dev")
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
        .collect();
    assert_eq!(interfaces, ["lo"]);
    let mut link = vec![0_u8; 16];
    link[4..8].copy_from_slice(&1_i32.to_ne_bytes());
    link[8..12].copy_from_slice(&1_u32.to_ne_bytes()); // IFF_UP
    link[12..16].copy_from_slice(&1_u32.to_ne_bytes());
    attribute(&mut link, 4, &1500_u32.to_ne_bytes()); // IFLA_MTU
    request(16, 4, &link).expect("bring isolated loopback up");
    let mut body = tc_message();
    attribute(&mut body, 1, b"netem\0");
    let mut options = vec![0_u8; 24];
    options[4..8].copy_from_slice(&1000_u32.to_ne_bytes());
    options[8..12]
        .copy_from_slice(&((u32::MAX as u64 * loss_percent as u64 / 100) as u32).to_ne_bytes());
    attribute(
        &mut options,
        10,
        &(i64::from(delay_ms) * 1_000_000).to_ne_bytes(),
    );
    attribute(
        &mut options,
        11,
        &(i64::from(jitter_ms) * 1_000_000).to_ne_bytes(),
    );
    attribute(&mut body, 2, &options);
    request(36, 4 | 0x400 | 0x100, &body).expect("install isolated netem qdisc");
}

fn attributes(mut bytes: &[u8]) -> Vec<(u16, &[u8])> {
    let mut result = Vec::new();
    while bytes.len() >= 4 {
        let len = u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize;
        assert!(len >= 4 && len <= bytes.len());
        result.push((
            u16::from_ne_bytes(bytes[2..4].try_into().unwrap()) & 0x3fff,
            &bytes[4..len],
        ));
        bytes = bytes.get(len.next_multiple_of(4)..).unwrap_or_default();
    }
    result
}

pub fn drops() -> u32 {
    for message in request(38, 0x300, &tc_message()).expect("read netem counters") {
        for (kind, value) in attributes(&message[20..]) {
            if kind == 7 {
                // TCA_STATS2
                for (kind, value) in attributes(value) {
                    if kind == 3 {
                        // TCA_STATS_QUEUE: qlen, backlog, drops
                        return u32::from_ne_bytes(value[8..12].try_into().unwrap());
                    }
                }
            }
        }
    }
    panic!("netem drop counter missing");
}

/// Enter a new ordinary-user namespace before running the fixture. Capturing
/// our own namespace IDs avoids requiring access to root-owned /proc/1/ns.
pub fn enter_fixture() -> bool {
    if std::env::var_os("DIRI_NETEM_PARENT_NET").is_some() {
        return true;
    }
    let mut command = std::process::Command::new("unshare");
    command
        .args(["--user", "--map-root-user", "--net"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "impaired_tcp_preserves_input_history_and_reconnect",
            "--ignored",
            "--nocapture",
        ]);
    for ns in ["net", "user"] {
        command.env(
            format!("DIRI_NETEM_PARENT_{}", ns.to_uppercase()),
            fs::read_link(format!("/proc/self/ns/{ns}")).unwrap(),
        );
    }
    assert!(
        command.status().expect("unshare executable").success(),
        "isolated network fixture failed"
    );
    false
}
