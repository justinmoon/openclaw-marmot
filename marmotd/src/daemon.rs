use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, anyhow};
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::*;
use pika_media::codec_opus::{OpusCodec, OpusPacket};
use pika_media::session::{InMemoryRelay, MediaFrame, MediaSession, SessionConfig};
use pika_media::tracks::{TrackAddress, broadcast_path};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::call_stt::{OpusToTranscriptPipeline, transcriber_from_env};

const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum InCmd {
    PublishKeypackage {
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        relays: Vec<String>,
    },
    SetRelays {
        #[serde(default)]
        request_id: Option<String>,
        relays: Vec<String>,
    },
    ListPendingWelcomes {
        #[serde(default)]
        request_id: Option<String>,
    },
    AcceptWelcome {
        #[serde(default)]
        request_id: Option<String>,
        wrapper_event_id: String,
    },
    ListGroups {
        #[serde(default)]
        request_id: Option<String>,
    },
    SendMessage {
        #[serde(default)]
        request_id: Option<String>,
        nostr_group_id: String,
        content: String,
    },
    AcceptCall {
        #[serde(default)]
        request_id: Option<String>,
        call_id: String,
    },
    RejectCall {
        #[serde(default)]
        request_id: Option<String>,
        call_id: String,
        #[serde(default = "default_reject_reason")]
        reason: String,
    },
    EndCall {
        #[serde(default)]
        request_id: Option<String>,
        call_id: String,
        #[serde(default = "default_end_reason")]
        reason: String,
    },
    Shutdown {
        #[serde(default)]
        request_id: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutMsg {
    Ready {
        protocol_version: u32,
        pubkey: String,
        npub: String,
    },
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        code: String,
        message: String,
    },
    KeypackagePublished {
        event_id: String,
    },
    WelcomeReceived {
        wrapper_event_id: String,
        welcome_event_id: String,
        from_pubkey: String,
        nostr_group_id: String,
        group_name: String,
    },
    GroupJoined {
        nostr_group_id: String,
        mls_group_id: String,
    },
    MessageReceived {
        nostr_group_id: String,
        from_pubkey: String,
        content: String,
        created_at: u64,
        message_id: String,
    },
    CallInviteReceived {
        call_id: String,
        from_pubkey: String,
        nostr_group_id: String,
    },
    CallSessionStarted {
        call_id: String,
        nostr_group_id: String,
        from_pubkey: String,
    },
    CallSessionEnded {
        call_id: String,
        reason: String,
    },
    CallDebug {
        call_id: String,
        tx_frames: u64,
        rx_frames: u64,
        rx_dropped: u64,
    },
    CallTranscriptFinal {
        call_id: String,
        text: String,
    },
}

fn default_reject_reason() -> String {
    "declined".to_string()
}

fn default_end_reason() -> String {
    "user_hangup".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CallSessionParams {
    moq_url: String,
    broadcast_base: String,
    tracks: Vec<CallTrackSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CallTrackSpec {
    name: String,
    codec: String,
    sample_rate: u32,
    channels: u8,
    frame_ms: u16,
}

#[derive(Debug, Clone)]
struct PendingCallInvite {
    call_id: String,
    from_pubkey: String,
    nostr_group_id: String,
    session: CallSessionParams,
}

#[derive(Debug)]
struct ActiveEchoCall {
    call_id: String,
    nostr_group_id: String,
    worker: EchoWorker,
}

#[derive(Debug)]
enum CallWorkerEvent {
    TranscriptFinal { call_id: String, text: String },
}

#[derive(Debug)]
struct EchoWorker {
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl EchoWorker {
    async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
    }
}

fn call_relay_pool() -> &'static Mutex<HashMap<String, InMemoryRelay>> {
    static RELAYS: OnceLock<Mutex<HashMap<String, InMemoryRelay>>> = OnceLock::new();
    RELAYS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn relay_key(params: &CallSessionParams) -> String {
    format!("{}|{}", params.moq_url, params.broadcast_base)
}

fn shared_call_relay(params: &CallSessionParams) -> InMemoryRelay {
    let mut relays = call_relay_pool().lock().expect("call relay pool poisoned");
    relays.entry(relay_key(params)).or_default().clone()
}

fn default_audio_call_session(call_id: &str) -> CallSessionParams {
    CallSessionParams {
        moq_url: "https://moq.local/anon".to_string(),
        broadcast_base: format!("pika/calls/{call_id}"),
        tracks: vec![CallTrackSpec {
            name: "audio0".to_string(),
            codec: "opus".to_string(),
            sample_rate: 48_000,
            channels: 1,
            frame_ms: 20,
        }],
    }
}

#[derive(Debug, Clone)]
pub struct AudioEchoSmokeStats {
    pub sent_frames: u64,
    pub echoed_frames: u64,
}

fn out_error(request_id: Option<String>, code: &str, message: impl Into<String>) -> OutMsg {
    OutMsg::Error {
        request_id,
        code: code.to_string(),
        message: message.into(),
    }
}

fn out_ok(request_id: Option<String>, result: Option<serde_json::Value>) -> OutMsg {
    OutMsg::Ok { request_id, result }
}

#[derive(Debug)]
enum ParsedCallSignal {
    Invite {
        call_id: String,
        session: CallSessionParams,
    },
    Accept {
        call_id: String,
        session: CallSessionParams,
    },
    Reject {
        call_id: String,
        reason: String,
    },
    End {
        call_id: String,
        reason: String,
    },
}

#[derive(Debug, Deserialize)]
struct CallSignalEnvelope {
    v: u32,
    ns: String,
    #[serde(rename = "type")]
    msg_type: String,
    call_id: String,
    #[allow(dead_code)]
    #[serde(default)]
    ts_ms: i64,
    #[serde(default)]
    body: serde_json::Value,
}

enum OutgoingCallSignal<'a> {
    Accept(&'a CallSessionParams),
    Reject { reason: &'a str },
    End { reason: &'a str },
}

fn parse_call_signal(content: &str) -> Option<ParsedCallSignal> {
    let env: CallSignalEnvelope = serde_json::from_str(content).ok()?;
    if env.v != 1 || env.ns != "pika.call" {
        return None;
    }
    match env.msg_type.as_str() {
        "call.invite" => {
            let session: CallSessionParams = serde_json::from_value(env.body).ok()?;
            Some(ParsedCallSignal::Invite {
                call_id: env.call_id,
                session,
            })
        }
        "call.accept" => {
            let session: CallSessionParams = serde_json::from_value(env.body).ok()?;
            Some(ParsedCallSignal::Accept {
                call_id: env.call_id,
                session,
            })
        }
        "call.reject" => {
            let reason = env
                .body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("declined")
                .to_string();
            Some(ParsedCallSignal::Reject {
                call_id: env.call_id,
                reason,
            })
        }
        "call.end" => {
            let reason = env
                .body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("remote_end")
                .to_string();
            Some(ParsedCallSignal::End {
                call_id: env.call_id,
                reason,
            })
        }
        _ => None,
    }
}

fn build_call_signal_json(call_id: &str, signal: OutgoingCallSignal<'_>) -> anyhow::Result<String> {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let value = match signal {
        OutgoingCallSignal::Accept(session) => json!({
            "v": 1,
            "ns": "pika.call",
            "type": "call.accept",
            "call_id": call_id,
            "ts_ms": ts_ms,
            "body": session,
        }),
        OutgoingCallSignal::Reject { reason } => json!({
            "v": 1,
            "ns": "pika.call",
            "type": "call.reject",
            "call_id": call_id,
            "ts_ms": ts_ms,
            "body": { "reason": reason },
        }),
        OutgoingCallSignal::End { reason } => json!({
            "v": 1,
            "ns": "pika.call",
            "type": "call.end",
            "call_id": call_id,
            "ts_ms": ts_ms,
            "body": { "reason": reason },
        }),
    };
    serde_json::to_string(&value).context("serialize call signal")
}

async fn publish_group_message(
    client: &Client,
    relay_urls: &[RelayUrl],
    mdk: &MDK<MdkSqliteStorage>,
    keys: &Keys,
    nostr_group_id: &str,
    content: String,
    label: &str,
) -> anyhow::Result<()> {
    let group_id_bytes = hex::decode(nostr_group_id).context("decode nostr_group_id")?;
    if group_id_bytes.len() != 32 {
        return Err(anyhow!("nostr_group_id must be 32 bytes hex"));
    }
    let groups = mdk.get_groups().context("get_groups")?;
    let found = groups
        .iter()
        .find(|g| g.nostr_group_id.as_slice() == group_id_bytes.as_slice());
    let Some(group) = found else {
        return Err(anyhow!(
            "group not found for nostr_group_id={nostr_group_id}"
        ));
    };
    let rumor = EventBuilder::new(Kind::Custom(9), content).build(keys.public_key());
    let msg_event = mdk
        .create_message(&group.mls_group_id, rumor)
        .context("create_message")?;
    let msg_tags: Tags = msg_event
        .tags
        .clone()
        .into_iter()
        .filter(|t| !matches!(t.kind(), TagKind::Protected))
        .collect();
    let msg_event = EventBuilder::new(msg_event.kind, msg_event.content)
        .tags(msg_tags)
        .sign_with_keys(keys)
        .context("sign call signal event")?;
    publish_and_confirm_multi(client, relay_urls, &msg_event, label).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_call_signal(
    client: &Client,
    relay_urls: &[RelayUrl],
    mdk: &MDK<MdkSqliteStorage>,
    keys: &Keys,
    nostr_group_id: &str,
    call_id: &str,
    signal: OutgoingCallSignal<'_>,
    label: &str,
) -> anyhow::Result<()> {
    let payload = build_call_signal_json(call_id, signal)?;
    publish_group_message(
        client,
        relay_urls,
        mdk,
        keys,
        nostr_group_id,
        payload,
        label,
    )
    .await
}

fn call_audio_track_spec(session: &CallSessionParams) -> Option<&CallTrackSpec> {
    session
        .tracks
        .iter()
        .find(|t| t.codec.eq_ignore_ascii_case("opus") && t.channels > 0 && t.sample_rate > 0)
}

fn start_stt_worker(
    call_id: &str,
    session: &CallSessionParams,
    peer_pubkey_hex: &str,
    out_tx: mpsc::UnboundedSender<OutMsg>,
    call_evt_tx: mpsc::UnboundedSender<CallWorkerEvent>,
) -> anyhow::Result<EchoWorker> {
    let relay = shared_call_relay(session);
    start_stt_worker_with_relay(
        call_id,
        session,
        relay,
        peer_pubkey_hex,
        out_tx,
        call_evt_tx,
    )
}

fn start_stt_worker_with_relay(
    call_id: &str,
    session: &CallSessionParams,
    relay: InMemoryRelay,
    peer_pubkey_hex: &str,
    out_tx: mpsc::UnboundedSender<OutMsg>,
    call_evt_tx: mpsc::UnboundedSender<CallWorkerEvent>,
) -> anyhow::Result<EchoWorker> {
    let Some(track) = call_audio_track_spec(session) else {
        return Err(anyhow!("call session missing opus audio track"));
    };

    let mut media = MediaSession::with_relay(
        SessionConfig {
            moq_url: session.moq_url.clone(),
        },
        relay,
    );
    media.connect();

    let subscribe_track = TrackAddress {
        broadcast_path: broadcast_path(&session.broadcast_base, peer_pubkey_hex)
            .map_err(|e| anyhow!("invalid peer broadcast path: {e}"))?,
        track_name: track.name.clone(),
    };
    let rx = media
        .subscribe(&subscribe_track)
        .context("subscribe peer track for stt")?;

    let mut pipeline = OpusToTranscriptPipeline::new(
        track.sample_rate,
        track.channels,
        transcriber_from_env().context("initialize stt transcriber")?,
    )
    .context("initialize stt pipeline")?;

    let call_id = call_id.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_task = stop.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut rx_frames = 0u64;
        let mut ticks = 0u64;
        while !stop_for_task.load(Ordering::Relaxed) {
            while let Ok(inbound) = rx.try_recv() {
                rx_frames = rx_frames.saturating_add(1);
                match pipeline.ingest_packet(OpusPacket(inbound.payload)) {
                    Ok(Some(text)) => {
                        let _ = call_evt_tx.send(CallWorkerEvent::TranscriptFinal {
                            call_id: call_id.clone(),
                            text,
                        });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(
                            "[marmotd] stt ingest failed call_id={} err={err:#}",
                            call_id
                        );
                    }
                }
            }

            ticks = ticks.saturating_add(1);
            if ticks.is_multiple_of(5) {
                let _ = out_tx.send(OutMsg::CallDebug {
                    call_id: call_id.clone(),
                    tx_frames: 0,
                    rx_frames,
                    rx_dropped: 0,
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        match pipeline.flush() {
            Ok(Some(text)) => {
                let _ = call_evt_tx.send(CallWorkerEvent::TranscriptFinal {
                    call_id: call_id.clone(),
                    text,
                });
            }
            Ok(None) => {}
            Err(err) => {
                warn!("[marmotd] stt flush failed call_id={} err={err:#}", call_id);
            }
        }
    });

    Ok(EchoWorker { stop, task })
}

fn start_echo_worker_with_relay(
    call_id: &str,
    session: &CallSessionParams,
    relay: InMemoryRelay,
    local_pubkey_hex: &str,
    peer_pubkey_hex: &str,
    out_tx: mpsc::UnboundedSender<OutMsg>,
) -> anyhow::Result<EchoWorker> {
    let mut media = MediaSession::with_relay(
        SessionConfig {
            moq_url: session.moq_url.clone(),
        },
        relay,
    );
    media.connect();

    let publish_track = TrackAddress {
        broadcast_path: broadcast_path(&session.broadcast_base, local_pubkey_hex)
            .map_err(|e| anyhow!("invalid local broadcast path: {e}"))?,
        track_name: "audio0".to_string(),
    };
    let subscribe_track = TrackAddress {
        broadcast_path: broadcast_path(&session.broadcast_base, peer_pubkey_hex)
            .map_err(|e| anyhow!("invalid peer broadcast path: {e}"))?,
        track_name: "audio0".to_string(),
    };
    let rx = media
        .subscribe(&subscribe_track)
        .context("subscribe peer track")?;

    let call_id = call_id.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_task = stop.clone();
    let task = tokio::spawn(async move {
        let codec = OpusCodec;
        let mut seq = 0u64;
        let mut tx_frames = 0u64;
        let mut rx_frames = 0u64;
        let mut ticks = 0u64;
        while !stop_for_task.load(Ordering::Relaxed) {
            while let Ok(inbound) = rx.try_recv() {
                rx_frames = rx_frames.saturating_add(1);
                let pcm = codec.decode_to_pcm_i16(&OpusPacket(inbound.payload));
                let packet = codec.encode_pcm_i16(&pcm);
                let frame = MediaFrame {
                    seq,
                    timestamp_us: seq.saturating_mul(20_000),
                    keyframe: true,
                    payload: packet.0,
                };
                if media.publish(&publish_track, frame).is_ok() {
                    tx_frames = tx_frames.saturating_add(1);
                    seq = seq.saturating_add(1);
                }
            }

            ticks = ticks.saturating_add(1);
            if ticks.is_multiple_of(5) {
                let _ = out_tx.send(OutMsg::CallDebug {
                    call_id: call_id.clone(),
                    tx_frames,
                    rx_frames,
                    rx_dropped: 0,
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    Ok(EchoWorker { stop, task })
}

pub async fn run_audio_echo_smoke(frame_count: u64) -> anyhow::Result<AudioEchoSmokeStats> {
    let call_id = "550e8400-e29b-41d4-a716-446655440000";
    let session = default_audio_call_session(call_id);
    let relay = InMemoryRelay::new();

    let mut peer = MediaSession::with_relay(
        SessionConfig {
            moq_url: session.moq_url.clone(),
        },
        relay.clone(),
    );
    let mut observer = MediaSession::with_relay(
        SessionConfig {
            moq_url: session.moq_url.clone(),
        },
        relay.clone(),
    );
    peer.connect();
    observer.connect();

    let peer_pubkey_hex = "11b9a894813efe60d39f8621ae9dc4c6d26de4732411c1cdf4bb15e88898a19c";
    let bot_pubkey_hex = "2284fc7b932b5dbbdaa2185c76a4e17a2ef928d4a82e29b812986b454b957f8f";
    let peer_track = TrackAddress {
        broadcast_path: broadcast_path(&session.broadcast_base, peer_pubkey_hex)
            .map_err(|e| anyhow!("peer broadcast path invalid: {e}"))?,
        track_name: "audio0".to_string(),
    };
    let bot_track = TrackAddress {
        broadcast_path: broadcast_path(&session.broadcast_base, bot_pubkey_hex)
            .map_err(|e| anyhow!("bot broadcast path invalid: {e}"))?,
        track_name: "audio0".to_string(),
    };
    let echoed_rx = observer
        .subscribe(&bot_track)
        .context("subscribe bot audio track")?;

    let (out_tx, _out_rx) = mpsc::unbounded_channel::<OutMsg>();
    let worker = start_echo_worker_with_relay(
        call_id,
        &session,
        relay,
        bot_pubkey_hex,
        peer_pubkey_hex,
        out_tx,
    )
    .context("start echo worker")?;

    let codec = OpusCodec;
    let mut sent_frames = 0u64;
    for i in 0..frame_count {
        let pcm = vec![i as i16, (i as i16).saturating_mul(-1)];
        let packet = codec.encode_pcm_i16(&pcm);
        let frame = MediaFrame {
            seq: i,
            timestamp_us: i * 20_000,
            keyframe: true,
            payload: packet.0,
        };
        let delivered = peer
            .publish(&peer_track, frame)
            .context("publish peer frame")?;
        if delivered > 0 {
            sent_frames = sent_frames.saturating_add(1);
        }
    }

    let mut echoed_frames = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while echoed_frames < sent_frames && tokio::time::Instant::now() < deadline {
        while echoed_rx.try_recv().is_ok() {
            echoed_frames = echoed_frames.saturating_add(1);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    worker.stop().await;

    if echoed_frames != sent_frames {
        return Err(anyhow!(
            "audio echo frame mismatch: sent={sent_frames} echoed={echoed_frames}"
        ));
    }

    Ok(AudioEchoSmokeStats {
        sent_frames,
        echoed_frames,
    })
}

async fn publish_and_confirm_multi(
    client: &Client,
    relays: &[RelayUrl],
    event: &Event,
    label: &str,
) -> anyhow::Result<RelayUrl> {
    let out = client
        .send_event_to(relays.to_vec(), event)
        .await
        .with_context(|| format!("send_event_to failed ({label})"))?;
    if out.success.is_empty() {
        return Err(anyhow!(
            "event publish had no successful relays ({label}): {out:?}"
        ));
    }

    // Confirm we can fetch it back from at least one relay that reported success.
    for relay_url in out.success.iter().cloned() {
        let fetched = client
            .fetch_events_from(
                [relay_url.clone()],
                Filter::new().id(event.id),
                Duration::from_secs(5),
            )
            .await
            .with_context(|| format!("fetch_events_from failed ({label}) relay={relay_url}"))?;
        if fetched.iter().any(|e| e.id == event.id) {
            return Ok(relay_url);
        }
    }

    Err(anyhow!(
        "published event not found on any successful relay after send ({label}) id={}",
        event.id
    ))
}

async fn stdout_writer(mut rx: mpsc::UnboundedReceiver<OutMsg>) -> anyhow::Result<()> {
    let mut stdout = tokio::io::stdout();
    while let Some(msg) = rx.recv().await {
        let line = serde_json::to_string(&msg).context("encode out msg")?;
        stdout.write_all(line.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn parse_relay_list(relay: &str, relays_override: &[String]) -> anyhow::Result<Vec<RelayUrl>> {
    let mut out = Vec::new();
    if relays_override.is_empty() {
        out.push(RelayUrl::parse(relay).context("parse relay url")?);
        return Ok(out);
    }
    for r in relays_override {
        let trimmed = r.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(RelayUrl::parse(trimmed).with_context(|| format!("parse relay url: {trimmed}"))?);
    }
    if out.is_empty() {
        return Err(anyhow!("relays list is empty"));
    }
    Ok(out)
}

fn event_h_tag_hex(ev: &Event) -> Option<String> {
    for t in ev.tags.iter() {
        if t.kind() == TagKind::h()
            && let Some(v) = t.content()
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

pub async fn daemon_main(
    relay: &str,
    state_dir: &Path,
    giftwrap_lookback_sec: u64,
    allow_pubkeys: &[String],
) -> anyhow::Result<()> {
    crate::ensure_dir(state_dir).context("create state dir")?;

    crate::check_relay_ready(relay, Duration::from_secs(90))
        .await
        .with_context(|| format!("relay readiness check failed for {relay}"))?;

    let keys = crate::load_or_create_keys(&state_dir.join("identity.json"))?;
    let pubkey_hex = keys.public_key().to_hex().to_lowercase();
    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| "<npub_err>".to_string());

    let (out_tx, out_rx) = mpsc::unbounded_channel::<OutMsg>();
    tokio::spawn(async move {
        if let Err(err) = stdout_writer(out_rx).await {
            eprintln!("[marmotd] stdout writer failed: {err:#}");
        }
    });

    // Build pubkey allowlist. Empty = open (allow all).
    let allowlist: HashSet<String> = allow_pubkeys
        .iter()
        .map(|pk| pk.trim().to_lowercase())
        .filter(|pk| !pk.is_empty())
        .collect();
    let is_open = allowlist.is_empty();
    if is_open {
        eprintln!(
            "[marmotd] WARNING: no --allow-pubkey specified, accepting all senders (open mode)"
        );
    } else {
        eprintln!("[marmotd] allowlist: {} pubkeys", allowlist.len());
        for pk in &allowlist {
            eprintln!("[marmotd]   allow: {pk}");
        }
    }
    let sender_allowed = |pubkey_hex: &str| -> bool {
        is_open || allowlist.contains(&pubkey_hex.trim().to_lowercase())
    };

    out_tx
        .send(OutMsg::Ready {
            protocol_version: PROTOCOL_VERSION,
            pubkey: pubkey_hex.clone(),
            npub,
        })
        .ok();

    let mut relay_urls = vec![RelayUrl::parse(relay).context("parse relay url")?];
    let client = crate::connect_client(&keys, relay).await?;
    let mdk = crate::new_mdk(state_dir, "daemon")?;

    let mut rx = client.notifications();

    // Subscribe to welcomes (GiftWrap kind 1059) addressed to us.
    // NOTE: `pubkey()` filter matches the event author, not the recipient.
    // GiftWraps can be authored by anyone, so we must filter by the recipient `p` tag.
    let since = Timestamp::now() - Duration::from_secs(giftwrap_lookback_sec);
    let gift_filter = Filter::new()
        .kind(Kind::GiftWrap)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::P), pubkey_hex.clone())
        .since(since)
        .limit(200);
    let gift_sub = client.subscribe(gift_filter, None).await?;

    // Track which wrapper events and group message wrapper events we've already processed.
    let mut seen_welcomes: HashSet<EventId> = HashSet::new();
    let mut seen_group_events: HashSet<EventId> = HashSet::new();

    // Track group subscriptions.
    let mut group_subs: HashMap<SubscriptionId, String> = HashMap::new();
    let mut pending_call_invites: HashMap<String, PendingCallInvite> = HashMap::new();
    let mut active_call: Option<ActiveEchoCall> = None;
    let (call_evt_tx, mut call_evt_rx) = mpsc::unbounded_channel::<CallWorkerEvent>();

    // On startup, subscribe to any groups already present in state, so the daemon is restart-safe.
    if let Ok(groups) = mdk.get_groups() {
        for g in groups.iter() {
            let nostr_group_id_hex = hex::encode(g.nostr_group_id);
            match crate::subscribe_group_msgs(&client, &nostr_group_id_hex).await {
                Ok(sid) => {
                    group_subs.insert(sid.clone(), nostr_group_id_hex.clone());
                }
                Err(err) => {
                    warn!(
                        "[marmotd] subscribe existing group failed nostr_group_id={nostr_group_id_hex} err={err:#}"
                    );
                }
            }
        }
    }

    // stdin command reader
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<InCmd>();
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = tokio::io::BufReader::new(stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<InCmd>(trimmed) {
                Ok(cmd) => {
                    cmd_tx.send(cmd).ok();
                }
                Err(err) => {
                    eprintln!("[marmotd] invalid cmd json: {err} line={trimmed}");
                }
            }
        }
    });

    let mut shutdown = false;
    while !shutdown {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    InCmd::PublishKeypackage { request_id, relays } => {
                        let selected = match parse_relay_list(relay, &relays) {
                            Ok(v) => v,
                            Err(e) => {
                                out_tx.send(out_error(request_id, "bad_relays", e.to_string())).ok();
                                continue;
                            }
                        };
                        relay_urls = selected.clone();
                        // Ensure client knows about relays.
                        for r in selected.iter() {
                            let _ = client.add_relay(r.clone()).await;
                        }
                        client.connect().await;

                        let (kp_content, kp_tags) = match mdk
                            .create_key_package_for_event(&keys.public_key(), selected.clone())
                        {
                            Ok(v) => v,
                            Err(e) => {
                                out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}"))).ok();
                                continue;
                            }
                        };
                        // Many public relays reject NIP-70 "protected" events. Keypackages and MLS
                        // wrapper events are safe to publish without protection, so strip it to keep
                        // public-relay deployments working.
                        let kp_tags: Tags = kp_tags
                            .into_iter()
                            .filter(|t| !matches!(t.kind(), TagKind::Protected))
                            .collect();
                        let ev = match EventBuilder::new(Kind::MlsKeyPackage, kp_content)
                            .tags(kp_tags)
                            .sign_with_keys(&keys)
                        {
                            Ok(v) => v,
                            Err(e) => {
                                out_tx.send(out_error(request_id, "sign_failed", format!("{e:#}"))).ok();
                                continue;
                            }
                        };

                        match publish_and_confirm_multi(&client, &selected, &ev, "keypackage").await {
                            Ok(_relay_confirmed) => {
                                out_tx.send(out_ok(request_id, Some(json!({"event_id": ev.id.to_hex()})))).ok();
                                out_tx.send(OutMsg::KeypackagePublished { event_id: ev.id.to_hex() }).ok();
                            }
                            Err(e) => {
                                out_tx.send(out_error(request_id, "publish_failed", format!("{e:#}"))).ok();
                            }
                        };
                    }
                    InCmd::SetRelays { request_id, relays } => {
                        match parse_relay_list(relay, &relays) {
                            Ok(v) => {
                                relay_urls = v.clone();
                                for r in v.iter() {
                                    let _ = client.add_relay(r.clone()).await;
                                }
                                client.connect().await;
                                out_tx.send(out_ok(request_id, Some(json!({"relays": v.iter().map(|r| r.to_string()).collect::<Vec<_>>()})))).ok();
                            }
                            Err(e) => {
                                out_tx.send(out_error(request_id, "bad_relays", e.to_string())).ok();
                            }
                        }
                    }
                    InCmd::ListPendingWelcomes { request_id } => {
                        match mdk.get_pending_welcomes(None) {
                            Ok(list) => {
                                let out = list
                                    .iter()
                                    .map(|w| {
                                    json!({
                                        "wrapper_event_id": w.wrapper_event_id.to_hex(),
                                        "welcome_event_id": w.id.to_hex(),
                                        "from_pubkey": w.welcomer.to_hex().to_lowercase(),
                                        "nostr_group_id": hex::encode(w.nostr_group_id),
                                        "group_name": w.group_name,
                                    })
                                    })
                                    .collect::<Vec<_>>();
                                let _ = out_tx.send(out_ok(request_id, Some(json!({ "welcomes": out }))));
                            }
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}")));
                            }
                        }
                    }
                    InCmd::AcceptWelcome { request_id, wrapper_event_id } => {
                        let wrapper = match EventId::from_hex(&wrapper_event_id) {
                            Ok(id) => id,
                            Err(_) => {
                                out_tx.send(out_error(request_id, "bad_event_id", "wrapper_event_id must be hex")).ok();
                                continue;
                            }
                        };
                        match mdk.get_pending_welcomes(None) {
                            Ok(list) => {
                                let found = list.into_iter().find(|w| w.wrapper_event_id == wrapper);
                                let Some(w) = found else {
                                    out_tx.send(out_error(request_id, "not_found", "pending welcome not found")).ok();
                                    continue;
                                };
                                let nostr_group_id_hex = hex::encode(w.nostr_group_id);
                                let mls_group_id_hex = hex::encode(w.mls_group_id.as_slice());
                                match mdk.accept_welcome(&w) {
                                    Ok(_) => {
                                        // Subscribe to group messages for this group.
                                        match crate::subscribe_group_msgs(&client, &nostr_group_id_hex).await {
                                            Ok(sid) => {
                                                group_subs.insert(sid.clone(), nostr_group_id_hex.clone());
                                            }
                                            Err(err) => {
                                                warn!("[marmotd] subscribe group msgs failed: {err:#}");
                                            }
                                        }

                                        // Backfill recent group messages, but dedupe by wrapper id.
                                        if let Some(relay0) = relay_urls.first().cloned() {
                                            let filter = Filter::new()
                                                .kind(Kind::MlsGroupMessage)
                                                .custom_tag(SingleLetterTag::lowercase(Alphabet::H), &nostr_group_id_hex)
                                                .since(Timestamp::now() - Duration::from_secs(60 * 60))
                                                .limit(200);
                                            if let Ok(events) = client.fetch_events_from([relay0], filter, Duration::from_secs(10)).await {
                                                for ev in events.iter() {
                                                    if !seen_group_events.insert(ev.id) {
                                                        continue;
                                                    }
                                                    if let Ok(MessageProcessingResult::ApplicationMessage(msg)) = mdk.process_message(ev) {
                                                        if !sender_allowed(&msg.pubkey.to_hex()) {
                                                            continue;
                                                        }
                                                        out_tx.send(OutMsg::MessageReceived{
                                                            nostr_group_id: event_h_tag_hex(ev).unwrap_or_else(|| nostr_group_id_hex.clone()),
                                                            from_pubkey: msg.pubkey.to_hex().to_lowercase(),
                                                            content: msg.content,
                                                            created_at: msg.created_at.as_secs(),
                                                            message_id: msg.id.to_hex(),
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }

                                        out_tx.send(out_ok(request_id, Some(json!({
                                            "nostr_group_id": nostr_group_id_hex,
                                            "mls_group_id": mls_group_id_hex,
                                        })))).ok();
                                        out_tx.send(OutMsg::GroupJoined { nostr_group_id: nostr_group_id_hex, mls_group_id: mls_group_id_hex }).ok();
                                    }
                                    Err(e) => {
                                        out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}"))).ok();
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}")));
                            }
                        }
                    }
                    InCmd::ListGroups { request_id } => {
                        match mdk.get_groups() {
                            Ok(gs) => {
                                let out = gs.iter().map(|g| {
                                    json!({
                                        "nostr_group_id": hex::encode(g.nostr_group_id),
                                        "mls_group_id": hex::encode(g.mls_group_id.as_slice()),
                                        "name": g.name,
                                        "description": g.description,
                                    })
                                }).collect::<Vec<_>>();
                                let _ = out_tx.send(out_ok(request_id, Some(json!({"groups": out}))));
                            }
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}")));
                            }
                        }
                    }
                    InCmd::SendMessage { request_id, nostr_group_id, content } => {
                        let group_id_bytes = match hex::decode(&nostr_group_id) {
                            Ok(b) => b,
                            Err(_) => {
                                out_tx.send(out_error(request_id, "bad_group_id", "nostr_group_id must be hex")).ok();
                                continue;
                            }
                        };
                        if group_id_bytes.len() != 32 {
                            out_tx.send(out_error(request_id, "bad_group_id", "nostr_group_id must be 32 bytes hex")).ok();
                            continue;
                        }
                        let groups = mdk.get_groups().context("get_groups")?;
                        let found = groups.iter().find(|g| g.nostr_group_id.as_slice() == group_id_bytes.as_slice());
                        let Some(g) = found else {
                            out_tx.send(out_error(request_id, "not_found", "group not found")).ok();
                            continue;
                        };
                        let mls_group_id = g.mls_group_id.clone();

                        let rumor = EventBuilder::new(Kind::Custom(9), content).build(keys.public_key());
                        let msg_event = match mdk.create_message(&mls_group_id, rumor) {
                            Ok(ev) => ev,
                            Err(e) => {
                                out_tx.send(out_error(request_id, "mdk_error", format!("{e:#}"))).ok();
                                continue;
                            }
                        };
                        let msg_tags: Tags = msg_event
                            .tags
                            .clone()
                            .into_iter()
                            .filter(|t| !matches!(t.kind(), TagKind::Protected))
                            .collect();
                        let msg_event = match EventBuilder::new(msg_event.kind, msg_event.content)
                            .tags(msg_tags)
                            .sign_with_keys(&keys)
                        {
                            Ok(ev) => ev,
                            Err(e) => {
                                out_tx.send(out_error(request_id, "sign_failed", format!("{e:#}"))).ok();
                                continue;
                            }
                        };

                        if relay_urls.is_empty() {
                            out_tx.send(out_error(request_id, "bad_relays", "no relays configured")).ok();
                            continue;
                        }
                        match publish_and_confirm_multi(&client, &relay_urls, &msg_event, "daemon_send").await {
                            Ok(_relay_confirmed) => {
                                let _ = out_tx.send(out_ok(request_id, Some(json!({"event_id": msg_event.id.to_hex()}))));
                            }
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "publish_failed", format!("{e:#}")));
                            }
                        }
                    }
                    InCmd::AcceptCall { request_id, call_id } => {
                        if active_call.is_some() {
                            let _ = out_tx.send(out_error(request_id, "busy", "call already active"));
                            continue;
                        }
                        let Some(invite) = pending_call_invites.remove(&call_id) else {
                            let _ = out_tx.send(out_error(request_id, "not_found", "pending call invite not found"));
                            continue;
                        };

                        match send_call_signal(
                            &client,
                            &relay_urls,
                            &mdk,
                            &keys,
                            &invite.nostr_group_id,
                            &invite.call_id,
                            OutgoingCallSignal::Accept(&invite.session),
                            "call_accept",
                        ).await {
                            Ok(()) => {}
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "publish_failed", format!("{e:#}")));
                                continue;
                            }
                        }

                        let worker = match start_stt_worker(
                            &invite.call_id,
                            &invite.session,
                            &invite.from_pubkey,
                            out_tx.clone(),
                            call_evt_tx.clone(),
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "runtime_error", format!("{e:#}")));
                                continue;
                            }
                        };

                        active_call = Some(ActiveEchoCall {
                            call_id: invite.call_id.clone(),
                            nostr_group_id: invite.nostr_group_id.clone(),
                            worker,
                        });
                        let _ = out_tx.send(out_ok(request_id, Some(json!({
                            "call_id": invite.call_id,
                            "nostr_group_id": invite.nostr_group_id,
                        }))));
                        let _ = out_tx.send(OutMsg::CallSessionStarted {
                            call_id: invite.call_id,
                            nostr_group_id: invite.nostr_group_id,
                            from_pubkey: invite.from_pubkey,
                        });
                    }
                    InCmd::RejectCall {
                        request_id,
                        call_id,
                        reason,
                    } => {
                        let Some(invite) = pending_call_invites.remove(&call_id) else {
                            let _ = out_tx.send(out_error(request_id, "not_found", "pending call invite not found"));
                            continue;
                        };
                        match send_call_signal(
                            &client,
                            &relay_urls,
                            &mdk,
                            &keys,
                            &invite.nostr_group_id,
                            &invite.call_id,
                            OutgoingCallSignal::Reject { reason: &reason },
                            "call_reject",
                        ).await {
                            Ok(()) => {
                                let _ = out_tx.send(out_ok(request_id, Some(json!({ "call_id": invite.call_id }))));
                            }
                            Err(e) => {
                                let _ = out_tx.send(out_error(request_id, "publish_failed", format!("{e:#}")));
                            }
                        }
                    }
                    InCmd::EndCall {
                        request_id,
                        call_id,
                        reason,
                    } => {
                        let Some(current) = active_call.take() else {
                            let _ = out_tx.send(out_error(request_id, "not_found", "active call not found"));
                            continue;
                        };
                        if current.call_id != call_id {
                            active_call = Some(current);
                            let _ = out_tx.send(out_error(request_id, "not_found", "active call id mismatch"));
                            continue;
                        }

                        let _ = send_call_signal(
                            &client,
                            &relay_urls,
                            &mdk,
                            &keys,
                            &current.nostr_group_id,
                            &call_id,
                            OutgoingCallSignal::End { reason: &reason },
                            "call_end",
                        )
                        .await;
                        current.worker.stop().await;
                        let _ = out_tx.send(out_ok(request_id, Some(json!({ "call_id": call_id }))));
                        let _ = out_tx.send(OutMsg::CallSessionEnded {
                            call_id,
                            reason,
                        });
                    }
                    InCmd::Shutdown { request_id } => {
                        out_tx.send(out_ok(request_id, None)).ok();
                        shutdown = true;
                    }
                }
            }
            call_evt = call_evt_rx.recv() => {
                let Some(call_evt) = call_evt else { continue; };
                match call_evt {
                    CallWorkerEvent::TranscriptFinal { call_id, text } => {
                        let Some(nostr_group_id) = active_call
                            .as_ref()
                            .filter(|c| c.call_id == call_id)
                            .map(|c| c.nostr_group_id.clone()) else {
                                continue;
                            };

                        let _ = out_tx.send(OutMsg::CallTranscriptFinal {
                            call_id: call_id.clone(),
                            text: text.clone(),
                        });
                        if let Err(err) = publish_group_message(
                            &client,
                            &relay_urls,
                            &mdk,
                            &keys,
                            &nostr_group_id,
                            text,
                            "call_transcript_final",
                        )
                        .await
                        {
                            warn!(
                                "[marmotd] call transcript publish failed call_id={} group={} err={err:#}",
                                call_id, nostr_group_id
                            );
                        }
                    }
                }
            }
            notification = rx.recv() => {
                let notification = match notification {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };

                let RelayPoolNotification::Event { subscription_id, event, .. } = notification else {
                    continue;
                };
                let event = *event;

                if subscription_id == gift_sub.val {
                    if event.kind != Kind::GiftWrap {
                        continue;
                    }
                    if !seen_welcomes.insert(event.id) {
                        continue;
                    }

                    // Unwrap and stage welcome in the MDK pending welcome store.
                    let unwrapped = match nostr_sdk::nostr::nips::nip59::extract_rumor(&keys, &event).await {
                        Ok(u) => u,
                        Err(e) => {
                            warn!("[marmotd] giftwrap unwrap failed id={} err={e:#}", event.id.to_hex());
                            continue;
                        }
                    };
                    if unwrapped.rumor.kind != Kind::MlsWelcome {
                        continue;
                    }

                    let wrapper_event_id = event.id;
                    let mut rumor = unwrapped.rumor;
                    let from = unwrapped.sender;

                    if !sender_allowed(&from.to_hex()) {
                        warn!("[marmotd] reject welcome (sender not allowed) from={}", from.to_hex());
                        continue;
                    }

                    if let Err(e) = mdk.process_welcome(&wrapper_event_id, &rumor) {
                        warn!("[marmotd] process_welcome failed wrapper_id={} err={e:#}", wrapper_event_id.to_hex());
                        continue;
                    }

                    // Read back the stored welcome record so we can surface group metadata.
                    let pending = match mdk.get_pending_welcomes(None) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("[marmotd] get_pending_welcomes failed err={e:#}");
                            continue;
                        }
                    };
                    let stored = pending.into_iter().find(|w| w.wrapper_event_id == wrapper_event_id);
                    let (nostr_group_id, group_name) = match stored {
                        Some(w) => (hex::encode(w.nostr_group_id), w.group_name),
                        None => ("".to_string(), "".to_string()),
                    };

                    out_tx.send(OutMsg::WelcomeReceived {
                        wrapper_event_id: wrapper_event_id.to_hex(),
                        welcome_event_id: rumor.id().to_hex(),
                        from_pubkey: from.to_hex().to_lowercase(),
                        nostr_group_id,
                        group_name,
                    }).ok();

                    continue;
                }

                if event.kind == Kind::MlsGroupMessage {
                    // Only process messages for subscriptions we created.
                    if !group_subs.contains_key(&subscription_id) {
                        continue;
                    }
                    if !seen_group_events.insert(event.id) {
                        continue;
                    }

                    let nostr_group_id = event_h_tag_hex(&event).unwrap_or_else(|| group_subs.get(&subscription_id).cloned().unwrap_or_default());
                    match mdk.process_message(&event) {
                        Ok(MessageProcessingResult::ApplicationMessage(msg)) => {
                            let sender_hex = msg.pubkey.to_hex().to_lowercase();
                            if !sender_allowed(&sender_hex) {
                                warn!("[marmotd] drop message (sender not allowed) from={sender_hex}");
                                continue;
                            }
                            if let Some(signal) = parse_call_signal(&msg.content) {
                                match signal {
                                    ParsedCallSignal::Invite { call_id, session } => {
                                        if active_call.is_some() {
                                            let _ = send_call_signal(
                                                &client,
                                                &relay_urls,
                                                &mdk,
                                                &keys,
                                                &nostr_group_id,
                                                &call_id,
                                                OutgoingCallSignal::Reject { reason: "busy" },
                                                "call_busy_reject",
                                            )
                                            .await;
                                            continue;
                                        }
                                        pending_call_invites.insert(
                                            call_id.clone(),
                                            PendingCallInvite {
                                                call_id: call_id.clone(),
                                                from_pubkey: sender_hex.clone(),
                                                nostr_group_id: nostr_group_id.clone(),
                                                session,
                                            },
                                        );
                                        out_tx
                                            .send(OutMsg::CallInviteReceived {
                                                call_id,
                                                from_pubkey: sender_hex,
                                                nostr_group_id,
                                            })
                                            .ok();
                                    }
                                    ParsedCallSignal::Accept { call_id, session } => {
                                        // We currently operate as the callee/echo sidecar.
                                        // Accepts are reserved for future outgoing-call support.
                                        let _ = (call_id, session);
                                    }
                                    ParsedCallSignal::Reject { call_id, reason } => {
                                        pending_call_invites.remove(&call_id);
                                        if active_call
                                            .as_ref()
                                            .map(|c| c.call_id == call_id)
                                            .unwrap_or(false)
                                        {
                                            if let Some(current) = active_call.take() {
                                                current.worker.stop().await;
                                            }
                                            out_tx
                                                .send(OutMsg::CallSessionEnded { call_id, reason })
                                                .ok();
                                        }
                                    }
                                    ParsedCallSignal::End { call_id, reason } => {
                                        pending_call_invites.remove(&call_id);
                                        if active_call
                                            .as_ref()
                                            .map(|c| c.call_id == call_id)
                                            .unwrap_or(false)
                                        {
                                            if let Some(current) = active_call.take() {
                                                current.worker.stop().await;
                                            }
                                            out_tx
                                                .send(OutMsg::CallSessionEnded { call_id, reason })
                                                .ok();
                                        }
                                    }
                                }
                                continue;
                            }
                            out_tx.send(OutMsg::MessageReceived {
                                nostr_group_id,
                                from_pubkey: sender_hex,
                                content: msg.content,
                                created_at: msg.created_at.as_secs(),
                                message_id: msg.id.to_hex(),
                            }).ok();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!("[marmotd] process_message failed id={} err={e:#}", event.id.to_hex());
                        }
                    }
                }
            }
        }
    }

    // Best-effort cleanup
    if let Some(current) = active_call.take() {
        current.worker.stop().await;
    }
    let _ = client.unsubscribe(&gift_sub.val).await;
    client.unsubscribe_all().await;
    client.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_call_invite_signal() {
        let content = serde_json::json!({
            "v": 1,
            "ns": "pika.call",
            "type": "call.invite",
            "call_id": "550e8400-e29b-41d4-a716-446655440000",
            "ts_ms": 1730000000000i64,
            "body": {
                "moq_url": "https://moq.local/anon",
                "broadcast_base": "pika/calls/550e8400-e29b-41d4-a716-446655440000",
                "tracks": [{
                    "name": "audio0",
                    "codec": "opus",
                    "sample_rate": 48000,
                    "channels": 1,
                    "frame_ms": 20
                }]
            }
        })
        .to_string();
        let parsed = parse_call_signal(&content).expect("parse call signal");
        match parsed {
            ParsedCallSignal::Invite { call_id, session } => {
                assert_eq!(call_id, "550e8400-e29b-41d4-a716-446655440000");
                assert_eq!(session.moq_url, "https://moq.local/anon");
                assert_eq!(
                    session.broadcast_base,
                    "pika/calls/550e8400-e29b-41d4-a716-446655440000"
                );
            }
            other => panic!("expected invite signal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn echo_worker_republishes_frames() {
        let stats = run_audio_echo_smoke(10).await.expect("audio echo smoke");
        assert_eq!(stats.sent_frames, 10);
        assert_eq!(stats.echoed_frames, 10);
    }
}
