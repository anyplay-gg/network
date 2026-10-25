//! `anyplay-probe`: phase 0 of the network. Two people on different networks run it, one with
//! `listen` and one with `connect`, and get the numbers the tunnel design rests on: does the
//! tunnel go direct or through a relay, how long it takes to get there, and the round trip,
//! loss and throughput a game would see.
//!
//! Without `--relay` it uses the relays the iroh project runs for everyone, because the
//! network's own relays do not exist yet. Only probe packets cross them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr};
use serde::{Deserialize, Serialize};

const ALPN: &[u8] = b"anyplay/probe/0";
const USAGE: &str = "\
anyplay-probe: measures a tunnel between two networks

  anyplay-probe listen [--relay <url>] [--relay-only] [--key <file>]
      prints a ticket and answers probes until stopped; with --key the identity is kept in
      the file (created with mode 0600 when missing), so the ticket survives restarts
  anyplay-probe connect <ticket> [--relay <url>] [--relay-only] [--seconds <n>]
                                 [--rate <per second>] [--size <bytes>] [--bulk <MiB>] [--json]
      measures the tunnel to the listener (defaults: 20 s, 60 per second, 160 bytes, 8 MiB)

Run listen on one network and connect on another, for example at home and on a phone's
hotspot. Without --relay, the relays of the iroh project are used. --relay-only on either side
keeps the tunnel on the relay, to measure the relayed path. While a relay is reached, the
ticket names no IP address; the two sides find a direct path through the relay.
";

#[derive(Serialize, Deserialize)]
struct Ticket {
    id: String,
    relay: Option<String>,
    addrs: Vec<String>,
}

