//! Subscribing to `GET /api/events`, and the one command that only does that.
//!
//! `tandem watch` prints each change as `version=<N> heads=<hex1>,<hex2>,...`.
//!
//! The events themselves carry no head data — they are wake-ups. On each one
//! the watcher reads `/api/heads` and prints what it finds, which is why two
//! publishes in quick succession may print as one line: a wake-up that
//! coalesces with the one behind it loses nothing, because the read that
//! answers it sees the later state anyway.
//!
//! [`subscribe`] and [`EventStream`] are the reusable half. The workspace
//! daemon subscribes to the same stream for the same reason and reads it the
//! same way; a second copy of an SSE line parser is a second place for a
//! keep-alive comment to be mistaken for an event.

use std::io::{BufRead, BufReader};

use anyhow::{anyhow, Context, Result};

use jj_tandem_client::http_client::{
    build_http_client, ConnectorTarget, RepoCapability, TandemClient,
};
use jj_tandem_protocol::{hex::to_hex, wire};

/// Open the head-change stream, or say why the server would not.
///
/// The stream has no request timeout: it is meant to stay open, and a
/// keep-alive comment rather than a reconnect is what says the server is
/// still there.
pub fn subscribe(server_addr: &str, token: &str) -> Result<EventStream<ResponseLines>> {
    let target = ConnectorTarget::parse(server_addr)?;
    let http = build_http_client(None)?;
    let events = http
        .get(format!("{}/api/events", target.base_url()))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .bearer_auth(token)
        .send()
        .with_context(|| format!("event subscription failed for {server_addr}"))?;

    if !events.status().is_success() {
        return Err(anyhow!(
            "event stream refused by {server_addr}: HTTP {}",
            events.status().as_u16()
        ));
    }

    Ok(EventStream::new(events))
}

pub fn run_watch(server_addr: &str, token: &str) -> Result<()> {
    // Refuse a server that cannot do this before opening a long-lived stream.
    let client =
        TandemClient::connect_with_requirements(server_addr, token, &[RepoCapability::WatchHeads])
            .with_context(|| format!("watch preflight failed for {server_addr}"))?;

    let events = subscribe(server_addr, token)?;

    eprintln!("watching heads on {server_addr}...");

    // What the last printed line said, so that a wake-up for a version
    // already reported does not print it twice.
    let mut last_printed: Option<u64> = None;
    print_heads(&client, &mut last_printed)?;

    for event in events {
        let event = event?;
        // The version in the event is a hint. The read below is the truth,
        // and it is what decides whether there is anything to print.
        tracing::trace!(version = event.version, "heads wake-up");
        print_heads(&client, &mut last_printed)?;
    }

    Ok(())
}

fn print_heads(client: &TandemClient, last_printed: &mut Option<u64>) -> Result<()> {
    let state = client.get_heads_state()?;
    if last_printed.is_some_and(|printed| printed >= state.version) {
        return Ok(());
    }
    *last_printed = Some(state.version);

    let hex_heads: Vec<String> = state.heads.iter().map(|head| to_hex(head)).collect();
    println!("version={} heads={}", state.version, hex_heads.join(","));
    Ok(())
}

// ─── Server-sent events ───────────────────────────────────────────────────────

/// The part of `text/event-stream` tandem needs.
///
/// Events are blank-line-separated blocks of `field: value` lines. Only the
/// `data:` lines matter here; comment lines (`:`), which are what a keep-alive
/// looks like, and every other field are skipped.
pub struct EventStream<R: BufRead> {
    reader: R,
}

/// What a subscription reads: the response body, buffered a line at a time.
pub type ResponseLines = BufReader<reqwest::blocking::Response>;

impl EventStream<ResponseLines> {
    pub fn new(response: reqwest::blocking::Response) -> Self {
        Self {
            reader: BufReader::new(response),
        }
    }
}

impl<R: BufRead> Iterator for EventStream<R> {
    type Item = Result<wire::HeadsEventBody>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut data = String::new();

        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                // End of stream: the server went away.
                Ok(0) => return None,
                Ok(_) => {}
                Err(err) => return Some(Err(anyhow!("reading the event stream: {err}"))),
            }

            let line = line.trim_end_matches(['\r', '\n']);

            // A blank line ends the event.
            if line.is_empty() {
                if data.is_empty() {
                    continue;
                }
                return match serde_json::from_str::<wire::HeadsEventBody>(&data) {
                    Ok(event) => Some(Ok(event)),
                    // A payload this client does not understand is not worth
                    // ending the watch over; the next one may be fine.
                    Err(err) => {
                        tracing::debug!(error = %err, data = %data, "skipping an unreadable event");
                        Some(Ok(wire::HeadsEventBody { version: 0 }))
                    }
                };
            }

            if let Some(payload) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(payload.trim_start());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(stream: &str) -> Vec<u64> {
        EventStream {
            reader: Cursor::new(stream.as_bytes().to_vec()),
        }
        .map(|event| event.expect("event").version)
        .collect()
    }

    #[test]
    fn events_are_read_one_blank_line_at_a_time() {
        let stream = "event: heads\ndata: {\"version\":1}\n\n\
                      event: heads\ndata: {\"version\":2}\n\n";
        assert_eq!(parse(stream), vec![1, 2]);
    }

    #[test]
    fn keep_alive_comments_are_skipped() {
        let stream = ":\n\n:keep-alive\n\nevent: heads\ndata: {\"version\":7}\n\n";
        assert_eq!(parse(stream), vec![7]);
    }

    #[test]
    fn a_truncated_final_event_ends_the_stream_without_an_error() {
        let stream = "event: heads\ndata: {\"version\":3}\n\ndata: {\"vers";
        assert_eq!(parse(stream), vec![3]);
    }
}
