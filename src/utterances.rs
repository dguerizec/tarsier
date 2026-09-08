//! Connection-owned, event-gated raw audio subscriptions. No disk recording.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use axum::{
    extract::ws::{Message, WebSocket},
    http::HeaderMap,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::{
    audio::{AudioHub, Frame, Packet},
    auth::Auth,
    model::{SemanticEvent, unix_ms},
    runtime::Runtime,
};

const RATE: usize = 48_000;
const SAMPLE_BYTES: usize = 4; // Interleaved stereo s16le.
const BLOCK_BYTES: usize = 960 * SAMPLE_BYTES;
const ROUTE: &str = "/api/v1/audio/utterances";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Condition {
    event: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Subscribe {
    Subscribe,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    #[serde(rename = "type")]
    kind: Subscribe,
    source: String,
    start: Condition,
    end: Condition,
    #[serde(default = "default_pre_roll")]
    pre_roll_ms: u64,
    #[serde(default = "default_max_duration")]
    max_duration_ms: u64,
}
fn default_pre_roll() -> u64 {
    300
}
fn default_max_duration() -> u64 {
    60_000
}

impl Subscription {
    fn validate(&self) -> Result<(), &'static str> {
        let event_valid = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        };
        if self.source.is_empty() || self.source.len() > 512 {
            return Err("invalid_source");
        }
        if !event_valid(&self.start.event)
            || !event_valid(&self.end.event)
            || self.start.event == self.end.event
        {
            return Err("invalid_event_conditions");
        }
        if self.pre_roll_ms > 5000 {
            return Err("pre_roll_ms_must_be_between_0_and_5000");
        }
        if !(100..=120_000).contains(&self.max_duration_ms) {
            return Err("max_duration_ms_must_be_between_100_and_120000");
        }
        Ok(())
    }
}

struct Active {
    id: String,
    opened: Instant,
    through: Instant,
    samples: usize,
    closing: Option<(Instant, String, Option<u64>)>,
}

struct Gate {
    subscription: Subscription,
    id: String,
    history: VecDeque<Frame>,
    active: Option<Active>,
    sequence: u64,
    last_frame: Option<Instant>,
}

fn metadata(value: Value) -> Message {
    Message::Text(value.to_string().into())
}

fn clip(frame: &Frame, from: Instant, to: Instant) -> Vec<u8> {
    let samples = frame.pcm.len() / SAMPLE_BYTES;
    let duration = Duration::from_nanos((samples as u64 * 1_000_000_000) / RATE as u64);
    let Some(start) = frame.captured.checked_sub(duration) else {
        return vec![];
    };
    let index = |at: Instant| {
        let nanos = at.saturating_duration_since(start).as_nanos();
        (nanos * RATE as u128)
            .div_ceil(1_000_000_000)
            .min(samples as u128) as usize
    };
    let first = index(from);
    let last = index(to);
    if first >= last {
        vec![]
    } else {
        frame.pcm[first * SAMPLE_BYTES..last * SAMPLE_BYTES].to_vec()
    }
}

impl Gate {
    fn new(subscription: Subscription) -> Self {
        Self {
            subscription,
            id: format!("{:016x}", rand::random::<u64>()),
            history: VecDeque::new(),
            active: None,
            sequence: 0,
            last_frame: None,
        }
    }

    fn trim(&mut self, now: Instant) {
        let oldest = now
            .checked_sub(Duration::from_millis(self.subscription.pre_roll_ms))
            .unwrap_or(now);
        while self
            .history
            .front()
            .is_some_and(|frame| frame.captured <= oldest)
        {
            self.history.pop_front();
        }
        // Independent of timestamps: never retain more than 5 seconds plus one block.
        while self.history.len() > 251 {
            self.history.pop_front();
        }
    }

