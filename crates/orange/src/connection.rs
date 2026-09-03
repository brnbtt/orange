//! Privacy-safe connection progress shared by signaling, WebRTC, and playback.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionFailure {
    Signaling,
    Room,
    Negotiation,
    Network,
    Playback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionEvent {
    WatchStarted,
    StreamInfo,
    SdpOfferReceived,
    SdpOfferInstalled,
    SdpAnswerInstalled,
    IceGathering,
    IceChecking,
    IceConnected,
    PeerConnected,
    PadAdded,
    ReceiveBranchReady,
    FirstVideoFrame,
    Failed(ConnectionFailure),
}

impl ConnectionEvent {
    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::WatchStarted => "watch-started",
            Self::StreamInfo => "stream-info",
            Self::SdpOfferReceived => "sdp-offer-received",
            Self::SdpOfferInstalled => "sdp-offer-installed",
            Self::SdpAnswerInstalled => "sdp-answer-installed",
            Self::IceGathering => "ice-gathering",
            Self::IceChecking => "ice-checking",
            Self::IceConnected => "ice-connected",
            Self::PeerConnected => "peer-connected",
            Self::PadAdded => "pad-added",
            Self::ReceiveBranchReady => "receive-branch-ready",
            Self::FirstVideoFrame => "first-video-frame",
            Self::Failed(ConnectionFailure::Signaling) => "signaling-failed",
            Self::Failed(ConnectionFailure::Room) => "room-failed",
            Self::Failed(ConnectionFailure::Negotiation) => "negotiation-failed",
            Self::Failed(ConnectionFailure::Network) => "network-failed",
            Self::Failed(ConnectionFailure::Playback) => "playback-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionStage {
    JoiningRoom,
    ExchangingStreamDetails,
    FindingDirectRoute,
    SecuringConnection,
    StartingMedia,
    Connected,
    Failed(ConnectionFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionCopy {
    pub(crate) primary: &'static str,
    pub(crate) title: &'static str,
    pub(crate) detail: &'static str,
}

impl ConnectionStage {
    const fn rank(self) -> Option<u8> {
        match self {
            Self::JoiningRoom => Some(0),
            Self::ExchangingStreamDetails => Some(1),
            Self::FindingDirectRoute => Some(2),
            Self::SecuringConnection => Some(3),
            Self::StartingMedia => Some(4),
            Self::Connected => Some(5),
            Self::Failed(_) => None,
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Connected | Self::Failed(_))
    }

    pub(crate) const fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }

    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::JoiningRoom => "joining-room",
            Self::ExchangingStreamDetails => "exchanging-stream-details",
            Self::FindingDirectRoute => "finding-direct-route",
            Self::SecuringConnection => "securing-connection",
            Self::StartingMedia => "starting-video-and-audio",
            Self::Connected => "connected",
            Self::Failed(ConnectionFailure::Signaling) => "failed-signaling",
            Self::Failed(ConnectionFailure::Room) => "failed-room",
            Self::Failed(ConnectionFailure::Negotiation) => "failed-negotiation",
            Self::Failed(ConnectionFailure::Network) => "failed-network",
            Self::Failed(ConnectionFailure::Playback) => "failed-playback",
        }
    }

    pub(crate) const fn copy(self) -> ConnectionCopy {
        match self {
            Self::JoiningRoom => ConnectionCopy {
                primary: "Connecting...",
                title: "Joining room",
                detail: "Contacting the room and waiting for the host",
            },
            Self::ExchangingStreamDetails => ConnectionCopy {
                primary: "Connecting...",
                title: "Exchanging stream details",
                detail: "Orange is agreeing how to receive the stream",
            },
            Self::FindingDirectRoute => ConnectionCopy {
                primary: "Connecting...",
                title: "Finding a direct route",
                detail: "ICE is checking available network paths",
            },
            Self::SecuringConnection => ConnectionCopy {
                primary: "Connecting...",
                title: "Securing connection",
                detail: "The direct route is ready; encryption is finishing",
            },
            Self::StartingMedia => ConnectionCopy {
                primary: "Connecting...",
                title: "Starting video and audio",
                detail: "The secure connection is ready; media is starting",
            },
            Self::Connected => ConnectionCopy {
                primary: "Connected",
                title: "Video is ready",
                detail: "Playback is starting",
            },
            Self::Failed(ConnectionFailure::Signaling) => ConnectionCopy {
                primary: "Couldn't connect",
                title: "Orange couldn't reach the service",
                detail: "Check your internet connection and try again",
            },
            Self::Failed(ConnectionFailure::Room) => ConnectionCopy {
                primary: "Couldn't connect",
                title: "The room isn't available",
                detail: "Check the code you entered or ask the host to start sharing",
            },
            Self::Failed(ConnectionFailure::Negotiation) => ConnectionCopy {
                primary: "Couldn't connect",
                title: "Stream details couldn't be exchanged",
                detail: "Ask the host to stop sharing, then try again",
            },
            Self::Failed(ConnectionFailure::Network) => ConnectionCopy {
                primary: "Couldn't connect",
                title: "No direct route was found",
                detail: "Check both networks and try again",
            },
            Self::Failed(ConnectionFailure::Playback) => ConnectionCopy {
                primary: "Couldn't start playback",
                title: "Video or audio couldn't be started",
                detail: "Try joining again; if it persists, restart Orange",
            },
        }
    }
}

