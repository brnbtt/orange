//! Privacy-safe ICE diagnostics and isolated gather probe.

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_webrtc as gst_webrtc;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::media_diagnostics::emit_diagnostic;

const MAX_CANDIDATE_BYTES: usize = 8 * 1024;
const MAX_ICE_EVENTS_PER_ROLE: u16 = 512;

#[derive(Clone, Copy)]
pub(crate) enum IceDirection {
    Local,
    Remote,
}

impl IceDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum IceAction {
    Gathered,
    Signalled,
    SignallingClosed,
    Submitted,
    SubmissionCompleted,
    SubmissionFailed,
    EndOfCandidates,
    Invalid,
}

impl IceAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gathered => "gathered",
            Self::Signalled => "signalled",
            Self::SignallingClosed => "signalling-closed",
            Self::Submitted => "submitted",
            Self::SubmissionCompleted => "submission-completed",
            Self::SubmissionFailed => "submission-failed",
            Self::EndOfCandidates => "end-of-candidates",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateKind {
    Host,
    Srflx,
    Prflx,
    Relay,
    Unknown,
}

impl CandidateKind {
    fn from_token(token: &str) -> Self {
        match token.to_ascii_lowercase().as_str() {
            "host" => Self::Host,
            "srflx" => Self::Srflx,
            "prflx" => Self::Prflx,
            "relay" => Self::Relay,
            _ => Self::Unknown,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Srflx => "srflx",
            Self::Prflx => "prflx",
            Self::Relay => "relay",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateTransport {
    Udp,
    Tcp,
    Unknown,
}

impl CandidateTransport {
    fn from_token(token: &str) -> Self {
        match token.to_ascii_lowercase().as_str() {
            "udp" => Self::Udp,
            "tcp" => Self::Tcp,
            _ => Self::Unknown,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateFamily {
    Ipv4,
    Ipv6,
    Mdns,
    Unknown,
}

impl CandidateFamily {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
            Self::Mdns => "mdns",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateScope {
    Public,
    Private,
    Shared,
    LinkLocal,
    Loopback,
    Unknown,
}

impl CandidateScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Shared => "shared",
            Self::LinkLocal => "link-local",
            Self::Loopback => "loopback",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CandidateClass {
    Candidate(CandidateMeta),
    EndOfCandidates,
    Invalid,
}

#[derive(Clone, Copy, Serialize)]
struct CandidatePayload {
    direction: &'static str,
    action: &'static str,
    kind: &'static str,
    transport: &'static str,
    family: &'static str,
    scope: &'static str,
    mline: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateMeta {
    kind: CandidateKind,
    transport: CandidateTransport,
    family: CandidateFamily,
    scope: CandidateScope,
}

impl CandidateMeta {
    const UNKNOWN: Self = Self {
        kind: CandidateKind::Unknown,
        transport: CandidateTransport::Unknown,
        family: CandidateFamily::Unknown,
        scope: CandidateScope::Unknown,
    };

    fn from_candidate_parts(
        kind: Option<&str>,
        transport: Option<&str>,
        address: Option<&str>,
    ) -> Self {
        let kind = kind
            .map(CandidateKind::from_token)
            .unwrap_or(CandidateKind::Unknown);
        let transport = transport
            .map(CandidateTransport::from_token)
            .unwrap_or(CandidateTransport::Unknown);
        let (family, scope) = classify_address(address.unwrap_or_default());
        Self {
            kind,
            transport,
            family,
            scope,
        }
    }

    fn payload(self, direction: IceDirection, action: IceAction, mline: u32) -> CandidatePayload {
        CandidatePayload {
            direction: direction.as_str(),
            action: action.as_str(),
            kind: self.kind.as_str(),
            transport: self.transport.as_str(),
            family: self.family.as_str(),
            scope: self.scope.as_str(),
            mline,
        }
    }
}

fn classify_address(raw: &str) -> (CandidateFamily, CandidateScope) {
    let address = raw.trim().trim_matches('[').trim_matches(']');
    if address.is_empty() {
        return (CandidateFamily::Unknown, CandidateScope::Unknown);
    }
    if address.to_ascii_lowercase().ends_with(".local") {
        return (CandidateFamily::Mdns, CandidateScope::Unknown);
    }
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => (CandidateFamily::Ipv4, classify_ipv4_scope(ip)),
        Ok(IpAddr::V6(ip)) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                (CandidateFamily::Ipv6, classify_ipv4_scope(mapped))
            } else {
                (CandidateFamily::Ipv6, classify_ipv6_scope(ip))
            }
        }
        Err(_) => (CandidateFamily::Unknown, CandidateScope::Unknown),
    }
}

fn classify_ipv4_scope(ip: Ipv4Addr) -> CandidateScope {
    if ip.is_unspecified()
        || ip.is_multicast()
        || is_documentation_ipv4(ip)
        || is_benchmark_ipv4(ip)
        || is_reserved_ipv4(ip)
    {
        CandidateScope::Unknown
    } else if is_shared_cgnat_ipv4(ip) {
        CandidateScope::Shared
    } else if ip.is_loopback() {
        CandidateScope::Loopback
    } else if ip.is_link_local() {
        CandidateScope::LinkLocal
    } else if ip.is_private() {
        CandidateScope::Private
    } else {
        CandidateScope::Public
    }
}

fn is_documentation_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    matches!((a, b, c), (192, 0, 2) | (198, 51, 100) | (203, 0, 113))
}

fn is_benchmark_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 198 && (18..=19).contains(&b)
}

fn is_reserved_ipv4(ip: Ipv4Addr) -> bool {
    let [a, _, _, _] = ip.octets();
    a == 0 || a >= 240
}

fn is_shared_cgnat_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

fn classify_ipv6_scope(ip: Ipv6Addr) -> CandidateScope {
    if ip.is_unspecified() || ip.is_multicast() || is_documentation_ipv6(ip) {
        CandidateScope::Unknown
    } else if ip.is_loopback() {
        CandidateScope::Loopback
    } else if ip.is_unicast_link_local() {
        CandidateScope::LinkLocal
    } else {
        let head = ip.segments()[0];
        if (head & 0xfe00) == 0xfc00 {
            CandidateScope::Private
        } else {
            CandidateScope::Public
        }
    }
}

fn is_documentation_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

fn parse_candidate_tokens(candidate: &str) -> Option<Vec<&str>> {
    let trimmed = candidate.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("end-of-candidates")
        || trimmed.eq_ignore_ascii_case("a=end-of-candidates")
    {
        return None;
    }
    let body = trimmed
        .strip_prefix("a=")
        .unwrap_or(trimmed)
        .strip_prefix("candidate:")?;
    Some(body.split_whitespace().collect())
}

fn validate_candidate_structure<'a>(
    tokens: &'a [&'a str],
) -> Option<(u32, CandidateTransport, u64, u16, CandidateKind, &'a str)> {
    if tokens.len() < 8 {
        return None;
    }
    let component = tokens[1].parse::<u32>().ok()?;
    let transport = CandidateTransport::from_token(tokens[2]);
    let priority = tokens[3].parse::<u64>().ok()?;
    let address = tokens[4];
    let port = tokens[5].parse::<u16>().ok()?;
    if !tokens[6].eq_ignore_ascii_case("typ") {
        return None;
    }
    let kind = CandidateKind::from_token(tokens[7]);
    Some((component, transport, priority, port, kind, address))
}

pub(crate) fn classify_candidate(candidate: &str) -> CandidateClass {
    if candidate.len() > MAX_CANDIDATE_BYTES {
        return CandidateClass::Invalid;
    }
    let Some(tokens) = parse_candidate_tokens(candidate) else {
        let trimmed = candidate.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("end-of-candidates")
            || trimmed.eq_ignore_ascii_case("a=end-of-candidates")
        {
            return CandidateClass::EndOfCandidates;
        }
        return CandidateClass::Invalid;
    };
    let Some((component, transport, priority, port, kind, address)) =
        validate_candidate_structure(&tokens)
    else {
        return CandidateClass::Invalid;
    };
    if component == 0 || priority == 0 || port == 0 {
        return CandidateClass::Invalid;
    }
    let (family, scope) = classify_address(address);
    CandidateClass::Candidate(CandidateMeta {
        kind,
        transport,
        family,
        scope,
    })
}

struct IceLimiter {
    emitted: AtomicU16,
    dropped: AtomicU32,
}

impl Default for IceLimiter {
    fn default() -> Self {
        Self {
            emitted: AtomicU16::new(0),
            dropped: AtomicU32::new(0),
        }
    }
}

#[derive(Clone)]
pub(crate) struct IceEventTracker {
    role: Arc<str>,
    limiter: Arc<IceLimiter>,
}

impl IceEventTracker {
    pub(crate) fn new(role: impl Into<String>) -> Self {
        Self {
            role: Arc::from(role.into()),
            limiter: Arc::new(IceLimiter::default()),
        }
    }

