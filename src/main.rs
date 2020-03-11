#![allow(dead_code)]

use std::{
    error::Error,
    future::Future,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
    u32,
};

use futures_core::{ready, TryFuture};
use futures_util::FutureExt;
use rand::{Rng, distributions::Uniform};
use reqwest::Client;
use serde::{Serialize, Deserialize};
use serde_repr::{Serialize_repr, Deserialize_repr};
use tokio::{
    net::UdpSocket,
    time::timeout,
};
use url::Url;

#[derive(Debug, Deserialize)]
struct PeerList {
    #[serde(rename="peer id")]
    peer_id: String,
    ip: String,
    port: i32,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Peers {
    Full(PeerList),
    Compact(Vec<u8>),
}

#[derive(Debug, Deserialize)]
struct TrackerSuccessResponse {
    #[serde(default)]
    #[serde(rename="warning message")]
    warning_message: Option<String>,
    interval: i32,
    #[serde(default)]
    #[serde(rename="min interval")]
    min_interval: i32,
    #[serde(default)]
    #[serde(rename="tracker id")]
    tracker_id: Option<String>,
    #[serde(default)]
    complete: Option<i32>,
    #[serde(default)]
    incomplete: Option<i32>,
    peers: Vec<Peers>,
}

#[derive(Debug, Deserialize)]
struct TrackerResponse {
    #[serde(default)]
    #[serde(rename="failure reason")]
    failure_reason: Option<String>,

    #[serde(default)]
    #[serde(flatten)]
    response: Option<TrackerSuccessResponse>,
}

#[derive(Debug)]
struct DurationFuture<F: Future> {
    inner: F,
    start: Option<Instant>,
}

impl<F: Future> DurationFuture<F> {
    fn new(inner: F, start: Option<Instant>) -> Self {
        Self {
            inner,
            start,
        }
    }

    fn pin_get_inner(self: Pin<&mut Self>) -> Pin<&mut F> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }

    fn pin_get_start(self: Pin<&mut Self>) -> &mut Option<Instant> {
        unsafe { &mut self.get_unchecked_mut().start }
    }
}

impl<F: Future> Future for DurationFuture<F> {
    type Output = (F::Output, Duration);
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.start.is_none() {
            *self.as_mut().pin_get_start() = Some(Instant::now());
        }
        Poll::Ready((
            ready!(self.as_mut().pin_get_inner().poll(cx)),
            self.start.unwrap().elapsed(),
        ))
    }
}

#[derive(Debug)]
struct DurationTryFuture<F: TryFuture> {
    inner: F,
    start: Option<Instant>,
}

impl<F: TryFuture> DurationTryFuture<F> {
    fn new(inner: F, start: Option<Instant>) -> Self {
        Self {
            inner,
            start,
        }
    }

    fn pin_get_inner(self: Pin<&mut Self>) -> Pin<&mut F> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }

    fn pin_get_start(self: Pin<&mut Self>) -> &mut Option<Instant> {
        unsafe { &mut self.get_unchecked_mut().start }
    }
}

impl<F: TryFuture> Future for DurationTryFuture<F> {
    type Output = Result<(F::Ok, Duration), F::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.start.is_none() {
            *self.as_mut().pin_get_start() = Some(Instant::now());
        }
        Poll::Ready(Ok((
            ready!(self.as_mut().pin_get_inner().try_poll(cx))?,
            self.start.unwrap().elapsed(),
        )))
    }
}

#[derive(Debug)]
struct ReadyTimeFuture<F: Future> {
    inner: F
}

impl<F: Future> ReadyTimeFuture<F> {
    fn new(inner: F) -> Self {
        Self { inner }
    }

    fn pin_get_inner(self: Pin<&mut Self>) -> Pin<&mut F> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }
}

impl<F: Future> Future for ReadyTimeFuture<F> {
    type Output = (F::Output, Instant);
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready((
            ready!(self.pin_get_inner().poll(cx)),
            Instant::now(),
        ))
    }
}

#[derive(Debug, PartialEq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
enum Action {
    Connect = 0,
    Announce = 1,
    Scrape = 2,
    Error = 3,
}

