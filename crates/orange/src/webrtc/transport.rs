use gstreamer as gst;

use crate::pipeline::Codec;

pub(super) const VIDEO_PAYLOAD: i32 = 96;
pub(super) const VIDEO_RTX_PAYLOAD: i32 = 97;
pub(super) const AUDIO_PAYLOAD: i32 = 111;

pub(crate) fn video_rtp_caps(codec: Codec, frame_rate: u32) -> gst::Caps {
    let builder = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", codec.rtp_encoding())
        .field("payload", VIDEO_PAYLOAD)
        .field("clock-rate", 90_000i32)
        .field("a-framerate", frame_rate.to_string());
    match codec {
        Codec::H264 => builder.field("packetization-mode", "1").build(),
        Codec::Av1 | Codec::H265 => builder.build(),
    }
}

pub(crate) fn audio_rtp_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("payload", AUDIO_PAYLOAD)
        .field("clock-rate", 48_000i32)
        .field("encoding-params", "2")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(caps: &gst::Caps) -> i32 {
        caps.structure(0).unwrap().get::<i32>("payload").unwrap()
    }

    #[test]
    fn generated_caps_use_non_overlapping_payload_assignments() {
        gst::init().unwrap();
        for codec in [Codec::Av1, Codec::H265, Codec::H264] {
            assert_eq!(payload(&video_rtp_caps(codec, 60)), VIDEO_PAYLOAD);
        }
        assert_eq!(payload(&audio_rtp_caps()), AUDIO_PAYLOAD);
        assert_ne!(AUDIO_PAYLOAD, VIDEO_PAYLOAD);
        assert_ne!(AUDIO_PAYLOAD, VIDEO_RTX_PAYLOAD);
    }
}