    pub(crate) fn emit_runtime_once(&self) {
        let (major, minor, micro, nano) = gst::version();
        emit_diagnostic(
            "ice-runtime",
            &self.role,
            serde_json::json!({
                "gst_major": major,
                "gst_minor": minor,
                "gst_micro": micro,
                "gst_nano": nano,
            }),
        );
    }

    pub(crate) fn emit(
        &self,
        direction: IceDirection,
        action: IceAction,
        mline: u32,
        class: CandidateClass,
    ) {
        let accepted = self
            .limiter
            .emitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_ICE_EVENTS_PER_ROLE).then_some(current.saturating_add(1))
            })
            .is_ok();
        if !accepted {
            let _ =
                self.limiter
                    .dropped
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        Some(current.saturating_add(1))
                    });
            return;
        }

        let metadata = match class {
            CandidateClass::Candidate(meta) => meta,
            CandidateClass::EndOfCandidates | CandidateClass::Invalid => CandidateMeta::UNKNOWN,
        };
        emit_diagnostic(
            "ice-candidate",
            &self.role,
            metadata.payload(direction, action, mline),
        );
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct IceRouteReport {
    pub(crate) selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) local: Option<IceRouteCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<IceRouteCandidate>,
}

#[derive(Debug, Serialize)]
pub(crate) struct IceRouteCandidate {
    kind: &'static str,
    transport: &'static str,
    family: &'static str,
    scope: &'static str,
}

