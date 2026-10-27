//! The browser sign-in as a native client does it (RFC 8252): the net service listens on a
//! loopback port, the launcher opens the cloud's page in the system browser, and the cloud sends
//! the browser back to the loopback address with a one-time code.
//!
//! The listener answers only `GET /callback` with the right `Host`, and only a request with the
//! `state` this sign-in made counts. Other local programs that touch the port get an error page
//! and change nothing.

use std::time::Duration;

use anyplay_proof::pkce;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How long the player has to finish in the browser.
pub const TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PATH: &str = "/callback";
const MAX_REQUEST: usize = 8 * 1024;

/// What the browser brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Callback {
    /// The one-time code for the token exchange.
    Code(String),
    /// The cloud sent the browser back with an error, such as `access_denied`.
    Denied(String),
    /// Nobody finished in time.
    TimedOut,
}

/// A sign-in waiting for its browser.
pub struct Started {
    /// The page the launcher opens in the system browser.
    pub url: String,
    /// The PKCE verifier for the token exchange. It never leaves this process before then.
    pub verifier: String,
    pub result: oneshot::Receiver<Callback>,
    pub task: JoinHandle<()>,
}

/// Whether `url` is a page a browser may be sent to: http or https, nothing else (no `file:`, no
/// custom protocol).
pub fn is_web_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !lower.contains(char::is_whitespace)
}

/// Opens the listener and builds the URL. `web` is the origin of the account web; `create`
/// starts at "Create account" instead of "Sign in".
pub async fn start(web: &str, create: bool, device_name: &str) -> std::io::Result<Started> {
    start_with(web, create, device_name, TIMEOUT).await
}

pub async fn start_with(
    web: &str,
    create: bool,
    device_name: &str,
    timeout: Duration,
) -> std::io::Result<Started> {
    if !is_web_url(web) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the account web must be an http or https address",
        ));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let verifier = pkce::new_verifier();
    let state = pkce::new_state();
    let redirect = format!("http://127.0.0.1:{port}{PATH}");
    let url = format!(
        "{}{}?client=net&intent={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&device={}",
        web.trim_end_matches('/'),
        anyplay_api::path::WEB_AUTHORIZE,
        if create { "create" } else { "sign_in" },
        encode(&redirect),
        state,
        pkce::challenge(&verifier),
        encode(device_name),
    );
    let (tx, result) = oneshot::channel();
    let task = tokio::spawn(async move {
        let outcome = tokio::time::timeout(timeout, serve(listener, port, &state))
            .await
            .unwrap_or(Callback::TimedOut);
        let _ = tx.send(outcome);
    });
    Ok(Started {
        url,
        verifier,
        result,
        task,
    })
}

/// Percent-encodes everything but the unreserved characters of RFC 3986.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn serve(listener: TcpListener, port: u16, state: &str) -> Callback {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            // A failing accept repeats at once: do not spin on it.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let answer = tokio::time::timeout(Duration::from_secs(5), handle(stream, port, state))
            .await
            .ok()
            .flatten();
        if let Some(callback) = answer {
            return callback;
        }
    }
}

/// Reads one request. `Some` when it ended the sign-in.
async fn handle(mut stream: TcpStream, port: u16, state: &str) -> Option<Callback> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let verdict = judge(&text, port, state);
    let (status, page) = match &verdict {
        Verdict::Done(Callback::Code(_)) => (200, DONE),
        Verdict::Done(_) => (200, DENIED),
        Verdict::Refused => (400, REFUSED),
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\nx-content-type-options: nosniff\r\ncontent-security-policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\r\nconnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Bad Request" },
        page.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(page.as_bytes()).await;
    let _ = stream.shutdown().await;
    match verdict {
        Verdict::Done(callback) => Some(callback),
        Verdict::Refused => None,
    }
}

enum Verdict {
    Done(Callback),
    Refused,
}

/// Decides what a request is. Anything but the cloud's redirect for this sign-in is refused.
fn judge(request: &str, port: u16, state: &str) -> Verdict {
    let mut lines = request.split("\r\n");
    let Some(line) = lines.next() else {
        return Verdict::Refused;
    };
    let mut parts = line.split(' ');
    let (Some("GET"), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Verdict::Refused;
    };
    if !version.starts_with("HTTP/1.") {
        return Verdict::Refused;
    }
    // A page in the player's browser must not be able to reach this through DNS rebinding.
    let host_ok = lines.any(|l| {
        l.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("host") && value.trim() == format!("127.0.0.1:{port}")
        })
    });
    if !host_ok {
        return Verdict::Refused;
    }
    let Some((path, query)) = target.split_once('?') else {
        return Verdict::Refused;
    };
    if path != PATH {
        return Verdict::Refused;
    }
    let (mut code, mut got_state, mut error) = (None, None, None);
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", v)) => code = Some(v),
            Some(("state", v)) => got_state = Some(v),
            Some(("error", v)) => error = Some(v),
            _ => {}
        }
    }
    let state_ok = got_state
        .is_some_and(|s| s.len() == state.len() && subtle_eq(s.as_bytes(), state.as_bytes()));
    if !state_ok {
        return Verdict::Refused;
    }
    if let Some(error) = error {
        let error: String = error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(40)
            .collect();
        return Verdict::Done(Callback::Denied(error));
    }
    match code {
        Some(code)
            if (16..=128).contains(&code.len())
                && code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Verdict::Done(Callback::Code(code.to_owned()))
        }
        _ => Verdict::Refused,
    }
}

fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

