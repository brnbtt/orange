//! Troubleshooting checks owned by the media CLI child process.
//!
//! This command reports plugin/runtime/network readiness only. It does not
//! start capture, decode to screen, or play audio.

use crate::{peer, pipeline};
use anyhow::Result;
use gstreamer as gst;
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const SCHEMA: u8 = 1;
const DETAIL_LIMIT: usize = 240;
const CHECK_IDS: [&str; 7] = [
    "runtime",
    "capture",
    "encoder",
    "decoder",
    "audio",
    "signalling",
    "stun",
];
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(3);
const STUN_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);

const RUNTIME_FACTORIES: [&str; 6] = [
    "webrtcbin",
    "nicesrc",
    "nicesink",
    "dtlssrtpenc",
    "dtlssrtpdec",
    "rtpbin",
];
const CAPTURE_FACTORIES: [&str; 2] = ["d3d11screencapturesrc", "d3d11convert"];
const HOST_AUDIO_FACTORIES: [&str; 5] = [
    "wasapi2src",
    "audioconvert",
    "audioresample",
    "opusenc",
    "rtpopuspay",
];
const RECEIVE_AUDIO_FACTORIES: [&str; 3] = ["rtpopusdepay", "opusdec", "volume"];
const RECEIVE_AUDIO_SINK_FACTORIES: [&str; 2] = ["wasapi2sink", "wasapisink"];
const H265_DECODE_FACTORIES: [&str; 3] = ["rtph265depay", "h265parse", "d3d11h265dec"];
const H264_DECODE_FACTORIES: [&str; 3] = ["rtph264depay", "h264parse", "d3d11h264dec"];
const PRESENTATION_FACTORIES: [&str; 2] = ["overlaycomposition", "d3d11videosink"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Pass,
    Fail,
    Inconclusive,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    id: &'static str,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Serialize)]
struct TroubleshootReport {
    schema: u8,
    checks: Vec<CheckResult>,
}

fn check(id: &'static str, status: CheckStatus, detail: impl Into<String>) -> CheckResult {
    let detail = detail.into();
    debug_assert!(detail.len() <= DETAIL_LIMIT, "detail too long for {id}");
    CheckResult { id, status, detail }
}

fn missing_required<'a>(
    required: &'a [&'a str],
    has_factory: &impl Fn(&str) -> bool,
) -> Vec<&'a str> {
    required
        .iter()
        .copied()
        .filter(|factory| !has_factory(factory))
        .collect()
}