impl From<CandidateMeta> for IceRouteCandidate {
    fn from(value: CandidateMeta) -> Self {
        Self {
            kind: value.kind.as_str(),
            transport: value.transport.as_str(),
            family: value.family.as_str(),
            scope: value.scope.as_str(),
        }
    }
}

pub(crate) fn selected_ice_route(stats: &gst::StructureRef) -> IceRouteReport {
    let mut by_id = HashMap::<String, gst::Structure>::new();
    for (key, value) in stats.iter() {
        let Ok(sample) = value.get::<gst::Structure>() else {
            continue;
        };
        let id = sample
            .get::<String>("id")
            .ok()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| key.to_string());
        by_id.insert(id, sample);
    }

    let selected_pair_id = by_id.values().find_map(|sample| {
        sample
            .get::<String>("selected-candidate-pair-id")
            .ok()
            .filter(|value| !value.is_empty())
    });
    let Some(selected_pair_id) = selected_pair_id else {
        return IceRouteReport {
            selected: false,
            local: None,
            remote: None,
        };
    };
    let Some(pair) = by_id.get(&selected_pair_id) else {
        return IceRouteReport {
            selected: false,
            local: None,
            remote: None,
        };
    };
    let Ok(local_id) = pair.get::<String>("local-candidate-id") else {
        return IceRouteReport {
            selected: false,
            local: None,
            remote: None,
        };
    };
    let Ok(remote_id) = pair.get::<String>("remote-candidate-id") else {
        return IceRouteReport {
            selected: false,
            local: None,
            remote: None,
        };
    };

    let local = by_id
        .get(&local_id)
        .map(|candidate| candidate_meta_from_stats(candidate.as_ref()))
        .map(Into::into);
    let remote = by_id
        .get(&remote_id)
        .map(|candidate| candidate_meta_from_stats(candidate.as_ref()))
        .map(Into::into);
    if local.is_none() || remote.is_none() {
        return IceRouteReport {
            selected: false,
            local: None,
            remote: None,
        };
    }

    IceRouteReport {
        selected: true,
        local,
        remote,
    }
}