#[derive(Debug)]
struct ConnectionModel {
    stage: ConnectionStage,
}

impl Default for ConnectionModel {
    fn default() -> Self {
        Self {
            stage: ConnectionStage::JoiningRoom,
        }
    }
}

impl ConnectionModel {
    fn stage(&self) -> ConnectionStage {
        self.stage
    }

    fn advance(&mut self, event: ConnectionEvent) -> bool {
        if self.stage.is_terminal() {
            return false;
        }
        let next = match event {
            ConnectionEvent::WatchStarted => ConnectionStage::JoiningRoom,
            ConnectionEvent::StreamInfo
            | ConnectionEvent::SdpOfferReceived
            | ConnectionEvent::SdpOfferInstalled
            | ConnectionEvent::SdpAnswerInstalled
            | ConnectionEvent::PadAdded
            | ConnectionEvent::ReceiveBranchReady => ConnectionStage::ExchangingStreamDetails,
            ConnectionEvent::IceGathering | ConnectionEvent::IceChecking => {
                ConnectionStage::FindingDirectRoute
            }
            ConnectionEvent::IceConnected => ConnectionStage::SecuringConnection,
            ConnectionEvent::PeerConnected => ConnectionStage::StartingMedia,
            ConnectionEvent::FirstVideoFrame => ConnectionStage::Connected,
            ConnectionEvent::Failed(failure) => ConnectionStage::Failed(failure),
        };
        if !matches!(next, ConnectionStage::Failed(_))
            && next.rank().unwrap_or_default() <= self.stage.rank().unwrap_or_default()
        {
            return false;
        }
        self.stage = next;
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionTransition {
    pub(crate) stage: ConnectionStage,
    pub(crate) event: ConnectionEvent,
    pub(crate) elapsed: Duration,
    pub(crate) previous_stage: Duration,
}

#[derive(Debug, Default)]
struct TrackerState {
    active: bool,
    model: ConnectionModel,
    started: Option<Instant>,
    changed: Option<Instant>,
}

#[derive(Debug, Default)]
pub(crate) struct ConnectionTracker {
    state: Mutex<TrackerState>,
}

impl ConnectionTracker {
    pub(crate) fn begin(&self) -> Option<ConnectionTransition> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.active {
            return None;
        }
        let now = Instant::now();
        state.active = true;
        state.model = ConnectionModel::default();
        state.started = Some(now);
        state.changed = Some(now);
        Some(ConnectionTransition {
            stage: state.model.stage(),
            event: ConnectionEvent::WatchStarted,
            elapsed: Duration::ZERO,
            previous_stage: Duration::ZERO,
        })
    }

    pub(crate) fn advance(&self, event: ConnectionEvent) -> Option<ConnectionTransition> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active || !state.model.advance(event) {
            return None;
        }
        let now = Instant::now();
        let elapsed = state
            .started
            .map_or(Duration::ZERO, |at| now.duration_since(at));
        let previous_stage = state
            .changed
            .map_or(Duration::ZERO, |at| now.duration_since(at));
        state.changed = Some(now);
        Some(ConnectionTransition {
            stage: state.model.stage(),
            event,
            elapsed,
            previous_stage,
        })
    }