#[derive(Default)]
struct Options {
    relay: Option<RelayUrl>,
    seconds: u64,
    rate: u32,
    size: usize,
    bulk: usize,
    json: bool,
    relay_only: bool,
    key: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut args = std::env::args().skip(1);
    let mode = args.next();
    let mut ticket = None;
    let mut options = Options {
        seconds: 20,
        rate: 60,
        size: 160,
        bulk: 8,
        ..Options::default()
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_default();
        match arg.as_str() {
            "--relay" => options.relay = value().parse().ok(),
            "--seconds" => options.seconds = value().parse().unwrap_or(20).clamp(1, 3600),
            "--rate" => options.rate = value().parse().unwrap_or(60).clamp(1, 1000),
            "--size" => options.size = value().parse().unwrap_or(160).clamp(16, 1100),
            "--bulk" => options.bulk = value().parse().unwrap_or(8).min(1024),
            "--json" => options.json = true,
            "--relay-only" => options.relay_only = true,
            "--key" => {
                options.key = Some(PathBuf::from(value())).filter(|p| !p.as_os_str().is_empty())
            }
            other if ticket.is_none() && !other.starts_with("--") => {
                ticket = Some(other.to_owned())
            }
            _ => {
                eprint!("{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    let result = match (mode.as_deref(), ticket) {
        (Some("listen"), None) => listen(&options).await,
        (Some("connect"), Some(ticket)) => connect(&ticket, &options).await,
        _ => {
            eprint!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn endpoint(options: &Options, accept: bool) -> Result<Endpoint, String> {
    let relays = match &options.relay {
        Some(url) => RelayMode::Custom(RelayMap::from(url.clone())),
        None => RelayMode::Default,
    };
    let mut builder = Endpoint::builder(presets::Minimal).relay_mode(relays);
    if let Some(path) = &options.key {
        builder = builder.secret_key(load_or_create_key(path)?);
    }
    if accept {
        builder = builder.alpns(vec![ALPN.to_vec()]);
    }
    if options.relay_only {
        builder = builder.clear_ip_transports();
    }
    let endpoint = builder.bind().await.map_err(|e| e.to_string())?;
    if tokio::time::timeout(Duration::from_secs(15), endpoint.online())
        .await
        .is_err()
    {
        eprintln!("warning: no relay reached in 15 s; only direct paths can work");
    }
    Ok(endpoint)
}

/// Reads the secret key from `path` (64 hex digits), or creates it there when the file is
/// missing. The node id, and with it the ticket, then stays the same across restarts.
fn load_or_create_key(path: &Path) -> Result<SecretKey, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let bytes: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE
                .decode(text.trim().as_bytes())
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| format!("{} is not a key file (64 hex digits)", path.display()))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let mut file = std::fs::OpenOptions::new();
            file.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
            let mut file = file
                .open(path)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            let line = format!("{}\n", data_encoding::HEXLOWER.encode(&key.to_bytes()));
            std::io::Write::write_all(&mut file, line.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(key)
        }
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

async fn listen(options: &Options) -> Result<(), String> {
    let endpoint = endpoint(options, true).await?;
    let addr = endpoint.addr();
    let relay = addr.relay_urls().next().map(|u| u.to_string());
    // Addresses only when no relay can introduce the two sides, so a ticket can be posted
    // without the listener's IP.
    let addrs = match relay {
        Some(_) => Vec::new(),
        None => addr.ip_addrs().map(|a| a.to_string()).collect(),
    };
    let ticket = Ticket {
        id: addr.id.to_string(),
        relay,
        addrs,
    };
    let encoded = data_encoding::BASE64URL_NOPAD.encode(
        serde_json::to_string(&ticket)
            .unwrap_or_default()
            .as_bytes(),
    );
    println!("Give this ticket to the other side:\n\n{encoded}\n");
    println!("Waiting for probes. Stop with Ctrl-C.");
    let serving = async {
        while let Some(incoming) = endpoint.accept().await {
            tokio::spawn(async move {
                let Ok(accepting) = incoming.accept() else {
                    return;
                };
                let Ok(connection) = accepting.await else {
                    return;
                };
                println!("probe from {}", connection.remote_id().fmt_short());
                answer(connection).await;
            });
        }
    };
    tokio::select! {
        () = serving => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    endpoint.close().await;
    Ok(())
}

/// Echoes datagrams and swallows bulk streams, answering with what arrived.
async fn answer(connection: Connection) {
    let echo = {
        let connection = connection.clone();
        async move {
            while let Ok(datagram) = connection.read_datagram().await {
                let _ = connection.send_datagram(datagram);
            }
        }
    };
    let bulk = {
        let connection = connection.clone();
        async move {
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                tokio::spawn(async move {
                    let mut total = 0u64;
                    let mut buf = vec![0u8; 64 * 1024];
                    while let Ok(Some(n)) = recv.read(&mut buf).await {
                        total += n as u64;
                    }
                    let _ = send.write_all(&total.to_le_bytes()).await;
                    let _ = send.finish();
                });
            }
        }
    };
    tokio::join!(echo, bulk);
}

#[derive(Serialize, Default)]
struct Report {
    connect_ms: f64,
    first_path: String,
    direct_after_ms: Option<f64>,
    relay_share: f64,
    sent: u32,
    received: u32,
    loss_percent: f64,
    rtt_ms: Quantiles,
    rtt_direct_ms: Option<Quantiles>,
    rtt_relay_ms: Option<Quantiles>,
    jitter_ms: f64,
    bulk_mib: usize,
    throughput_mbit: Option<f64>,
}

#[derive(Serialize, Default, Clone, Copy)]
struct Quantiles {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn quantiles(sorted: &[f64]) -> Option<Quantiles> {
    if sorted.is_empty() {
        return None;
    }
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    Some(Quantiles {
        p50: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: sorted[sorted.len() - 1],
    })
}

/// True while the connection sends on a relay path.
fn relayed(connection: &Connection) -> Option<bool> {
    let paths = connection.paths();
    let selected = paths.iter().find(|p| p.is_selected())?;
    Some(selected.is_relay())
}

async fn connect(ticket: &str, options: &Options) -> Result<(), String> {
    let decoded = data_encoding::BASE64URL_NOPAD
        .decode(ticket.trim().as_bytes())
        .map_err(|_| "the ticket is not readable")?;
    let ticket: Ticket =
        serde_json::from_slice(&decoded).map_err(|_| "the ticket is not readable")?;
    let id = ticket
        .id
        .parse()
        .map_err(|_| "the ticket names no endpoint")?;
    let mut addrs: Vec<TransportAddr> = ticket
        .addrs
        .iter()
        .filter_map(|a| a.parse().ok())
        .map(TransportAddr::Ip)
        .collect();
    if let Some(relay) = ticket.relay.as_deref().and_then(|r| r.parse().ok()) {
        addrs.push(TransportAddr::Relay(relay));
    }
    let endpoint = endpoint(options, false).await?;

    let started = Instant::now();
    let connection = tokio::time::timeout(
        Duration::from_secs(30),
        endpoint.connect(EndpointAddr::from_parts(id, addrs), ALPN),
    )
    .await
    .map_err(|_| "no connection in 30 s")?
    .map_err(|e| e.to_string())?;
    let mut report = Report {
        connect_ms: ms(started.elapsed()),
        first_path: match relayed(&connection) {
            Some(true) => "relay".into(),
            Some(false) => "direct".into(),
            None => "unknown".into(),
        },
        bulk_mib: options.bulk,
        ..Report::default()
    };
    if !options.json {
        println!(
            "connected in {:.0} ms, first path {}",
            report.connect_ms, report.first_path
        );
    }

    let direct_at = {
        let connection = connection.clone();
        tokio::spawn(async move {
            let mut paths = connection.paths_stream();
            while let Some(list) = paths.next().await {
                if list.iter().any(|p| p.is_selected() && p.is_ip()) {
                    return Some(started.elapsed());
                }
            }
            None
        })
    };

    let sent_at = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        u32,
        (Instant, bool),
    >::new()));
    let received = {
        let connection = connection.clone();
        let sent_at = sent_at.clone();
        tokio::spawn(async move {
            let mut rtts = Vec::new();
            while let Ok(datagram) = connection.read_datagram().await {
                let Some(seq) = datagram
                    .get(..4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                else {
                    continue;
                };
                let sent = sent_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&seq);
                if let Some((at, via_relay)) = sent {
                    rtts.push((ms(at.elapsed()), via_relay));
                }
            }
            rtts
        })
    };

    let total = options.rate.saturating_mul(options.seconds as u32);
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / options.rate as f64));
    let mut relay_sends = 0u32;
    for seq in 0..total {
        tick.tick().await;
        let mut datagram = vec![0u8; options.size];
        datagram[..4].copy_from_slice(&seq.to_le_bytes());
        let via_relay = relayed(&connection).unwrap_or(true);
        relay_sends += via_relay as u32;
        sent_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seq, (Instant::now(), via_relay));
        if connection.send_datagram(datagram.into()).is_err() {
            break;
        }
        if !options.json && seq > 0 && seq % options.rate == 0 {
            let path = if via_relay { "relay" } else { "direct" };
            println!("{:>4} s  {path}", seq / options.rate);
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    report.sent = total;
    report.relay_share = if total == 0 {
        0.0
    } else {
        relay_sends as f64 / total as f64
    };
    report.direct_after_ms = match tokio::time::timeout(Duration::from_millis(10), direct_at).await
    {
        Ok(Ok(Some(at))) => Some(ms(at)),
        _ => None,
    };

    if options.bulk > 0 {
        report.throughput_mbit = bulk(&connection, options.bulk).await;
    }
    connection.close(0u8.into(), b"done");
    let rtts = received.await.unwrap_or_default();
    report.received = rtts.len() as u32;
    report.loss_percent = if total == 0 {
        0.0
    } else {
        100.0 * (total - report.received.min(total)) as f64 / total as f64
    };
    let mut all: Vec<f64> = rtts.iter().map(|(r, _)| *r).collect();
    report.jitter_ms = jitter(&all);
    all.sort_by(f64::total_cmp);
    report.rtt_ms = quantiles(&all).unwrap_or_default();
    let by = |relay: bool| {
        let mut part: Vec<f64> = rtts
            .iter()
            .filter(|(_, r)| *r == relay)
            .map(|(v, _)| *v)
            .collect();
        part.sort_by(f64::total_cmp);
        quantiles(&part)
    };
    report.rtt_direct_ms = by(false);
    report.rtt_relay_ms = by(true);
    endpoint.close().await;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print_report(&report);
    }
    Ok(())
}

/// Sends `mib` MiB on one stream and returns the rate the listener confirms.
async fn bulk(connection: &Connection, mib: usize) -> Option<f64> {
    let (mut send, mut recv) = connection.open_bi().await.ok()?;
    let chunk = vec![0x5Au8; 64 * 1024];
    let started = Instant::now();
    for _ in 0..mib * 16 {
        send.write_all(&chunk).await.ok()?;
    }
    send.finish().ok()?;
    let mut confirmed = [0u8; 8];
    tokio::time::timeout(Duration::from_secs(120), recv.read_exact(&mut confirmed))
        .await
        .ok()?
        .ok()?;
    let bytes = u64::from_le_bytes(confirmed) as f64;
    Some(bytes * 8.0 / started.elapsed().as_secs_f64() / 1_000_000.0)
}

/// The mean difference between consecutive round trips.
fn jitter(rtts: &[f64]) -> f64 {
    if rtts.len() < 2 {
        return 0.0;
    }
    rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (rtts.len() - 1) as f64
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn print_report(report: &Report) {
    let q = |q: &Quantiles| {
        format!(
            "p50 {:.1}  p95 {:.1}  p99 {:.1}  max {:.1} ms",
            q.p50, q.p95, q.p99, q.max
        )
    };
    println!();
    println!(
        "connect        {:.0} ms, first path {}",
        report.connect_ms, report.first_path
    );
    match report.direct_after_ms {
        Some(at) => println!("direct after   {at:.0} ms"),
        None => println!("direct after   never: the tunnel stayed on the relay"),
    }
    println!(
        "on the relay   {:.0} % of the probes",
        report.relay_share * 100.0
    );
    println!(
        "datagrams      {} sent, {} back, {:.2} % lost",
        report.sent, report.received, report.loss_percent
    );
    println!("round trip     {}", q(&report.rtt_ms));
    if let Some(direct) = &report.rtt_direct_ms {
        println!("  direct       {}", q(direct));
    }
    if let Some(relay) = &report.rtt_relay_ms {
        println!("  relay        {}", q(relay));
    }
    println!("jitter         {:.1} ms", report.jitter_ms);
    if let Some(rate) = report.throughput_mbit {
        println!(
            "throughput     {rate:.1} Mbit/s ({} MiB on one stream)",
            report.bulk_mib
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_pick_from_sorted_samples() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let q = quantiles(&samples).unwrap();
        assert_eq!((q.p50, q.p95, q.max), (51.0, 95.0, 100.0));
        assert!(quantiles(&[]).is_none());
    }

    #[test]
    fn jitter_is_the_mean_step() {
        assert_eq!(jitter(&[10.0, 12.0, 11.0]), 1.5);
        assert_eq!(jitter(&[10.0]), 0.0);
    }

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("anyplay-probe-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_key_file_is_created_once_and_reloaded() {
        let path = scratch("create");
        let first = load_or_create_key(&path).unwrap();
        let second = load_or_create_key(&path).unwrap();
        assert_eq!(first.to_bytes(), second.to_bytes());
        assert_eq!(first.public(), second.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_key_file_is_refused_not_replaced() {
        let path = scratch("broken");
        std::fs::write(&path, "not a key").unwrap();
        assert!(load_or_create_key(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_ticket_survives_encoding() {
        let ticket = Ticket {
            id: "ab".into(),
            relay: Some("https://relay.example".into()),
            addrs: vec!["192.0.2.1:4000".into()],
        };
        let text = data_encoding::BASE64URL_NOPAD
            .encode(serde_json::to_string(&ticket).unwrap().as_bytes());
        let back: Ticket = serde_json::from_slice(
            &data_encoding::BASE64URL_NOPAD
                .decode(text.as_bytes())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(back.addrs, ticket.addrs);
    }
}