fn candidate_meta_from_stats(candidate: &gst::StructureRef) -> CandidateMeta {
    CandidateMeta::from_candidate_parts(
        candidate.get::<String>("candidate-type").ok().as_deref(),
        candidate.get::<String>("protocol").ok().as_deref(),
        candidate
            .get::<String>("address")
            .or_else(|_| candidate.get::<String>("ip"))
            .ok()
            .as_deref(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    Complete,
    TimedOut,
    Failed,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CandidateCounts {
    pub(crate) udp: u32,
    pub(crate) tcp: u32,
    pub(crate) ipv4: u32,
    pub(crate) ipv6: u32,
    pub(crate) host: u32,
    pub(crate) srflx: u32,
    pub(crate) prflx: u32,
    pub(crate) relay: u32,
    pub(crate) invalid: u32,
}

impl CandidateCounts {
    fn add_class(&mut self, class: CandidateClass) {
        match class {
            CandidateClass::Candidate(meta) => {
                match meta.transport {
                    CandidateTransport::Udp => self.udp = self.udp.saturating_add(1),
                    CandidateTransport::Tcp => self.tcp = self.tcp.saturating_add(1),
                    CandidateTransport::Unknown => {}
                }
                match meta.family {
                    CandidateFamily::Ipv4 => self.ipv4 = self.ipv4.saturating_add(1),
                    CandidateFamily::Ipv6 => self.ipv6 = self.ipv6.saturating_add(1),
                    CandidateFamily::Mdns | CandidateFamily::Unknown => {}
                }
                match meta.kind {
                    CandidateKind::Host => self.host = self.host.saturating_add(1),
                    CandidateKind::Srflx => self.srflx = self.srflx.saturating_add(1),
                    CandidateKind::Prflx => self.prflx = self.prflx.saturating_add(1),
                    CandidateKind::Relay => self.relay = self.relay.saturating_add(1),
                    CandidateKind::Unknown => {}
                }
            }
            CandidateClass::Invalid => {
                self.invalid = self.invalid.saturating_add(1);
            }
            CandidateClass::EndOfCandidates => {}
        }
    }

    pub(crate) fn total(&self) -> u32 {
        self.host
            .saturating_add(self.srflx)
            .saturating_add(self.prflx)
            .saturating_add(self.relay)
    }
}

#[derive(Debug)]
pub(crate) struct ProbeReport {
    pub(crate) outcome: ProbeOutcome,
    pub(crate) candidates: CandidateCounts,
}

impl ProbeReport {
    pub(crate) fn detail(&self) -> String {
        let detail = format!(
            "outcome={:?} total={} udp={} tcp={} ipv4={} ipv6={} host={} srflx={}",
            self.outcome,
            self.candidates.total(),
            self.candidates.udp,
            self.candidates.tcp,
            self.candidates.ipv4,
            self.candidates.ipv6,
            self.candidates.host,
            self.candidates.srflx,
        );
        if detail.len() <= 240 {
            detail
        } else {
            detail.chars().take(240).collect()
        }
    }
}

fn probe_webrtcbin() -> anyhow::Result<gst::Element> {
    Ok(gst::ElementFactory::make("webrtcbin")
        .name("ice-probe")
        .property_from_str("bundle-policy", "max-bundle")
        .build()?)
}

fn probe_webrtcbin_with_stun(stun_server: Option<&str>) -> anyhow::Result<gst::Element> {
    let bin = probe_webrtcbin()?;
    if let Some(stun_server) = stun_server {
        bin.set_property("stun-server", stun_server);
    }
    Ok(bin)
}

async fn create_offer_and_set_local(bin: &gst::Element) -> anyhow::Result<()> {
    let (offer_tx, offer_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    let offer_tx = Arc::new(Mutex::new(Some(offer_tx)));
    let offer_tx_for_offer = offer_tx.clone();
    let weak_bin = bin.downgrade();

    let promise = gst::Promise::with_change_func(move |reply| {
        let result = (|| -> anyhow::Result<()> {
            let Some(reply) =
                reply.map_err(|error| anyhow::anyhow!("offer promise failed: {error:?}"))?
            else {
                anyhow::bail!("offer promise returned no payload");
            };
            if let Ok(error) = reply.get::<gst::glib::Error>("error") {
                anyhow::bail!("offer promise error: {error}");
            }
            let offer_value = reply
                .value("offer")
                .map_err(|_| anyhow::anyhow!("offer promise did not include an offer"))?;
            let offer = offer_value
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .map_err(|_| anyhow::anyhow!("offer promise returned an invalid offer"))?;
            let Some(bin) = weak_bin.upgrade() else {
                anyhow::bail!("webrtcbin dropped before set-local-description");
            };

            let offer_tx_for_install = offer_tx_for_offer.clone();
            let install = gst::Promise::with_change_func(move |reply| {
                let completion = (|| -> anyhow::Result<()> {
                    if let Some(reply) = reply
                        .map_err(|error| anyhow::anyhow!("set-local promise failed: {error:?}"))?
                    {
                        if let Ok(error) = reply.get::<gst::glib::Error>("error") {
                            anyhow::bail!("set-local promise error: {error}");
                        }
                    }
                    Ok(())
                })();
                if let Some(tx) = offer_tx_for_install
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    let _ = tx.send(completion);
                }
            });

            bin.emit_by_name::<()>("set-local-description", &[&offer, &install]);
            Ok(())
        })();

        if let Err(error) = result {
            if let Some(tx) = offer_tx_for_offer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = tx.send(Err(error));
            }
        }
    });

    bin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    offer_rx
        .await
        .map_err(|_| anyhow::anyhow!("offer callback channel closed"))?
}

async fn probe_with_stun(stun_server: Option<&str>) -> ProbeReport {
    if gst::init().is_err() {
        return ProbeReport {
            outcome: ProbeOutcome::Failed,
            candidates: CandidateCounts::default(),
        };
    }

    let mut report = ProbeReport {
        outcome: ProbeOutcome::Failed,
        candidates: CandidateCounts::default(),
    };
    let pipeline = gst::Pipeline::new();
    let Ok(bin) = probe_webrtcbin_with_stun(stun_server) else {
        return report;
    };
    if pipeline.add(&bin).is_err() {
        return report;
    }

    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("clock-rate", 48_000i32)
        .field("payload", 96i32)
        .build();
    let _ = bin.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
        "add-transceiver",
        &[
            &gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
            &Some(caps),
        ],
    );

    let counts = Arc::new(Mutex::new(CandidateCounts::default()));
    let counts_for_candidates = counts.clone();
    let ice_handler = bin.connect("on-ice-candidate", false, move |values| {
        let class = match (values[1].get::<u32>(), values[2].get::<String>()) {
            (Ok(_), Ok(candidate)) => classify_candidate(&candidate),
            _ => CandidateClass::Invalid,
        };
        let mut counts = counts_for_candidates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts.add_class(class);
        None
    });

    let (gather_tx, gather_rx) = tokio::sync::oneshot::channel();
    let gather_tx = Arc::new(Mutex::new(Some(gather_tx)));
    let gather_tx_for_notify = gather_tx.clone();
    let gather_handler = bin.connect_notify(Some("ice-gathering-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        if state == gst_webrtc::WebRTCICEGatheringState::Complete {
            if let Some(tx) = gather_tx_for_notify
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = tx.send(());
            }
        }
    });

    if pipeline.set_state(gst::State::Playing).is_ok() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let offer_result = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            create_offer_and_set_local(&bin),
        )
        .await;
        report.outcome = match offer_result {
            Ok(Ok(())) => match tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                gather_rx,
            )
            .await
            {
                Ok(Ok(())) => ProbeOutcome::Complete,
                Ok(Err(_)) => ProbeOutcome::Failed,
                Err(_) => ProbeOutcome::TimedOut,
            },
            Ok(Err(_)) => ProbeOutcome::Failed,
            Err(_) => ProbeOutcome::TimedOut,
        };
    }

    bin.disconnect(ice_handler);
    bin.disconnect(gather_handler);
    let _ = pipeline.set_state(gst::State::Null);
    report.candidates = *counts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    report
}