    fn start(&mut self, at: Instant, event_sequence: u64) -> Vec<Message> {
        if self.active.is_some() {
            return vec![];
        }
        self.trim(at);
        self.sequence += 1;
        let id = format!("{}:{}", self.id, self.sequence);
        let from = at
            .checked_sub(Duration::from_millis(self.subscription.pre_roll_ms))
            .unwrap_or(at);
        let pre_samples: usize = self
            .history
            .iter()
            .map(|frame| clip(frame, from, at).len() / SAMPLE_BYTES)
            .sum();
        self.active = Some(Active {
            id: id.clone(),
            opened: at,
            through: from,
            samples: 0,
            closing: None,
        });
        let mut output = vec![metadata(json!({"type":"started", "utterance_id":id,
            "event_sequence":event_sequence, "pre_roll_ms":pre_samples as f64 * 1000.0 / RATE as f64}))];
        for frame in self.history.clone() {
            output.extend(self.deliver(&frame));
        }
        output
    }

    fn deliver(&mut self, frame: &Frame) -> Vec<Message> {
        let Some(active) = &mut self.active else {
            return vec![];
        };
        let until = active
            .closing
            .as_ref()
            .map_or(frame.captured, |(at, _, _)| *at);
        // Every capture block contains new samples. Arrival-time jitter must not
        // make us discard the start of a block as if it duplicated the previous one.
        let from = active
            .opened
            .checked_sub(Duration::from_millis(self.subscription.pre_roll_ms))
            .unwrap_or(active.opened);
        let bytes = clip(frame, from, until);
        active.through = active.through.max(frame.captured.min(until));
        let mut output = vec![];
        if !bytes.is_empty() {
            active.samples += bytes.len() / SAMPLE_BYTES;
            output.push(Message::Binary(bytes.into()));
        }
        if active
            .closing
            .as_ref()
            .is_some_and(|(at, _, _)| frame.captured >= *at)
        {
            output.extend(self.finish());
        }
        output
    }

    fn audio(&mut self, frame: Frame, now: Instant) -> Result<Vec<Message>, &'static str> {
        if frame.pcm.len() != BLOCK_BYTES
            || frame.captured > now
            || now.duration_since(frame.captured) > Duration::from_millis(250)
            || self.last_frame.is_some_and(|last| frame.captured <= last)
        {
            return Err("invalid_or_stale_audio");
        }
        self.last_frame = Some(frame.captured);
        let mut output = vec![];
        if let Some(active) = &self.active {
            let deadline = active.opened + Duration::from_millis(self.subscription.max_duration_ms);
            if frame.captured >= deadline {
                output.extend(self.close(deadline, "max_duration", None));
            }
        }
        output.extend(self.deliver(&frame));
        if self.subscription.pre_roll_ms > 0 {
            self.history.push_back(frame);
        }
        self.trim(now);
        Ok(output)
    }

    fn close(&mut self, at: Instant, reason: &str, sequence: Option<u64>) -> Vec<Message> {
        let Some(active) = &mut self.active else {
            return vec![];
        };
        if active.closing.is_none() {
            active.closing = Some((at, reason.into(), sequence));
        }
        if active.through >= at {
            self.finish()
        } else {
            vec![]
        }
    }

    fn finish(&mut self) -> Vec<Message> {
        let Some(active) = self.active.take() else {
            return vec![];
        };
        let (_, reason, sequence) =
            active
                .closing
                .unwrap_or((active.through, "interrupted".into(), None));
        vec![metadata(
            json!({"type":"ended", "utterance_id":active.id, "reason":reason,
            "event_sequence":sequence, "samples":active.samples,
            "audio_ms": active.samples as f64 * 1000.0 / RATE as f64}),
        )]
    }

    fn abort(&mut self, reason: &str) -> Vec<Message> {
        if let Some(active) = &mut self.active {
            active.closing = Some((active.through, reason.into(), None));
        }
        self.history.clear();
        self.finish()
    }

    fn tick(&mut self, now: Instant) -> Vec<Message> {
        self.trim(now);
        let mut output = vec![];
        if let Some(active) = &self.active {
            let deadline = active.opened + Duration::from_millis(self.subscription.max_duration_ms);
            if now >= deadline {
                output.extend(self.close(deadline, "max_duration", None));
            }
        }
        if self
            .active
            .as_ref()
            .and_then(|a| a.closing.as_ref())
            .is_some_and(|(at, _, _)| {
                now.saturating_duration_since(*at) >= Duration::from_millis(250)
            })
        {
            output.extend(self.abort("audio_tail_timeout"));
        }
        output
    }
}