    pub(crate) fn snapshot(&self) -> Option<ConnectionStage> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active.then(|| state.model.stage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_events_map_to_the_six_user_facing_stages() {
        let mut model = ConnectionModel::default();
        assert_eq!(model.stage(), ConnectionStage::JoiningRoom);

        for (event, expected) in [
            (
                ConnectionEvent::StreamInfo,
                ConnectionStage::ExchangingStreamDetails,
            ),
            (
                ConnectionEvent::IceChecking,
                ConnectionStage::FindingDirectRoute,
            ),
            (
                ConnectionEvent::IceConnected,
                ConnectionStage::SecuringConnection,
            ),
            (
                ConnectionEvent::PeerConnected,
                ConnectionStage::StartingMedia,
            ),
            (ConnectionEvent::FirstVideoFrame, ConnectionStage::Connected),
        ] {
            model.advance(event);
            assert_eq!(model.stage(), expected);
        }
    }

    #[test]
    fn signaling_and_receive_setup_events_do_not_claim_media_is_starting() {
        for event in [
            ConnectionEvent::StreamInfo,
            ConnectionEvent::SdpOfferReceived,
            ConnectionEvent::SdpOfferInstalled,
            ConnectionEvent::SdpAnswerInstalled,
            ConnectionEvent::PadAdded,
            ConnectionEvent::ReceiveBranchReady,
        ] {
            let mut model = ConnectionModel::default();
            model.advance(event);
            assert_eq!(model.stage(), ConnectionStage::ExchangingStreamDetails);
        }
    }

    #[test]
    fn late_asynchronous_callbacks_cannot_move_progress_backward() {
        let mut model = ConnectionModel::default();
        model.advance(ConnectionEvent::PeerConnected);

        for late in [
            ConnectionEvent::StreamInfo,
            ConnectionEvent::SdpAnswerInstalled,
            ConnectionEvent::IceGathering,
            ConnectionEvent::IceChecking,
            ConnectionEvent::IceConnected,
            ConnectionEvent::PadAdded,
            ConnectionEvent::ReceiveBranchReady,
        ] {
            assert!(!model.advance(late));
            assert_eq!(model.stage(), ConnectionStage::StartingMedia);
        }
    }

    #[test]
    fn terminal_failure_cannot_be_overwritten_by_a_later_callback() {
        let mut model = ConnectionModel::default();
        assert!(model.advance(ConnectionEvent::Failed(ConnectionFailure::Network)));

        for late in [
            ConnectionEvent::StreamInfo,
            ConnectionEvent::IceConnected,
            ConnectionEvent::PeerConnected,
            ConnectionEvent::FirstVideoFrame,
        ] {
            assert!(!model.advance(late));
            assert_eq!(
                model.stage(),
                ConnectionStage::Failed(ConnectionFailure::Network)
            );
        }
    }

    #[test]
    fn first_video_frame_is_terminal_connected_progress() {
        let mut model = ConnectionModel::default();
        assert!(model.advance(ConnectionEvent::FirstVideoFrame));
        assert!(!model.advance(ConnectionEvent::Failed(ConnectionFailure::Network)));
        assert_eq!(model.stage(), ConnectionStage::Connected);
    }

    #[test]
    fn tracker_is_inactive_until_connection_feedback_begins() {
        let tracker = ConnectionTracker::default();
        assert_eq!(tracker.snapshot(), None);

        let transition = tracker.begin().expect("first begin should be visible");
        assert_eq!(transition.stage, ConnectionStage::JoiningRoom);
        assert_eq!(transition.event, ConnectionEvent::WatchStarted);
        assert_eq!(transition.elapsed, std::time::Duration::ZERO);
        assert_eq!(transition.previous_stage, std::time::Duration::ZERO);
        assert_eq!(tracker.snapshot(), Some(ConnectionStage::JoiningRoom));
        assert!(tracker.begin().is_none());
    }

    #[test]
    fn tracker_reports_only_accepted_visible_transitions() {
        let tracker = ConnectionTracker::default();
        tracker.begin();

        let exchanging = tracker
            .advance(ConnectionEvent::SdpOfferReceived)
            .expect("SDP should advance the visible stage");
        assert_eq!(exchanging.stage, ConnectionStage::ExchangingStreamDetails);
        assert_eq!(exchanging.event, ConnectionEvent::SdpOfferReceived);

        assert!(tracker.advance(ConnectionEvent::StreamInfo).is_none());
        assert_eq!(
            tracker.snapshot(),
            Some(ConnectionStage::ExchangingStreamDetails)
        );
    }

    #[test]
    fn every_stage_exposes_fixed_privacy_safe_copy() {
        for stage in [
            ConnectionStage::JoiningRoom,
            ConnectionStage::ExchangingStreamDetails,
            ConnectionStage::FindingDirectRoute,
            ConnectionStage::SecuringConnection,
            ConnectionStage::StartingMedia,
            ConnectionStage::Connected,
            ConnectionStage::Failed(ConnectionFailure::Signaling),
            ConnectionStage::Failed(ConnectionFailure::Room),
            ConnectionStage::Failed(ConnectionFailure::Negotiation),
            ConnectionStage::Failed(ConnectionFailure::Network),
            ConnectionStage::Failed(ConnectionFailure::Playback),
        ] {
            let copy = stage.copy();
            assert!(!copy.primary.is_empty());
            assert!(!copy.title.is_empty());
            assert!(!copy.detail.is_empty());
            let visible = format!("{} {} {}", copy.primary, copy.title, copy.detail);
            for sensitive in ["candidate:", "a=", "room code", "192.168."] {
                assert!(!visible.contains(sensitive), "unsafe copy for {stage:?}");
            }
        }
    }
}