pub(crate) async fn probe() -> ProbeReport {
    probe_with_stun(Some(crate::peer::STUN)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_classifies_mdns_ipv6_and_rejects_malformed() {
        let mdns = classify_candidate(
            "candidate:1 1 udp 2122252543 host-abc.local 5000 typ host generation 0",
        );
        let ipv6 = classify_candidate(
            "a=candidate:2 1 tcp 1518280447 2001:4860::1 9 typ srflx tcptype passive",
        );
        let malformed = classify_candidate("candidate:missing fields");

        assert!(matches!(mdns, CandidateClass::Candidate(_)));
        assert!(matches!(ipv6, CandidateClass::Candidate(_)));
        assert!(matches!(malformed, CandidateClass::Invalid));

        let CandidateClass::Candidate(meta) = mdns else {
            unreachable!();
        };
        assert_eq!(meta.family, CandidateFamily::Mdns);
        assert_eq!(meta.kind, CandidateKind::Host);

        let CandidateClass::Candidate(meta) = ipv6 else {
            unreachable!();
        };
        assert_eq!(meta.family, CandidateFamily::Ipv6);
        assert_eq!(meta.transport, CandidateTransport::Tcp);
        assert_eq!(meta.kind, CandidateKind::Srflx);

        assert!(matches!(
            classify_candidate("candidate:1 nope udp 1 10.0.0.1 5000 typ host"),
            CandidateClass::Invalid
        ));
        assert!(matches!(
            classify_candidate("candidate:1 1 udp 0 10.0.0.1 5000 typ host"),
            CandidateClass::Invalid
        ));
        assert!(matches!(
            classify_candidate("candidate:1 1 udp 1 10.0.0.1 0 typ host"),
            CandidateClass::Invalid
        ));
    }

    #[test]
    fn classifier_marks_special_ranges_as_unknown_or_shared() {
        let cgnat = classify_candidate("candidate:1 1 udp 10 100.64.10.10 5000 typ host");
        let unspecified = classify_candidate("candidate:1 1 udp 10 0.0.0.0 5000 typ host");
        let mapped = classify_candidate("candidate:1 1 udp 10 ::ffff:192.168.1.5 5000 typ host");
        let docs = classify_candidate("candidate:1 1 udp 10 2001:db8::1 5000 typ host");

        let CandidateClass::Candidate(cgnat) = cgnat else {
            panic!("cgnat candidate should parse");
        };
        assert_eq!(cgnat.scope, CandidateScope::Shared);

        let CandidateClass::Candidate(unspecified) = unspecified else {
            panic!("unspecified candidate should parse");
        };
        assert_eq!(unspecified.scope, CandidateScope::Unknown);

        let CandidateClass::Candidate(mapped) = mapped else {
            panic!("mapped candidate should parse");
        };
        assert_eq!(mapped.scope, CandidateScope::Private);

        let CandidateClass::Candidate(docs) = docs else {
            panic!("docs candidate should parse");
        };
        assert_eq!(docs.scope, CandidateScope::Unknown);
    }

    #[test]
    fn emitted_events_never_include_candidate_strings() {
        let tracker = IceEventTracker::new("watch");
        let sentinel = "candidate:9 1 udp 1 LEAK-SENTINEL.local 9999 typ host";

        let captured = crate::media_diagnostics::capture_diagnostics(|| {
            tracker.emit(
                IceDirection::Local,
                IceAction::Gathered,
                0,
                classify_candidate(sentinel),
            );
            tracker.emit(
                IceDirection::Remote,
                IceAction::Submitted,
                0,
                classify_candidate(sentinel),
            );
        });

        let json = serde_json::to_string(&captured).unwrap();
        assert!(!json.contains("LEAK-SENTINEL"));
        assert!(json.contains("\"event\":\"ice-candidate\""));
        assert!(json.contains("\"direction\":\"local\""));
        assert!(json.contains("\"direction\":\"remote\""));
    }

    #[test]
    fn selected_route_uses_selected_pair_ids_not_first_candidates() {
        gst::init().unwrap();

        let local_first = gst::Structure::builder("local-first")
            .field("id", "local-first")
            .field("candidate-type", "host")
            .field("protocol", "udp")
            .field("address", "10.0.0.10")
            .build();
        let local_selected = gst::Structure::builder("local-selected")
            .field("id", "local-selected")
            .field("candidate-type", "srflx")
            .field("protocol", "tcp")
            .field("address", "198.51.100.10")
            .build();
        let remote_first = gst::Structure::builder("remote-first")
            .field("id", "remote-first")
            .field("candidate-type", "host")
            .field("protocol", "udp")
            .field("address", "10.0.0.11")
            .build();
        let remote_selected = gst::Structure::builder("remote-selected")
            .field("id", "remote-selected")
            .field("candidate-type", "relay")
            .field("protocol", "udp")
            .field("address", "203.0.114.20")
            .build();
        let selected_pair = gst::Structure::builder("pair-selected")
            .field("id", "pair-selected")
            .field("local-candidate-id", "local-selected")
            .field("remote-candidate-id", "remote-selected")
            .build();
        let transport = gst::Structure::builder("transport")
            .field("id", "transport")
            .field("selected-candidate-pair-id", "pair-selected")
            .build();
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("local-first", local_first)
            .field("local-selected", local_selected)
            .field("remote-first", remote_first)
            .field("remote-selected", remote_selected)
            .field("pair-selected", selected_pair)
            .field("transport", transport)
            .build();

        let route = selected_ice_route(&stats);

        assert!(route.selected);
        let local = route.local.unwrap();
        assert_eq!(local.kind, "srflx");
        assert_eq!(local.transport, "tcp");
        let remote = route.remote.unwrap();
        assert_eq!(remote.kind, "relay");
    }

    #[test]
    fn limiter_does_not_wrap_after_u16_overflow_point() {
        let tracker = IceEventTracker::new("overflow-test");
        let class = classify_candidate("candidate:1 1 udp 1 10.0.0.1 5000 typ host");

        let captured = crate::media_diagnostics::capture_diagnostics(|| {
            for _ in 0..70_000 {
                tracker.emit(IceDirection::Local, IceAction::Gathered, 0, class);
            }
        });

        assert_eq!(captured.len(), usize::from(MAX_ICE_EVENTS_PER_ROLE));
    }

    #[tokio::test]
    async fn isolated_probe_without_stun_gathers_local_host_candidates() {
        let report = probe_with_stun(None).await;

        assert_eq!(report.outcome, ProbeOutcome::Complete);
        assert!(report.candidates.host > 0);
        assert!(report.detail().is_ascii());
        assert!(report.detail().len() <= 240);
    }

    fn maybe_transaction_id(packet: &[u8]) -> Option<[u8; 12]> {
        if packet.len() < 20 {
            return None;
        }
        if u16::from_be_bytes([packet[0], packet[1]]) != 0x0001 {
            return None;
        }
        if [packet[4], packet[5], packet[6], packet[7]] != [0x21, 0x12, 0xA4, 0x42] {
            return None;
        }
        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&packet[8..20]);
        Some(transaction_id)
    }

    fn build_binding_success(transaction_id: [u8; 12], sender: std::net::SocketAddrV4) -> Vec<u8> {
        // Two attributes: MAPPED-ADDRESS + XOR-MAPPED-ADDRESS (12 bytes each).
        let mut response = Vec::with_capacity(44);
        response.extend_from_slice(&0x0101u16.to_be_bytes());
        response.extend_from_slice(&24u16.to_be_bytes());
        response.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        response.extend_from_slice(&transaction_id);

        response.extend_from_slice(&0x0001u16.to_be_bytes());
        response.extend_from_slice(&8u16.to_be_bytes());
        response.push(0);
        response.push(0x01);
        response.extend_from_slice(&sender.port().to_be_bytes());
        response.extend_from_slice(&sender.ip().octets());

        response.extend_from_slice(&0x0020u16.to_be_bytes());
        response.extend_from_slice(&8u16.to_be_bytes());
        response.push(0);
        response.push(0x01);

        let xored_port = sender.port() ^ 0x2112;
        response.extend_from_slice(&xored_port.to_be_bytes());

        let mapped = [203u8, 0, 113, 7];
        let cookie = [0x21u8, 0x12, 0xA4, 0x42];
        response.extend(mapped.iter().zip(cookie).map(|(octet, mask)| octet ^ mask));
        response
    }

    fn local_ipv4_for_stun_uri() -> Ipv4Addr {
        let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0));
        let Ok(probe) = probe else {
            return Ipv4Addr::LOCALHOST;
        };
        if probe.connect((Ipv4Addr::new(1, 1, 1, 1), 53)).is_ok() {
            if let Ok(std::net::SocketAddr::V4(local)) = probe.local_addr() {
                if !local.ip().is_loopback() {
                    return *local.ip();
                }
            }
        }
        Ipv4Addr::LOCALHOST
    }

    async fn spawn_local_stun_server() -> (
        String,
        Arc<AtomicU32>,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .unwrap();
        let address = socket.local_addr().unwrap();
        let stun_host = local_ipv4_for_stun_uri();
        let requests = Arc::new(AtomicU32::new(0));
        let requests_for_worker = requests.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    received = socket.recv_from(&mut buffer) => {
                        let Ok((size, sender)) = received else {
                            break;
                        };
                        let std::net::SocketAddr::V4(sender) = sender else {
                            continue;
                        };
                        let Some(transaction_id) = maybe_transaction_id(&buffer[..size]) else {
                            continue;
                        };
                        requests_for_worker.fetch_add(1, Ordering::AcqRel);
                        let response = build_binding_success(transaction_id, sender);
                        let _ = socket.send_to(&response, sender).await;
                    }
                }
            }
        });
        (
            format!("stun://{}:{}", stun_host, address.port()),
            requests,
            stop_tx,
            worker,
        )
    }

    #[tokio::test]
    async fn isolated_probe_with_local_stun_gathers_srflx_candidates() {
        let (stun_server, requests, stop_tx, worker) = spawn_local_stun_server().await;
        let report = probe_with_stun(Some(&stun_server)).await;
        let _ = stop_tx.send(());
        let _ = worker.await;

        assert!(
            requests.load(Ordering::Acquire) > 0,
            "probe did not send a STUN binding request to {stun_server}"
        );
        assert_eq!(report.outcome, ProbeOutcome::Complete);
        assert!(report.candidates.srflx > 0);
        assert!(report.detail().is_ascii());
        assert!(report.detail().len() <= 240);
    }
}
