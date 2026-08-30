use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;

use crate::pipeline::Codec;

pub(super) const VIDEO_PAYLOAD: i32 = 96;
pub(super) const VIDEO_RTX_PAYLOAD: i32 = 97;
pub(super) const AUDIO_PAYLOAD: i32 = 111;
const VIDEO_PAYLOAD_U8: u8 = VIDEO_PAYLOAD as u8;
const VIDEO_RTX_PAYLOAD_U8: u8 = VIDEO_RTX_PAYLOAD as u8;
const AUDIO_PAYLOAD_U8: u8 = AUDIO_PAYLOAD as u8;

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

pub(crate) fn configure_receive_transport(bin: &gst::Element, live_output: bool) -> Result<()> {
    if !live_output {
        return Ok(());
    }
    const LATENCY_MS: u32 = 100;
    bin.set_property("latency", LATENCY_MS);
    let rtpbin = bin
        .dynamic_cast_ref::<gst::Bin>()
        .context("webrtcbin is not a GstBin")?
        .by_name("rtpbin")
        .context("webrtcbin has no internal rtpbin")?;
    rtpbin.set_property("latency", LATENCY_MS);
    rtpbin.set_property("drop-on-latency", false);
    rtpbin.connect("new-jitterbuffer", false, move |values| {
        let Ok(jitterbuffer) = values[1].get::<gst::Element>() else {
            return None;
        };
        prepare_media_jitterbuffer(&jitterbuffer);
        None
    });
    if std::env::var("ORANGE_RTP_BUFFER_MODE").as_deref() == Ok("none") {
        rtpbin.set_property_from_str("buffer-mode", "none");
        rtpbin.set_property_from_str("rtcp-sync", "never");
    }
    Ok(())
}

fn prepare_media_jitterbuffer(jitterbuffer: &gst::Element) {
    jitterbuffer.set_property("latency", 100u32);
    jitterbuffer.set_property("do-lost", true);
    // Bundled audio and video can share one RTP session index. Default to no
    // silent drops until the first RTP packet identifies its payload type.
    jitterbuffer.set_property("drop-on-latency", false);
    let jitterbuffer_weak = jitterbuffer.downgrade();
    if let Some(sink) = jitterbuffer.static_pad("sink") {
        sink.add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::BUFFER,
            move |_, info| {
                let classified = match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => buffer
                        .map_readable()
                        .ok()
                        .and_then(|map| rtp_payload_type(map.as_slice()))
                        .and_then(|payload| {
                            jitterbuffer_weak.upgrade().map(|jitterbuffer| {
                                configure_jitterbuffer_for_payload(&jitterbuffer, payload)
                            })
                        })
                        .unwrap_or(false),
                    Some(gst::PadProbeData::Event(event)) => {
                        let gst::EventView::Caps(caps) = event.view() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        jitterbuffer_weak.upgrade().is_some_and(|jitterbuffer| {
                            configure_jitterbuffer_for_caps(&jitterbuffer, caps.caps())
                        })
                    }
                    _ => false,
                };
                if classified {
                    gst::PadProbeReturn::Remove
                } else {
                    gst::PadProbeReturn::Ok
                }
            },
        );
    }
}

fn rtp_payload_type(packet: &[u8]) -> Option<u8> {
    (packet.len() >= 2 && packet[0] >> 6 == 2).then(|| packet[1] & 0x7f)
}

fn configure_jitterbuffer_for_payload(jitterbuffer: &gst::Element, payload: u8) -> bool {
    match payload {
        AUDIO_PAYLOAD_U8 => {
            jitterbuffer.set_property("drop-on-latency", false);
            true
        }
        VIDEO_PAYLOAD_U8 | VIDEO_RTX_PAYLOAD_U8 => {
            jitterbuffer.set_property("drop-on-latency", true);
            true
        }
        _ => false,
    }
}

fn configure_jitterbuffer_for_caps(jitterbuffer: &gst::Element, caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    let payload = structure
        .get::<i32>("payload")
        .ok()
        .or_else(|| {
            structure
                .get::<u32>("payload")
                .ok()
                .map(|value| value as i32)
        })
        .and_then(|value| u8::try_from(value).ok());
    let encoding = structure.get::<String>("encoding-name").ok();
    if let Some(payload) = payload {
        if configure_jitterbuffer_for_payload(jitterbuffer, payload) {
            return true;
        }
    }
    let is_audio = encoding.as_deref() == Some("OPUS");
    let is_video = matches!(encoding.as_deref(), Some("AV1" | "H264" | "H265"));
    if !is_audio && !is_video {
        return false;
    }
    // Video stays at the live edge. Opus must turn late packets into GAP events
    // so opusdec can conceal them instead of joining discontinuous waveforms.
    jitterbuffer.set_property("drop-on-latency", is_video);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jitterbuffer() -> gst::Element {
        gst::ElementFactory::make("rtpjitterbuffer")
            .build()
            .unwrap()
    }

    fn payload(caps: &gst::Caps) -> i32 {
        caps.structure(0).unwrap().get::<i32>("payload").unwrap()
    }

    #[test]
    fn generated_caps_select_the_expected_jitterbuffer_policy() {
        gst::init().unwrap();
        for codec in [Codec::Av1, Codec::H265, Codec::H264] {
            let jitter = jitterbuffer();
            assert!(configure_jitterbuffer_for_caps(
                &jitter,
                video_rtp_caps(codec, 60).as_ref(),
            ));
            assert!(jitter.property::<bool>("drop-on-latency"));
        }
        let audio = jitterbuffer();
        assert!(configure_jitterbuffer_for_caps(
            &audio,
            audio_rtp_caps().as_ref(),
        ));
        assert!(!audio.property::<bool>("drop-on-latency"));
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

    #[test]
    fn receiver_transport_keeps_video_live_without_silently_dropping_audio() {
        gst::init().unwrap();
        let bin = gst::ElementFactory::make("webrtcbin")
            .name("latency-test")
            .build()
            .unwrap();

        configure_receive_transport(&bin, true).unwrap();

        assert_eq!(bin.property::<u32>("latency"), 100);
        let rtpbin = bin
            .dynamic_cast_ref::<gst::Bin>()
            .unwrap()
            .by_name("rtpbin")
            .unwrap();
        assert_eq!(rtpbin.property::<u32>("latency"), 100);
        assert!(!rtpbin.property::<bool>("drop-on-latency"));

        let video = jitterbuffer();
        prepare_media_jitterbuffer(&video);
        assert!(configure_jitterbuffer_for_caps(
            &video,
            video_rtp_caps(Codec::Av1, 60).as_ref(),
        ));
        assert!(video.property::<bool>("drop-on-latency"));

        let audio = jitterbuffer();
        prepare_media_jitterbuffer(&audio);
        assert!(configure_jitterbuffer_for_caps(
            &audio,
            audio_rtp_caps().as_ref(),
        ));
        assert_eq!(audio.property::<u32>("latency"), 100);
        assert!(audio.property::<bool>("do-lost"));
        assert!(!audio.property::<bool>("drop-on-latency"));
        assert_eq!(rtp_payload_type(&[0x80, 0xe0]), Some(VIDEO_PAYLOAD_U8));
        assert_eq!(rtp_payload_type(&[0x80, 0xef]), Some(AUDIO_PAYLOAD_U8));
        assert_eq!(rtp_payload_type(&[0x00, 0x60]), None);
    }
}
