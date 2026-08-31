use serde::{Deserialize, Serialize};

/// Messages exchanged between peers and the relay.
///
/// Anything carrying a `peer` field is routed: a host may be talking to several
/// viewers at once, so SDP and ICE must say which conversation they belong to.
/// Viewers do not know their own id - the relay stamps it on the way through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    /// Peer -> server, optional first message: prove who you are.
    Authenticate { session: String },
    /// Server -> peer: identity accepted.
    Authenticated { name: String },
    /// Host -> server: open a room.
    Host,
    /// Server -> host: the room is open under this code.
    Hosting {
        code: String,
        #[serde(default)]
        diagnostic_session: Option<String>,
    },
    /// Viewer -> server: join a room.
    Join { code: String },
    /// Server -> viewer: whose stream this is.
    StreamInfo {
        #[serde(default)]
        host_name: Option<String>,
        #[serde(default)]
        diagnostic_session: Option<String>,
    },
    /// Server -> host: a viewer arrived, start negotiating with it.
    ViewerJoined {
        peer: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Server -> host: a viewer disconnected, tear its branch down.
    ViewerLeft { peer: String },
    /// Either direction: session description.
    Sdp {
        #[serde(default)]
        peer: String,
        kind: String,
        sdp: String,
    },
    /// Either direction: ICE candidate.
    Ice {
        #[serde(default)]
        peer: String,
        mline: u32,
        candidate: String,
    },
    /// Server -> peer: something went wrong.
    Error { message: String },
}

impl Signal {
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Overwrite the routing id, so a viewer cannot claim to be another peer.
    pub(super) fn with_peer(self, id: &str) -> Self {
        match self {
            Signal::Sdp { kind, sdp, .. } => Signal::Sdp {
                peer: id.to_string(),
                kind,
                sdp,
            },
            Signal::Ice {
                mline, candidate, ..
            } => Signal::Ice {
                peer: id.to_string(),
                mline,
                candidate,
            },
            other => other,
        }
    }

    pub(super) fn peer_id(&self) -> Option<&str> {
        match self {
            Signal::Sdp { peer, .. } | Signal::Ice { peer, .. } => Some(peer),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_session_messages_deserialize_without_diagnostic_session() {
        let hosting: Signal =
            serde_json::from_str(r#"{"type":"hosting","code":"ABC-234"}"#).unwrap();
        let Signal::Hosting {
            diagnostic_session: hosting_session,
            ..
        } = hosting
        else {
            panic!("expected hosting signal");
        };
        assert_eq!(hosting_session, None);

        let stream_info: Signal =
            serde_json::from_str(r#"{"type":"streaminfo","host_name":null}"#).unwrap();
        let Signal::StreamInfo {
            diagnostic_session: viewer_session,
            ..
        } = stream_info
        else {
            panic!("expected stream info signal");
        };
        assert_eq!(viewer_session, None);
    }
}