macro_rules! page {
    ($title:expr, $text:expr) => {
        concat!(
            "<!doctype html><html lang=en><meta charset=utf-8>",
            "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
            "<title>",
            $title,
            "</title>",
            "<style>:root{color-scheme:dark light}body{margin:0;min-height:100vh;display:grid;",
            "place-items:center;background:#05070f;color:#f2f4f8;font:16px/1.5 system-ui,sans-serif}",
            "main{max-width:26rem;padding:1.5rem}h1{font-size:1.5rem;font-weight:400;margin:0 0 .5rem}",
            "p{margin:0;color:#b9bfcc}@media (prefers-color-scheme:light){body{background:#f6f7fa;",
            "color:#14161c}p{color:#4a5060}}</style>",
            "<main><h1>",
            $title,
            "</h1><p>",
            $text,
            "</p></main>"
        )
    };
}

const DONE: &str = page!(
    "Signed in",
    "You can close this tab and go back to AnyPlay."
);
const DENIED: &str = page!(
    "Sign-in stopped",
    "Go back to AnyPlay and start again when you are ready."
);
const REFUSED: &str = page!(
    "This link is not valid",
    "Start the sign-in again from AnyPlay."
);

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "state-0123456789abcdef";
    const CODE: &str = "code-0123456789abcdef";

    fn get(port: u16, target: &str, host: &str) -> String {
        let _ = port;
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: test\r\n\r\n")
    }

    fn verdict(request: &str) -> Option<Callback> {
        match judge(request, 4000, STATE) {
            Verdict::Done(callback) => Some(callback),
            Verdict::Refused => None,
        }
    }

    #[test]
    fn the_redirect_of_this_sign_in_gives_its_code() {
        let request = get(
            4000,
            &format!("/callback?code={CODE}&state={STATE}"),
            "127.0.0.1:4000",
        );
        assert_eq!(verdict(&request), Some(Callback::Code(CODE.into())));
    }

    #[test]
    fn a_wrong_or_missing_state_is_refused() {
        for query in [
            format!("code={CODE}&state=other-state-0123456789"),
            format!("code={CODE}"),
            format!("code={CODE}&state="),
            format!("state={STATE}"),
        ] {
            let request = get(4000, &format!("/callback?{query}"), "127.0.0.1:4000");
            assert_eq!(verdict(&request), None, "{query}");
        }
    }

    #[test]
    fn another_host_path_or_method_is_refused() {
        let query = format!("code={CODE}&state={STATE}");
        for request in [
            get(4000, &format!("/callback?{query}"), "evil.example:4000"),
            get(4000, &format!("/callback?{query}"), "localhost:4000"),
            get(4000, &format!("/callback?{query}"), "127.0.0.1:4001"),
            get(4000, &format!("/other?{query}"), "127.0.0.1:4000"),
            get(4000, &format!("/callback/x?{query}"), "127.0.0.1:4000"),
            format!("POST /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n\r\n"),
            format!("GET /callback?{query} HTTP/1.1\r\n\r\n"),
            "garbage".to_owned(),
        ] {
            assert_eq!(verdict(&request), None, "{request:?}");
        }
    }

    #[test]
    fn a_code_of_odd_shape_is_refused() {
        for code in ["short", "has space in it 0123456789", &"a".repeat(129)] {
            let request = get(
                4000,
                &format!("/callback?code={code}&state={STATE}"),
                "127.0.0.1:4000",
            );
            assert_eq!(verdict(&request), None, "{code}");
        }
    }

    #[test]
    fn an_error_from_the_cloud_ends_the_sign_in() {
        let request = get(
            4000,
            &format!("/callback?error=access_denied&state={STATE}"),
            "127.0.0.1:4000",
        );
        assert_eq!(
            verdict(&request),
            Some(Callback::Denied("access_denied".into()))
        );
    }

    #[tokio::test]
    async fn the_listener_waits_for_the_right_request() {
        let started = start_with(
            "https://cloud.example/",
            false,
            "Alec's PC",
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        // The URL carries everything the cloud needs and nothing secret.
        assert!(
            started
                .url
                .starts_with("https://cloud.example/web/authorize?client=net")
        );
        assert!(started.url.contains("code_challenge_method=S256"));
        assert!(
            started.url.contains("device=Alec%27s%20PC")
                || started.url.contains("device=Alec%27s%20PC")
        );
        assert!(!started.url.contains(&started.verifier));
        let redirect = started
            .url
            .split("redirect_uri=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .replace("%3A", ":")
            .replace("%2F", "/");
        let port: u16 = redirect
            .strip_prefix("http://127.0.0.1:")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let state = started
            .url
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_owned();

        async fn ask(port: u16, target: &str) -> String {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let request = format!(
                "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            answer
        }

        let wrong = ask(port, &format!("/callback?code={CODE}&state=nope")).await;
        assert!(wrong.starts_with("HTTP/1.1 400"), "{wrong}");
        let right = ask(port, &format!("/callback?code={CODE}&state={state}")).await;
        assert!(right.starts_with("HTTP/1.1 200"), "{right}");
        assert!(right.contains("cache-control: no-store"));
        assert_eq!(started.result.await.unwrap(), Callback::Code(CODE.into()));
    }

    #[tokio::test]
    async fn nobody_coming_back_times_out() {
        let started = start_with(
            "https://cloud.example",
            true,
            "PC",
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(started.url.contains("intent=create"));
        assert_eq!(started.result.await.unwrap(), Callback::TimedOut);
    }
}

#[cfg(test)]
mod scheme_tests {
    use super::is_web_url;

    #[test]
    fn only_http_and_https_are_pages() {
        assert!(is_web_url("https://anyplaynet.com/sign-in"));
        assert!(is_web_url("http://127.0.0.1:8080/"));
        assert!(!is_web_url("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("ms-settings:network"));
        assert!(!is_web_url("https://a.example/ b"));
        assert!(!is_web_url(""));
    }
}