#[derive(Debug, Serialize)]
#[repr(C)]
struct TrackerConnectRequest {
    protocol_id: u64,
    action: Action,
    transaction_id: u32,
}

impl TrackerConnectRequest {
    fn new() -> Self {
        let transaction_id =
            rand::thread_rng().sample(Uniform::from(u32::MIN..=u32::MAX));
        Self {
            protocol_id: PROTOCOL_ID_MAGIC,
            transaction_id,
            action: Action::Connect,
        }
    }
}

#[derive(Debug, Deserialize)]
#[repr(C)]
struct TrackerConnectResponse {
    action: Action,
    transaction_id: u32,
    connection_id: u64,
}

const PROTOCOL_ID_MAGIC: u64 = 0x41727101980;

const NYAA_URL: &str = "http://nyaa.tracker.wf:7777/announce";
const UDP_URL: &str = "udp://p4p.arenabg.com:1337";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let trackers = vec![NYAA_URL, UDP_URL];
    for tracker in trackers {
        let tracker_url = Url::parse(tracker)?;
        match tracker_url.scheme() {
            "http" | "https" => {
                let client = Client::new();
                let http_req_fut = client.get(tracker_url).send();
                // TODO: expected point of failure: connecting to host
                let (http_resp, resp_time) =
                    DurationFuture::new(http_req_fut, None).await;
                let http_resp = http_resp?;
                let http_bytes = http_resp.bytes().await?;
                // TODO: fail early on empty response body
                // TODO: expected point of failure: interpreting
                // response as bencode
                let resp: TrackerResponse =
                    serde_bencode::from_bytes(&http_bytes)?;
                let failure_reason = resp.failure_reason;
                if let Some(r) = failure_reason {
                    println!("Error message from HTTP tracker: {}", r);
                    println!(
                        "HTTP tracker response time: {}",
                        resp_time.as_secs_f64(),
                    );
                }
            },
            "udp" => {
                // TODO: handle IPv6 tracker addresses
                let mut socket =
                    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
                let tracker_addr =
                    tracker_url
                        .socket_addrs(|| None)?
                        .into_iter()
                        .next().unwrap();
                let mut request = TrackerConnectRequest::new();
                let bc = {
                    let mut config = bincode::config();
                    config.big_endian();
                    config
                };
                let mut response_bytes = [0u8; 16];
                socket.connect(tracker_addr).await?;
                let timeouts =
                    (0..=8).map(|n| Duration::from_secs(15 * 2_u64.pow(n)));
                let mut bytes_received = 0;
                for t in timeouts {
                    request = TrackerConnectRequest::new();
                    let request_bytes = bc.serialize(&request)?;
                    let send_fut = ReadyTimeFuture::new(
                        socket.send(&request_bytes)
                    ).map(|(r, t)| Ok::<_, tokio::io::Error>((r?, t)));
                    let (bytes_sent, start) =
                        send_fut.await?;
                    assert_eq!(
                        bytes_sent,
                        bc.serialized_size(&request)? as usize,
                    );
                    let recv_fut =
                        timeout(t, socket.recv(&mut response_bytes));
                    bytes_received = match recv_fut.await {
                        Ok(x) => x?,
                        Err(_) => {
                            eprintln!(
                                "No response received in {} seconds, retrying",
                                t.as_secs(),
                            );
                            continue;
                        },
                    };
                }
                // TODO: actually handle response not satisfying
                // expected conditions (replace assertions with actual
                // handling logic)
                assert!(bytes_received >= 16);
                let response: TrackerConnectResponse =
                    match bc.deserialize(&response_bytes) {
                        Ok(r) => r,
                        Err(_) => {
                            unimplemented!(
                                "handle unexpected response format"
                            );
                        },
                };
                assert_eq!(response.transaction_id, request.transaction_id);
                assert_eq!(response.action, Action::Connect);
                println!("Response from UDP tracker: {:?}", response);
            },
            _ => {
                unimplemented!("handle invalid URI scheme");
            }
        }
    }
    Ok(())
}