async fn send(sender: &mut SplitSink<WebSocket, Message>, messages: Vec<Message>) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(250), async {
            for message in messages {
                sender.send(message).await?;
            }
            Ok::<_, axum::Error>(())
        })
        .await,
        Ok(Ok(()))
    )
}

async fn allowed(auth: &Option<Auth>, headers: &HeaderMap) -> bool {
    match auth {
        Some(auth) => auth.allowed(headers, ROUTE, "GET").await,
        None => true,
    }
}

pub(crate) async fn stream(
    socket: WebSocket,
    audio: AudioHub,
    runtime: Runtime,
    auth: Option<Auth>,
    headers: HeaderMap,
    virtual_source: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let (mut sender, mut receiver) = socket.split();
    let request = tokio::select! {
        _ = shutdown.changed() => return,
        request = tokio::time::timeout(Duration::from_secs(10), receiver.next()) => request,
    };
    let subscription = match request {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Subscription>(&text).ok(),
        _ => None,
    };
    let Some(subscription) = subscription else {
        send(
            &mut sender,
            vec![metadata(
                json!({"type":"error", "code":"subscribe_required"}),
            )],
        )
        .await;
        return;
    };
    let policy_error = subscription.validate().err();
    if let Some(error) = policy_error {
        send(
            &mut sender,
            vec![metadata(json!({"type":"error", "code":error}))],
        )
        .await;
        return;
    }
    // This is the subscription authorization boundary. Fine-grained token
    // permissions can refine it; today the existing API destination is required.
    if !allowed(&auth, &headers).await {
        return;
    }
    let mut states = runtime.subscribe_state();
    if subscription.source == virtual_source
        || !states
            .borrow()
            .audio_capture_sources
            .contains(&subscription.source)
    {
        send(
            &mut sender,
            vec![metadata(
                json!({"type":"error", "code":"source_not_enabled"}),
            )],
        )
        .await;
        return;
    }
    let mut packets = audio.subscribe_raw(&subscription.source);
    let mut events = runtime.subscribe_events();
    let mut gate = Gate::new(subscription);
    if !send(
        &mut sender,
        vec![metadata(
            json!({"type":"subscribed", "subscription_id":gate.id,
        "subscription":gate.subscription,
        "format":{"encoding":"pcm_s16le", "sample_rate":RATE, "channels":2, "block_ms":20},
        "processing":"raw_capture"}),
        )],
    )
    .await
    {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut auth_tick = tokio::time::interval(Duration::from_secs(1));
    auth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_audio = Instant::now();
    let reason = loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break "shutdown",
            _ = auth_tick.tick() => {
                if !allowed(&auth, &headers).await { break "authorization_revoked"; }
                if !send(&mut sender, vec![metadata(json!({"type":"heartbeat", "active":gate.active.as_ref().map(|active| &active.id)}))]).await { break "slow_client"; }
            },
            changed = states.changed() => {
                if changed.is_err() || !states.borrow_and_update().audio_capture_sources.contains(&gate.subscription.source) { break "source_disabled"; }
            },
            event = events.recv() => {
                let event: SemanticEvent = match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => break "events_lost",
                    Err(_) => break "events_closed",
                };
                let now = Instant::now();
                let at = now.checked_sub(Duration::from_millis(unix_ms().saturating_sub(event.emitted_at_ms))).unwrap_or(now);
                let output = if event.kind == gate.subscription.start.event {
                    if !allowed(&auth, &headers).await { break "authorization_revoked"; }
                    gate.start(at, event.sequence)
                } else if event.kind == gate.subscription.end.event { gate.close(at, "condition", Some(event.sequence)) } else { vec![] };
                if !send(&mut sender, output).await { break "slow_client"; }
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(text))) if serde_json::from_str::<Value>(&text).ok() == Some(json!({"type":"unsubscribe"})) => break "unsubscribed",
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {},
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "disconnected",
                _ => break "invalid_client_message",
            },
            _ = tick.tick() => {
                if last_audio.elapsed() >= Duration::from_secs(1) { break "audio_stalled"; }
                let output = gate.tick(Instant::now());
                if !send(&mut sender, output).await { break "slow_client"; }
            },
            packet = packets.recv() => {
                let frame = match packet {
                    Ok(Packet::Audio(frame)) => frame,
                    Ok(Packet::Error(_)) => break "audio_source_error",
                    Err(broadcast::error::RecvError::Lagged(_)) => break "audio_lost",
                    Err(_) => break "audio_closed",
                };
                last_audio = Instant::now();
                match gate.audio(frame, last_audio) {
                    Ok(output) => if !send(&mut sender, output).await { break "slow_client"; },
                    Err(error) => break error,
                }
            },
        }
    };
    send(&mut sender, gate.abort(reason)).await;
    send(
        &mut sender,
        vec![
            metadata(json!({"type":"closed", "reason":reason})),
            Message::Close(None),
        ],
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message as ClientFrame, client::IntoClientRequest},
    };

    fn subscription(pre_roll_ms: u64) -> Subscription {
        serde_json::from_value(json!({"type":"subscribe", "source":"test-mic",
            "start":{"event":"gesture.phone_near_mouth.started"},
            "end":{"event":"gesture.phone_near_mouth.ended"}, "pre_roll_ms":pre_roll_ms}))
        .unwrap()
    }

    fn frame(at: Instant, value: u8) -> Frame {
        Frame {
            captured: at,
            pcm: Arc::new(vec![value; BLOCK_BYTES]),
        }
    }

    fn binary(messages: &[Message]) -> Vec<u8> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Binary(bytes) => Some(bytes.as_ref()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    fn texts(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Text(text) => Some(serde_json::from_str(text).unwrap()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn utterances_zero_buffer_clips_start_and_final_partial_block() {
        let origin = Instant::now();
        let mut gate = Gate::new(subscription(0));
        assert!(gate.audio(frame(origin, 1), origin).unwrap().is_empty());
        assert!(gate.history.is_empty());
        let opening = origin + Duration::from_millis(5);
        assert_eq!(texts(&gate.start(opening, 1))[0]["pre_roll_ms"], 0.0);
        let data = gate
            .audio(
                frame(origin + Duration::from_millis(20), 2),
                origin + Duration::from_millis(20),
            )
            .unwrap();
        assert_eq!(binary(&data), vec![2; 15 * 48 * SAMPLE_BYTES]);
        assert!(
            gate.close(origin + Duration::from_millis(25), "condition", Some(2))
                .is_empty()
        );
        let data = gate
            .audio(
                frame(origin + Duration::from_millis(40), 3),
                origin + Duration::from_millis(40),
            )
            .unwrap();
        assert_eq!(binary(&data), vec![3; 5 * 48 * SAMPLE_BYTES]);
        assert_eq!(texts(&data)[0]["samples"], 960);
        assert_eq!(texts(&data)[0]["reason"], "condition");
        assert!(
            gate.audio(
                frame(origin + Duration::from_millis(60), 4),
                origin + Duration::from_millis(60)
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn utterances_keeps_all_samples_despite_capture_scheduling_jitter() {
        let origin = Instant::now();
        let mut gate = Gate::new(subscription(0));
        gate.start(origin, 1);
        let mut output = vec![];
        for millis in [20, 39, 61] {
            let at = origin + Duration::from_millis(millis);
            output.extend(gate.audio(frame(at, 5), at).unwrap());
        }
        assert_eq!(binary(&output).len(), 3 * BLOCK_BYTES);
    }

    #[test]
    fn utterances_pre_roll_is_bounded_sample_aligned_and_not_repeated() {
        let origin = Instant::now();
        let mut gate = Gate::new(subscription(25));
        for index in 1..=3 {
            let at = origin + Duration::from_millis(index * 20);
            assert!(gate.audio(frame(at, index as u8), at).unwrap().is_empty());
        }
        let start = origin + Duration::from_millis(60);
        let messages = gate.start(start, 1);
        assert_eq!(texts(&messages)[0]["pre_roll_ms"], 25.0);
        let mut expected = vec![2; 5 * 48 * SAMPLE_BYTES];
        expected.extend(vec![3; BLOCK_BYTES]);
        assert_eq!(binary(&messages), expected);
        assert!(gate.start(start, 2).is_empty());
        let at = origin + Duration::from_millis(80);
        assert_eq!(
            binary(&gate.audio(frame(at, 4), at).unwrap()),
            vec![4; BLOCK_BYTES]
        );
        assert_eq!(texts(&gate.abort("disconnected"))[0]["audio_ms"], 45.0);
        assert!(gate.history.is_empty());
    }

    #[test]
    fn utterances_subscription_validation_and_capture_failures_are_closed() {
        assert!(subscription(0).validate().is_ok());
        assert!(subscription(5001).validate().is_err());
        let mut request = subscription(300);
        request.end = request.start.clone();
        assert!(request.validate().is_err());
        let mut gate = Gate::new(subscription(300));
        let at = Instant::now();
        gate.start(at, 1);
        assert!(
            gate.audio(frame(at, 1), at + Duration::from_secs(1))
                .is_err()
        );
        assert!(
            gate.audio(
                Frame {
                    captured: at,
                    pcm: Arc::new(vec![0; 3])
                },
                at
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Subscription>(
                json!({"type":"subscribe", "source":"test", "command":"rm"})
            )
            .is_err()
        );
    }

    #[test]
    fn utterances_maximum_duration_is_enforced_before_sending_a_block() {
        let origin = Instant::now();
        let mut request = subscription(0);
        request.max_duration_ms = 100;
        let mut gate = Gate::new(request);
        gate.start(origin, 1);
        let mut output = vec![];
        for index in 1..=6 {
            let at = origin + Duration::from_millis(index * 20);
            output.extend(gate.audio(frame(at, index as u8), at).unwrap());
        }
        assert_eq!(binary(&output).len(), 100 * 48 * SAMPLE_BYTES);
        assert_eq!(texts(&output).last().unwrap()["reason"], "max_duration");
        assert!(gate.active.is_none());
    }

    async fn test_server() -> (
        String,
        Runtime,
        AudioHub,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        test_server_with_auth(None).await
    }

    async fn test_server_with_auth(
        auth: Option<Auth>,
    ) -> (
        String,
        Runtime,
        AudioHub,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let runtime = Runtime::new();
        runtime
            .update(|state| state.audio_capture_sources = vec!["test-mic".into()])
            .await;
        let audio = AudioHub::default();
        let (stop, shutdown) = watch::channel(false);
        let app = crate::api::router_with_controls(
            crate::config::Config::default(),
            runtime.clone(),
            crate::pipeline::PreviewHub::new(),
            None,
            crate::api::ApiOptions {
                auth,
                audio: Some(audio.clone()),
                ..Default::default()
            },
            shutdown.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/api/v1/audio/utterances",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let mut shutdown = shutdown;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await
                .unwrap();
        });
        (url, runtime, audio, stop, task)
    }

    async fn next_text(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        kind: &str,
    ) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ClientFrame::Text(text) = socket.next().await.unwrap().unwrap() {
                    let value: Value = serde_json::from_str(&text).unwrap();
                    if value["type"] == kind {
                        break value;
                    }
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn utterances_websocket_subscription_streams_only_its_audio_and_stops_on_disable() {
        let (url, runtime, audio, stop, server) = test_server().await;
        let (mut socket, _) = connect_async(&url).await.unwrap();
        socket
            .send(ClientFrame::Text(
                serde_json::to_string(&subscription(0)).unwrap().into(),
            ))
            .await
            .unwrap();
        assert_eq!(
            next_text(&mut socket, "subscribed").await["format"]["sample_rate"],
            RATE
        );
        audio.publish_test_audio("test-mic", Packet::Audio(frame(Instant::now(), 1)));
        runtime
            .emit("unrelated.event", "test", None, json!({}))
            .await;
        runtime
            .emit("gesture.phone_near_mouth.started", "test", None, json!({}))
            .await;
        next_text(&mut socket, "started").await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        audio.publish_test_audio("test-mic", Packet::Audio(frame(Instant::now(), 7)));
        let bytes = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ClientFrame::Binary(bytes) = socket.next().await.unwrap().unwrap() {
                    break bytes;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(bytes.as_ref(), vec![7; BLOCK_BYTES]);
        runtime
            .update(|state| state.audio_capture_sources.clear())
            .await;
        assert_eq!(
            next_text(&mut socket, "ended").await["reason"],
            "source_disabled"
        );
        next_text(&mut socket, "closed").await;
        drop(socket);
        stop.send(true).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn utterances_enforces_token_destination_and_revocation_on_live_subscription() {
        let directory =
            std::env::temp_dir().join(format!("tarsier-utterances-auth-{}", rand::random::<u64>()));
        let auth = Auth::new(directory.join("auth.json"), "worker-token").unwrap();
        auth.reset_password("test-password-long-enough").unwrap();
        let (url, runtime, audio, stop, server) = test_server_with_auth(Some(auth)).await;
        assert!(connect_async(&url).await.is_err());
        let base = url
            .replace("ws://", "http://")
            .replace("/api/v1/audio/utterances", "");
        let client = reqwest::Client::new();
        let login = client
            .post(format!("{base}/api/v1/auth/login"))
            .header("x-tarsier-request", "1")
            .json(&json!({"password":"test-password-long-enough"}))
            .send()
            .await
            .unwrap();
        assert!(login.status().is_success());
        let cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        for destination in ["mcp", "api"] {
            let token: Value = client
                .post(format!("{base}/api/v1/auth/tokens"))
                .header("x-tarsier-request", "1")
                .header("cookie", &cookie)
                .json(&json!({"name":"utterances-test", "destinations":[destination]}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let mut request = url.clone().into_client_request().unwrap();
            request.headers_mut().insert(
                "authorization",
                format!("Bearer {}", token["token"].as_str().unwrap())
                    .parse()
                    .unwrap(),
            );
            let connected = connect_async(request).await;
            if destination == "mcp" {
                assert!(connected.is_err());
                continue;
            }
            let (mut socket, _) = connected.unwrap();
            socket
                .send(ClientFrame::Text(
                    serde_json::to_string(&subscription(300)).unwrap().into(),
                ))
                .await
                .unwrap();
            next_text(&mut socket, "subscribed").await;
            let audio_task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                loop {
                    tick.tick().await;
                    audio.publish_test_audio("test-mic", Packet::Audio(frame(Instant::now(), 1)));
                }
            });
            runtime
                .emit("gesture.phone_near_mouth.started", "test", None, json!({}))
                .await;
            next_text(&mut socket, "started").await;
            let response = client
                .post(format!(
                    "{base}/api/v1/auth/tokens/{}/revoke",
                    token["id"].as_str().unwrap()
                ))
                .header("x-tarsier-request", "1")
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            assert_eq!(
                next_text(&mut socket, "ended").await["reason"],
                "authorization_revoked"
            );
            next_text(&mut socket, "closed").await;
            audio_task.abort();
            drop(socket);
            break;
        }
        stop.send(true).unwrap();
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TARSIER_TEST_PIPER_MODEL, Piper, ffmpeg and the Whisper client environment"]
    async fn utterances_real_whisper_transcribes_server_gated_speech_with_pre_roll() {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        async fn process(mut command: tokio::process::Command, bytes: Vec<u8>) -> Vec<u8> {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let writer = tokio::spawn(async move {
                input.write_all(&bytes).await.unwrap();
            });
            let result = child.wait_with_output().await.unwrap();
            writer.await.unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        }

        let model = std::env::var("TARSIER_TEST_PIPER_MODEL").unwrap();
        let model_config: Value =
            serde_json::from_slice(&std::fs::read(format!("{model}.json")).unwrap()).unwrap();
        let rate = model_config["audio"]["sample_rate"]
            .as_u64()
            .unwrap()
            .to_string();
        let mut piper = tokio::process::Command::new("piper");
        piper.args(["--model", &model, "--output-raw"]);
        let speech = process(piper, b"Bonjour, ceci est un test de transmission. Je parle pendant le geste et je termine ma phrase.\n".to_vec()).await;
        let mut ffmpeg = tokio::process::Command::new("ffmpeg");
        ffmpeg.args([
            "-v", "error", "-f", "s16le", "-ar", &rate, "-ac", "1", "-i", "pipe:0", "-ar", "48000",
            "-ac", "2", "-f", "s16le", "pipe:1",
        ]);
        let mut pcm = process(ffmpeg, speech).await;
        pcm.resize(pcm.len().div_ceil(BLOCK_BYTES) * BLOCK_BYTES, 0);
        let (url, runtime, audio, stop, server) = test_server().await;
        let base = url
            .replace("ws://", "http://")
            .replace("/api/v1/audio/utterances", "");
        let mut client = tokio::process::Command::new("uv")
            .args([
                "run",
                "--project",
                "clients/whisper",
                "tarsier-whisper",
                "--url",
                &base,
                "--source",
                "test-mic",
                "--pre-roll-ms",
                "300",
                "--json",
            ])
            .env("TARSIER_API_TOKEN", "local-test-token")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(client.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("Whisper client closed before subscription");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["type"] == "subscribed" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let speech_task = tokio::spawn(async move {
            // Start the event after speech has begun: pre-roll must recover its beginning.
            for (index, block) in pcm.chunks_exact(BLOCK_BYTES).enumerate() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                audio.publish_test_audio(
                    "test-mic",
                    Packet::Audio(Frame {
                        captured: Instant::now(),
                        pcm: Arc::new(block.to_vec()),
                    }),
                );
                if index == 9 {
                    runtime
                        .emit("gesture.phone_near_mouth.started", "test", None, json!({}))
                        .await;
                }
            }
            runtime
                .emit("gesture.phone_near_mouth.ended", "test", None, json!({}))
                .await;
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                audio.publish_test_audio("test-mic", Packet::Audio(frame(Instant::now(), 0)));
            }
        });
        let final_text = tokio::time::timeout(Duration::from_secs(30), async {
            let mut partials = 0;
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("Whisper client closed without final text");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["type"] == "partial" {
                    partials += 1;
                }
                if value["type"] == "final" {
                    assert!(partials > 0);
                    assert_eq!(value["reason"], "condition");
                    break value["text"].as_str().unwrap().to_lowercase();
                }
            }
        })
        .await
        .unwrap();
        println!("Full server-to-Whisper transcription: {final_text}");
        assert!(final_text.contains("bonjour") && final_text.contains("phrase"));
        speech_task.await.unwrap();
        stop.send(true).unwrap();
        assert!(client.wait().await.unwrap().success());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn utterances_rejects_unavailable_sources_and_invalid_subscriptions() {
        let (url, _, _, stop, server) = test_server().await;
        for source in ["unknown", "tarsier_microphone"] {
            let (mut socket, _) = connect_async(url.clone().into_client_request().unwrap())
                .await
                .unwrap();
            let mut request = subscription(300);
            request.source = source.into();
            socket
                .send(ClientFrame::Text(
                    serde_json::to_string(&request).unwrap().into(),
                ))
                .await
                .unwrap();
            assert_eq!(
                next_text(&mut socket, "error").await["code"],
                "source_not_enabled"
            );
        }
        stop.send(true).unwrap();
        server.await.unwrap();
    }
}