fn has_factory(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

fn join_missing(missing: &[&str]) -> String {
    missing.join(",")
}

fn skipped_due_to_runtime(id: &'static str) -> CheckResult {
    check(
        id,
        CheckStatus::Inconclusive,
        "Skipped because runtime is unavailable. Repair or update Orange, then retry.",
    )
}

pub(crate) async fn run(server: &str) -> Result<()> {
    let report = collect(server).await;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

async fn collect(server: &str) -> TroubleshootReport {
    let runtime = runtime_check();
    let runtime_ready = runtime.status == CheckStatus::Pass;
    let checks = vec![
        runtime,
        capture_check(runtime_ready),
        encoder_check(runtime_ready),
        decoder_check(runtime_ready),
        audio_check(runtime_ready),
        signalling_check(server).await,
        stun_check().await,
    ];

    debug_assert_eq!(checks.len(), CHECK_IDS.len());
    debug_assert!(checks
        .iter()
        .zip(CHECK_IDS)
        .all(|(result, expected)| result.id == expected));

    TroubleshootReport {
        schema: SCHEMA,
        checks,
    }
}

fn runtime_check() -> CheckResult {
    runtime_check_with(gst::init().is_ok(), &has_factory)
}

fn runtime_check_with(init_ok: bool, has: &impl Fn(&str) -> bool) -> CheckResult {
    if !init_ok {
        return check(
            "runtime",
            CheckStatus::Fail,
            "GStreamer runtime could not initialize. Repair or update Orange, then retry.",
        );
    }
    let missing = missing_required(&RUNTIME_FACTORIES, has);
    if missing.is_empty() {
        return check(
            "runtime",
            CheckStatus::Pass,
            "Runtime and WebRTC transport factories are available for checks.",
        );
    }
    check(
        "runtime",
        CheckStatus::Fail,
        format!(
            "Missing runtime factories: {}. Repair or update Orange, then retry.",
            join_missing(&missing)
        ),
    )
}

fn capture_check(runtime_ready: bool) -> CheckResult {
    if !runtime_ready {
        return skipped_due_to_runtime("capture");
    }
    let missing = missing_required(&CAPTURE_FACTORIES, &has_factory);
    if missing.is_empty() {
        return check(
            "capture",
            CheckStatus::Pass,
            "Capture factories are available; no capture was started.",
        );
    }
    check(
        "capture",
        CheckStatus::Fail,
        format!(
            "Missing capture factories: {}. Repair or update Orange, then retry.",
            join_missing(&missing)
        ),
    )
}

fn encoder_check(runtime_ready: bool) -> CheckResult {
    if !runtime_ready {
        return skipped_due_to_runtime("encoder");
    }
    let (codec, encoder) = match pipeline::select_encoder(None) {
        Ok(found) => found,
        Err(_) => {
            return check(
                "encoder",
                CheckStatus::Fail,
                "No compatible zero-copy encoder found. Update GPU drivers, update Orange, or try another PC.",
            );
        }
    };

    let parser = codec.parser();
    let payloader = codec.payloader();
    let required = [encoder, parser, payloader];
    let missing = missing_required(&required, &has_factory);
    if missing.is_empty() {
        return check(
            "encoder",
            CheckStatus::Pass,
            format!(
                "Auto-selected {codec:?} via {encoder}; parser/payloader present. Availability only, no encode run."
            ),
        );
    }
    check(
        "encoder",
        CheckStatus::Fail,
        format!(
            "Selected {codec:?} via {encoder}, but missing {}. Repair/update Orange; update GPU drivers or try another PC.",
            join_missing(&missing)
        ),
    )
}

#[derive(Debug)]
struct DecoderProbe {
    h265_missing: Vec<&'static str>,
    h264_missing: Vec<&'static str>,
    presentation_missing: Vec<&'static str>,
}

impl DecoderProbe {
    fn h265_ready(&self) -> bool {
        self.h265_missing.is_empty()
    }

    fn h264_ready(&self) -> bool {
        self.h264_missing.is_empty()
    }

    fn presentation_ready(&self) -> bool {
        self.presentation_missing.is_empty()
    }

    fn missing_names(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        missing.extend(self.h265_missing.iter().copied());
        missing.extend(self.h264_missing.iter().copied());
        missing.extend(self.presentation_missing.iter().copied());
        missing.sort_unstable();
        missing.dedup();
        missing
    }
}

fn decoder_probe_with(has: &impl Fn(&str) -> bool) -> DecoderProbe {
    DecoderProbe {
        h265_missing: missing_required(&H265_DECODE_FACTORIES, has),
        h264_missing: missing_required(&H264_DECODE_FACTORIES, has),
        presentation_missing: missing_required(&PRESENTATION_FACTORIES, has),
    }
}

fn decoder_check(runtime_ready: bool) -> CheckResult {
    if !runtime_ready {
        return skipped_due_to_runtime("decoder");
    }
    decoder_result(decoder_probe_with(&has_factory))
}

fn decoder_result(probe: DecoderProbe) -> CheckResult {
    let status = match (
        probe.h265_ready(),
        probe.h264_ready(),
        probe.presentation_ready(),
    ) {
        (true, true, true) => CheckStatus::Pass,
        (false, false, _) => CheckStatus::Fail,
        _ => CheckStatus::Inconclusive,
    };

    if status == CheckStatus::Pass {
        return check(
            "decoder",
            CheckStatus::Pass,
            "H265/H264 decode and presentation factories are available; no decode/display output was run.",
        );
    }

    let state = format!(
        "H265:{} H264:{} Render:{}",
        if probe.h265_ready() {
            "ready"
        } else {
            "missing"
        },
        if probe.h264_ready() {
            "ready"
        } else {
            "missing"
        },
        if probe.presentation_ready() {
            "ready"
        } else {
            "missing"
        }
    );
    let missing = join_missing(&probe.missing_names());
    check(
        "decoder",
        status,
        format!(
            "{state}; missing {missing}. Repair/update Orange; update GPU drivers or try another PC for codec support."
        ),
    )
}

#[derive(Debug)]
struct AudioProbe {
    host_missing: Vec<&'static str>,
    receive_missing: Vec<&'static str>,
    has_output_sink: bool,
}

impl AudioProbe {
    fn missing_names(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        missing.extend(self.host_missing.iter().copied());
        missing.extend(self.receive_missing.iter().copied());
        if !self.has_output_sink {
            missing.extend(RECEIVE_AUDIO_SINK_FACTORIES);
        }
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    fn ready(&self) -> bool {
        self.host_missing.is_empty() && self.receive_missing.is_empty() && self.has_output_sink
    }
}

fn audio_probe_with(has: &impl Fn(&str) -> bool) -> AudioProbe {
    AudioProbe {
        host_missing: missing_required(&HOST_AUDIO_FACTORIES, has),
        receive_missing: missing_required(&RECEIVE_AUDIO_FACTORIES, has),
        has_output_sink: RECEIVE_AUDIO_SINK_FACTORIES.iter().copied().any(has),
    }
}

fn audio_check(runtime_ready: bool) -> CheckResult {
    if !runtime_ready {
        return skipped_due_to_runtime("audio");
    }
    let probe = audio_probe_with(&has_factory);
    if probe.ready() {
        return check(
            "audio",
            CheckStatus::Pass,
            "Host and receive audio factories are available with a WASAPI sink; no audio output was played.",
        );
    }
    check(
        "audio",
        CheckStatus::Fail,
        format!(
            "Missing audio factories: {}. Repair or update Orange, then retry.",
            join_missing(&probe.missing_names())
        ),
    )
}

async fn signalling_check(server: &str) -> CheckResult {
    signalling_check_with(server, SIGNAL_TIMEOUT).await
}

async fn signalling_check_with(server: &str, timeout: Duration) -> CheckResult {
    match tokio::time::timeout(timeout, orange_signal::connect(server)).await {
        Err(_) => check(
            "signalling",
            CheckStatus::Fail,
            "Signalling WebSocket handshake timed out. Check connection, DNS or VPN, then retry.",
        ),
        Ok(Err(_)) => check(
            "signalling",
            CheckStatus::Fail,
            "Signalling WebSocket handshake failed. Check server URL, connection, DNS or VPN, then retry.",
        ),
        Ok(Ok(client)) => {
            let _ = tokio::time::timeout(Duration::from_secs(1), client.close()).await;
            check(
                "signalling",
                CheckStatus::Pass,
                "WebSocket handshake succeeded; room join and authentication were not tested.",
            )
        }
    }
}

async fn stun_check() -> CheckResult {
    let Some((host, port)) = parse_stun_endpoint(peer::STUN) else {
        return check(
            "stun",
            CheckStatus::Fail,
            "Configured STUN endpoint is invalid. Repair or update Orange, then retry.",
        );
    };

    match probe_stun_ipv4(host, port).await {
        StunProbeStatus::Pass => check(
            "stun",
            CheckStatus::Pass,
            "STUN server reachability succeeded over IPv4; friend connectivity is still untested.",
        ),
        StunProbeStatus::DnsTimeout => check(
            "stun",
            CheckStatus::Fail,
            "IPv4 STUN DNS timed out. Check DNS, VPN or network path, then retry.",
        ),
        StunProbeStatus::DnsFailed => check(
            "stun",
            CheckStatus::Fail,
            "IPv4 STUN DNS failed. Check DNS, VPN or network path, then retry.",
        ),
        StunProbeStatus::NoIpv4Address => check(
            "stun",
            CheckStatus::Inconclusive,
            "STUN DNS returned no IPv4 address. Check DNS/network setup, then retry.",
        ),
        StunProbeStatus::SocketFailure => check(
            "stun",
            CheckStatus::Fail,
            "IPv4 STUN UDP socket failed. Check connection, VPN or network path, then retry.",
        ),
        StunProbeStatus::InvalidResponse => check(
            "stun",
            CheckStatus::Fail,
            "IPv4 STUN replies were invalid. Retry; if it persists, check VPN/network path.",
        ),
        StunProbeStatus::Timeout => check(
            "stun",
            CheckStatus::Inconclusive,
            "No valid IPv4 STUN response before deadline. Check connection, DNS or VPN, then retry.",
        ),
    }
}

fn parse_stun_endpoint(endpoint: &str) -> Option<(&str, u16)> {
    let rest = endpoint.strip_prefix("stun://")?;
    let (host, port_text) = rest.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host, port_text.parse::<u16>().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StunProbeStatus {
    Pass,
    DnsTimeout,
    DnsFailed,
    NoIpv4Address,
    SocketFailure,
    InvalidResponse,
    Timeout,
}

#[derive(Debug, Clone, Copy)]
struct StunProbeConfig {
    attempts: usize,
    recv_timeout: Duration,
    send_timeout: Duration,
    total_timeout: Duration,
}

impl Default for StunProbeConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            recv_timeout: Duration::from_millis(900),
            send_timeout: Duration::from_millis(250),
            total_timeout: STUN_TOTAL_TIMEOUT,
        }
    }
}

async fn probe_stun_ipv4(host: &str, port: u16) -> StunProbeStatus {
    let addresses = match tokio::time::timeout(
        Duration::from_millis(900),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Err(_) => return StunProbeStatus::DnsTimeout,
        Ok(Err(_)) => return StunProbeStatus::DnsFailed,
        Ok(Ok(iter)) => iter
            .filter(|addr| matches!(addr.ip(), IpAddr::V4(_)))
            .collect::<Vec<_>>(),
    };
    if addresses.is_empty() {
        return StunProbeStatus::NoIpv4Address;
    }
    probe_stun_ipv4_endpoints(&addresses, StunProbeConfig::default()).await
}

async fn probe_stun_ipv4_endpoints(
    addresses: &[SocketAddr],
    config: StunProbeConfig,
) -> StunProbeStatus {
    if addresses.is_empty() {
        return StunProbeStatus::NoIpv4Address;
    }

    let deadline = Instant::now() + config.total_timeout;
    let mut observed_socket_failure = false;
    let mut observed_invalid_response = false;

    for attempt in 0..config.attempts {
        if Instant::now() >= deadline {
            break;
        }
        let target = addresses[attempt % addresses.len()];

        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(socket) => socket,
            Err(_) => {
                observed_socket_failure = true;
                continue;
            }
        };
        if socket.connect(target).await.is_err() {
            observed_socket_failure = true;
            continue;
        }

        let transaction_id = random_transaction_id();
        let request = stun_binding_request(transaction_id);
        match tokio::time::timeout(config.send_timeout, socket.send(&request)).await {
            Err(_) | Ok(Err(_)) => {
                observed_socket_failure = true;
                continue;
            }
            Ok(Ok(_)) => {}
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let recv_budget = remaining.min(config.recv_timeout);
        if recv_budget.is_zero() {
            break;
        }

        let mut response = [0u8; 1500];
        let bytes = match tokio::time::timeout(recv_budget, socket.recv(&mut response)).await {
            Err(_) => continue,
            Ok(Err(_)) => {
                observed_socket_failure = true;
                continue;
            }
            Ok(Ok(bytes)) => bytes,
        };
        match validate_stun_binding_response(&response[..bytes], &transaction_id) {
            Ok(()) => return StunProbeStatus::Pass,
            Err(_) => observed_invalid_response = true,
        }
    }

    if observed_invalid_response {
        StunProbeStatus::InvalidResponse
    } else if observed_socket_failure {
        StunProbeStatus::SocketFailure
    } else {
        StunProbeStatus::Timeout
    }
}

fn random_transaction_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    for chunk in id.as_chunks_mut::<4>().0 {
        chunk.copy_from_slice(&gst::glib::random_int().to_be_bytes());
    }
    id
}

