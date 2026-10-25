//! `anyplay-net`: started by the launcher, which talks to it over standard input and output.

use std::path::PathBuf;

const USAGE: &str = "\
anyplay-net: the AnyPlay net service

The launcher starts it and talks to it over standard input and output.

Options:
  --data <folder>       where the service keeps its files
  --file-credentials    keep the sign-in in a file instead of the system's credential store
  --relay-ca <file>     also trust the root certificates in this PEM file for relays
  --relay-only          connect to other players only through a relay, so they never learn
                        this PC's addresses
  --version             print the version
";

fn main() -> std::process::ExitCode {
    let mut data = None;
    let mut file_credentials = std::env::var_os("ANYPLAY_NET_FILE_CREDENTIALS").is_some();
    let mut relay_ca = std::env::var_os("ANYPLAY_NET_RELAY_CA").map(PathBuf::from);
    let mut relay_only = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data") => data = args.next().map(PathBuf::from),
            Some("--file-credentials") => file_credentials = true,
            Some("--relay-ca") => relay_ca = args.next().map(PathBuf::from),
            Some("--relay-only") => relay_only = true,
            Some("--version") => {
                println!("anyplay-net {}", env!("CARGO_PKG_VERSION"));
                return std::process::ExitCode::SUCCESS;
            }
            _ => {
                eprint!("{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ANYPLAY_NET_LOG")
                .unwrap_or_else(|_| "info,iroh=warn,noq=warn".into()),
        )
        .init();
    let relay_roots = match relay_ca.as_deref().map(read_roots).transpose() {
        Ok(roots) => roots.unwrap_or_default(),
        Err(error) => {
            tracing::error!("--relay-ca: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let Some(data) = data.or_else(default_data) else {
        tracing::error!("no data folder; pass --data");
        return std::process::ExitCode::FAILURE;
    };
    anyplay_net::install_crypto();
    let config = anyplay_net::Config {
        data,
        file_credentials,
        relay_roots,
        bind: None,
        relay_only,
        device_name: device_name(),
        audio: anyplay_net::voice::Audio::system(),
        workers: anyplay_net::screen::WorkerSet::new(
            anyplay_net::worker_process::ProcessWorkers::new(),
        ),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("anyplay-net")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("no runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(anyplay_net::run(
        config,
        tokio::io::stdin(),
        tokio::io::stdout(),
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
    ));
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `%LOCALAPPDATA%\AnyPlay\net` on Windows, `$XDG_DATA_HOME/anyplay/net` elsewhere.
fn default_data() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("AnyPlay").join("net"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|base| base.join("anyplay").join("net"))
    }
}

/// The certificates of a PEM file.
fn read_roots(
    path: &std::path::Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    use rustls::pki_types::pem::PemObject;
    let roots = rustls::pki_types::CertificateDer::pem_file_iter(path)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if roots.is_empty() {
        return Err("the file holds no certificate".into());
    }
    Ok(roots)
}

/// The computer's name, as the account's device list shows it.
fn device_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "PC".to_owned());
    name.chars().take(64).collect()
}
