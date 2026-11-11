//! Read the bitrate a stream advertises in its ICY response headers.
//!
//! Directories such as radio.net do not publish a bitrate, so the only source
//! is the stream itself. A probe opens each URL just long enough to read the
//! headers, then closes it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// One result from the probe worker.
pub enum ProbeEvent {
    Result { id: Uuid, bitrate: Option<u32> },
    Done,
}

/// The bitrate in one header, when it is a positive `icy-br`, `ice-bitrate`
/// or `icy-bitrate` value.
pub fn from_header(name: &str, value: &str) -> Option<u32> {
    if !(name.eq_ignore_ascii_case("icy-br")
        || name.eq_ignore_ascii_case("ice-bitrate")
        || name.eq_ignore_ascii_case("icy-bitrate"))
    {
        return None;
    }
    value.trim().parse::<u32>().ok().filter(|value| *value > 0)
}

/// Probe every URL for its advertised bitrate, with bounded concurrency.
/// Results arrive on `tx`; `Done` is sent when the queue is exhausted or
/// cancelled.
pub fn probe_streams(
    targets: Vec<(Uuid, String)>,
    cancel: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<ProbeEvent>,
) {
    const CONCURRENCY: usize = 16;
    const TIMEOUT: Duration = Duration::from_secs(4);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::warn!("could not start the bitrate probe: {e}");
            let _ = tx.send(ProbeEvent::Done);
            return;
        }
    };
    runtime.block_on(async move {
        let client = match reqwest::Client::builder()
            .user_agent("VibeRadio/0.1")
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                log::warn!("could not build the bitrate client: {e}");
                let _ = tx.send(ProbeEvent::Done);
                return;
            }
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for (id, url) in targets {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let client = client.clone();
            let tx = tx.clone();
            let cancel = Arc::clone(&cancel);
            tasks.spawn(async move {
                let _permit = permit;
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let bitrate = probe_one(&client, &url).await;
                let _ = tx.send(ProbeEvent::Result { id, bitrate });
            });
        }
        while tasks.join_next().await.is_some() {}
        let _ = tx.send(ProbeEvent::Done);
    });
}

async fn probe_one(client: &reqwest::Client, url: &str) -> Option<u32> {
    let response = client
        .get(url)
        .header("Icy-MetaData", "1")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .headers()
        .iter()
        .find_map(|(name, value)| from_header(name.as_str(), value.to_str().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_each_bitrate_header_spelling() {
        assert_eq!(from_header("icy-br", "128"), Some(128));
        assert_eq!(from_header("Ice-Bitrate", " 192 "), Some(192));
        assert_eq!(from_header("icy-bitrate", "320"), Some(320));
        assert_eq!(from_header("icy-br", "0"), None);
        assert_eq!(from_header("icy-br", "fast"), None);
        assert_eq!(from_header("content-type", "audio/mpeg"), None);
    }

    #[test]
    fn probes_a_stream_for_its_bitrate() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        if std::env::var_os("HTTP_PROXY").is_some()
            || std::env::var_os("http_proxy").is_some()
        {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = socket.read(&mut scratch);
                let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nicy-br: 192\r\nConnection: close\r\n\r\n",
                );
                let _ = socket.flush();
            }
        });
        let id = Uuid::new_v4();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        probe_streams(vec![(id, format!("http://{addr}/live"))], cancel, tx);
        let mut found = None;
        for event in rx {
            match event {
                ProbeEvent::Result { id: got, bitrate } if got == id => found = bitrate,
                ProbeEvent::Done => break,
                _ => {}
            }
        }
        assert_eq!(found, Some(192));
    }
}