fn stun_binding_request(transaction_id: [u8; 12]) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    packet[2..4].copy_from_slice(&0u16.to_be_bytes());
    packet[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
    packet[8..20].copy_from_slice(&transaction_id);
    packet
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StunValidationError {
    TooShort,
    WrongType,
    Truncated,
    WrongCookie,
    TransactionMismatch,
    MissingMappedAddress,
}

fn validate_stun_binding_response(
    packet: &[u8],
    transaction_id: &[u8; 12],
) -> Result<(), StunValidationError> {
    if packet.len() < 20 {
        return Err(StunValidationError::TooShort);
    }
    if u16::from_be_bytes([packet[0], packet[1]]) != 0x0101 {
        return Err(StunValidationError::WrongType);
    }

    let body_length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let Some(message_end) = 20usize.checked_add(body_length) else {
        return Err(StunValidationError::Truncated);
    };
    if message_end > packet.len() {
        return Err(StunValidationError::Truncated);
    }

    if u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) != 0x2112_A442 {
        return Err(StunValidationError::WrongCookie);
    }
    if packet[8..20] != transaction_id[..] {
        return Err(StunValidationError::TransactionMismatch);
    }

    let mut offset = 20usize;
    let mut has_mapped = false;
    while offset < message_end {
        if offset + 4 > message_end {
            return Err(StunValidationError::Truncated);
        }
        let attribute_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let value_length = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        offset += 4;

        if offset + value_length > message_end {
            return Err(StunValidationError::Truncated);
        }
        if matches!(attribute_type, 0x0001 | 0x0020)
            && value_length >= 8
            && packet[offset + 1] == 0x01
        {
            has_mapped = true;
        }

        offset += value_length;
        let padding = (4 - (value_length % 4)) % 4;
        if offset + padding > message_end {
            return Err(StunValidationError::Truncated);
        }
        offset += padding;
    }

    if !has_mapped {
        return Err(StunValidationError::MissingMappedAddress);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const MAGIC_COOKIE: u32 = 0x2112_A442;

    #[derive(Clone, Copy)]
    enum ResponseKind {
        Valid,
        MalformedType,
        MismatchedTransaction,
        TruncatedAttributes,
        NoResponse,
    }

    #[test]
    fn runtime_check_requires_webrtc_transport_factories() {
        let check = runtime_check_with(true, &|factory| factory != "webrtcbin");

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("webrtcbin"));
    }

    #[test]
    fn runtime_unavailable_skip_includes_next_step_guidance() {
        let capture = capture_check(false);
        assert_eq!(capture.status, CheckStatus::Inconclusive);
        assert!(capture.detail.contains("Repair or update Orange"));
    }

    #[test]
    fn audio_check_requires_receive_path_and_one_supported_wasapi_sink() {
        let available = Arc::new([
            "wasapi2src",
            "audioconvert",
            "audioresample",
            "opusenc",
            "rtpopuspay",
            "rtpopusdepay",
            "opusdec",
            "volume",
            "wasapi2sink",
        ]);
        let has = |factory: &str| available.contains(&factory);
        let probe = audio_probe_with(&has);
        assert!(probe.ready());

        let missing_sink = Arc::new([
            "wasapi2src",
            "audioconvert",
            "audioresample",
            "opusenc",
            "rtpopuspay",
            "rtpopusdepay",
            "opusdec",
            "volume",
        ]);
        let has = |factory: &str| missing_sink.contains(&factory);
        let probe = audio_probe_with(&has);
        assert!(!probe.ready());
        let missing = join_missing(&probe.missing_names());
        assert!(missing.contains("wasapi2sink"));
        assert!(missing.contains("wasapisink"));
    }

    #[test]
    fn decoder_mixed_codec_availability_is_inconclusive() {
        let available = Arc::new([
            "rtph265depay",
            "h265parse",
            "d3d11h265dec",
            "overlaycomposition",
            "d3d11videosink",
        ]);
        let has = |factory: &str| available.contains(&factory);
        let probe = decoder_probe_with(&has);

        assert!(probe.h265_ready());
        assert!(!probe.h264_ready());
        assert!(probe.presentation_ready());
    }

    #[test]
    fn decoder_detail_names_missing_plugins_for_mixed_codec() {
        let probe = decoder_probe_with(&|factory| {
            !matches!(factory, "h264parse" | "d3d11h264dec" | "rtph264depay")
        });
        let status = match (
            probe.h265_ready(),
            probe.h264_ready(),
            probe.presentation_ready(),
        ) {
            (true, true, true) => CheckStatus::Pass,
            (false, false, _) => CheckStatus::Fail,
            _ => CheckStatus::Inconclusive,
        };
        assert_eq!(status, CheckStatus::Inconclusive);
        let missing = join_missing(&probe.missing_names());
        assert!(missing.contains("h264parse"));
        assert!(missing.contains("d3d11h264dec"));
    }

    fn success_response_for(transaction_id: [u8; 12]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&0x0101u16.to_be_bytes());
        packet.extend_from_slice(&12u16.to_be_bytes());
        packet.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        packet.extend_from_slice(&transaction_id);
        packet.extend_from_slice(&0x0020u16.to_be_bytes());
        packet.extend_from_slice(&8u16.to_be_bytes());
        packet.push(0);
        packet.push(1);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&[1, 2, 3, 4]);
        packet
    }

    fn response_packet(request: &[u8], kind: ResponseKind) -> Option<Vec<u8>> {
        if request.len() < 20 {
            return None;
        }
        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&request[8..20]);
        let mut packet = success_response_for(transaction_id);
        match kind {
            ResponseKind::Valid => Some(packet),
            ResponseKind::MalformedType => {
                packet[1] = 0x02;
                Some(packet)
            }
            ResponseKind::MismatchedTransaction => {
                packet[19] ^= 0x01;
                Some(packet)
            }
            ResponseKind::TruncatedAttributes => {
                packet[2..4].copy_from_slice(&16u16.to_be_bytes());
                packet.truncate(30);
                Some(packet)
            }
            ResponseKind::NoResponse => None,
        }
    }

    async fn spawn_stun_responder(kind: ResponseKind) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut request = [0u8; 1024];
            for _ in 0..6 {
                let Ok((size, peer)) = socket.recv_from(&mut request).await else {
                    break;
                };
                if let Some(response) = response_packet(&request[..size], kind) {
                    let _ = socket.send_to(&response, peer).await;
                }
            }
        });
        address
    }

    fn test_probe_config() -> StunProbeConfig {
        StunProbeConfig {
            attempts: 3,
            recv_timeout: Duration::from_millis(100),
            send_timeout: Duration::from_millis(100),
            total_timeout: Duration::from_millis(450),
        }
    }

    #[test]
    fn valid_stun_binding_response_is_accepted() {
        let transaction_id = [7; 12];
        let packet = success_response_for(transaction_id);
        assert!(validate_stun_binding_response(&packet, &transaction_id).is_ok());
    }

    #[test]
    fn mismatched_stun_transaction_id_is_rejected() {
        let transaction_id = [7; 12];
        let mut packet = success_response_for(transaction_id);
        packet[19] ^= 0x01;
        assert!(validate_stun_binding_response(&packet, &transaction_id).is_err());
    }

    #[test]
    fn truncated_stun_attributes_are_rejected() {
        let transaction_id = [7; 12];
        let mut packet = success_response_for(transaction_id);
        packet[2..4].copy_from_slice(&16u16.to_be_bytes());
        packet.truncate(30);
        assert!(validate_stun_binding_response(&packet, &transaction_id).is_err());
    }

    #[tokio::test]
    async fn stun_probe_accepts_a_valid_binding_response() {
        let address = spawn_stun_responder(ResponseKind::Valid).await;
        let status = probe_stun_ipv4_endpoints(&[address], test_probe_config()).await;
        assert_eq!(status, StunProbeStatus::Pass);
    }

    #[tokio::test]
    async fn stun_probe_rejects_malformed_binding_response_type() {
        let address = spawn_stun_responder(ResponseKind::MalformedType).await;
        let status = probe_stun_ipv4_endpoints(&[address], test_probe_config()).await;
        assert_eq!(status, StunProbeStatus::InvalidResponse);
    }

    #[tokio::test]
    async fn stun_probe_rejects_mismatched_binding_transaction_id() {
        let address = spawn_stun_responder(ResponseKind::MismatchedTransaction).await;
        let status = probe_stun_ipv4_endpoints(&[address], test_probe_config()).await;
        assert_eq!(status, StunProbeStatus::InvalidResponse);
    }

    #[tokio::test]
    async fn stun_probe_rejects_truncated_binding_attributes() {
        let address = spawn_stun_responder(ResponseKind::TruncatedAttributes).await;
        let status = probe_stun_ipv4_endpoints(&[address], test_probe_config()).await;
        assert_eq!(status, StunProbeStatus::InvalidResponse);
    }

    #[tokio::test]
    async fn stun_probe_times_out_without_a_response() {
        let address = spawn_stun_responder(ResponseKind::NoResponse).await;
        let status = probe_stun_ipv4_endpoints(&[address], test_probe_config()).await;
        assert_eq!(status, StunProbeStatus::Timeout);
    }

    async fn free_local_addr() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address.to_string()
    }

    async fn wait_until_listening(address: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("relay did not start listening");
    }

    #[tokio::test]
    async fn signalling_check_passes_on_local_websocket_handshake() {
        let address = free_local_addr().await;
        let server = format!("ws://{address}/ws");
        let relay = tokio::spawn({
            let address = address.clone();
            async move {
                let _ = orange_signal::serve(&address).await;
            }
        });
        wait_until_listening(&address).await;

        let check = signalling_check_with(&server, Duration::from_secs(1)).await;

        relay.abort();
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn a_completely_missing_decoder_runtime_still_produces_actionable_output() {
        // Broken installs produce the longest plugin list; it must still fit
        // the desktop's detail limit and retain the recovery instruction.
        let result = decoder_result(decoder_probe_with(&|_| false));
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("d3d11videosink"));
        assert!(result.detail.contains("Repair/update Orange"));
        assert!(result.detail.len() <= 240);
    }

    #[tokio::test]
    async fn signalling_check_fails_on_local_http_upgrade_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = format!("ws://{address}/ws");
        let rejector = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length:0\r\n\r\n")
                    .await;
            }
        });

        let check = signalling_check_with(&server, Duration::from_secs(1)).await;

        rejector.abort();
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("handshake failed"));
    }

    #[tokio::test]
    async fn signalling_check_times_out_on_unresponsive_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = format!("ws://{address}/ws");
        let hanger = tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });

        let check = signalling_check_with(&server, Duration::from_millis(150)).await;

        hanger.abort();
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("timed out"));
    }
}
