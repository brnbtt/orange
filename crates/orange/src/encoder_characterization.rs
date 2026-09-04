//! The `characterize-bitrate` subcommand: a developer benchmark, not part of
//! any user-facing path.
//!
//! It sits outside `media_diagnostics/` on purpose. That tree instruments a
//! live session and ships in every binary that streams; this one drives an
//! encoder through scripted phases in a throwaway process and only ever runs
//! when someone asks for it by name.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use serde::Serialize;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::pipeline::{configure_encoder, Codec};

const KEYFRAME_BURST_WINDOW_MS: u64 = 500;
const KEYFRAME_AFTER_MUTATION_MS: u64 = 250;
const MAX_OUTPUT_GAP_MS: u64 = 100;
const MAX_PROPERTY_SET_MS: u64 = 250;
const MAX_RECEIVER_FRAME_LAG: u64 = 2;
const MAX_RECEIVER_TIME_LAG_MS: u64 = 100;
const SELECTED_BITRATE_KBPS: u32 = 22_800;
const FRAME_RATE: u64 = 60;
const PHASE_DURATION_MS: u64 = 3_500;
const EXPECTED_PHASE_FRAMES: u64 = FRAME_RATE * PHASE_DURATION_MS / 1_000;
const MIN_PHASE_FRAMES: u64 = EXPECTED_PHASE_FRAMES * 9 / 10;
const PHASE_DURATION: Duration = Duration::from_millis(PHASE_DURATION_MS);
const WARMUP_DURATION: Duration = Duration::from_secs(2);
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ENCODERS: [&str; 2] = ["mfh265enc", "nvd3d11h265enc"];

#[derive(Default)]
struct FrameProgress {
    state: Mutex<FrameProgressState>,
}

#[derive(Default)]
struct FrameProgressState {
    frames: u64,
    bytes: u64,
    last_ms: Option<u64>,
    max_gap_ms: u64,
    keyframes: u64,
    keyframe_bursts: u64,
    last_keyframe_ms: Option<u64>,
    first_keyframe_in_phase_ms: Option<u64>,
}

struct PhaseStart {
    started_ms: u64,
    frames: u64,
    bytes: u64,
    keyframes: u64,
    keyframe_bursts: u64,
}

#[derive(Clone, Copy, Serialize)]
struct PhaseMeasurement {
    frames: u64,
    bytes: u64,
    keyframes: u64,
    keyframe_bursts: u64,
    first_keyframe_after_phase_start_ms: Option<u64>,
    max_gap_ms: u64,
    silent_ms: u64,
}

fn phase_is_continuous(measurement: &PhaseMeasurement) -> bool {
    measurement.frames > 0
        && measurement.max_gap_ms <= MAX_OUTPUT_GAP_MS
        && measurement.silent_ms <= MAX_OUTPUT_GAP_MS
        && measurement.keyframe_bursts == 0
}

fn phase_pair_is_continuous(encoded: &PhaseMeasurement, receiver: &PhaseMeasurement) -> bool {
    phase_is_continuous(encoded)
        && phase_is_continuous(receiver)
        && encoded.frames >= MIN_PHASE_FRAMES
        && receiver.frames >= MIN_PHASE_FRAMES
        && encoded.frames.abs_diff(receiver.frames) <= MAX_RECEIVER_FRAME_LAG
        && encoded.silent_ms.abs_diff(receiver.silent_ms) <= MAX_RECEIVER_TIME_LAG_MS
}

fn bitrate_response_is_effective(observed_kbps: [u64; 4]) -> bool {
    let [selected, half, quarter, rebound] = observed_kbps.map(u128::from);
    let targets = [22_800u128, 11_400, 5_700, 22_800];
    let within_target_range = [selected, half, quarter, rebound]
        .into_iter()
        .zip(targets)
        .all(|(observed, target)| observed * 10 >= target * 5 && observed * 10 <= target * 14);

    within_target_range
        && half * 20 <= selected * 13
        && quarter * 5 <= half * 4
        && quarter * 5 <= selected * 2
        && rebound * 5 >= selected * 4
        && rebound * 5 <= selected * 7
}

fn keyframe_near_mutation(measurement: &PhaseMeasurement, property_set_ms: u64) -> bool {
    measurement
        .first_keyframe_after_phase_start_ms
        .is_some_and(|delay| delay <= property_set_ms.saturating_add(KEYFRAME_AFTER_MUTATION_MS))
}

fn characterization_verdict(
    mutable_in_playing: bool,
    continuous: bool,
    bitrate_response_effective: bool,
) -> (bool, &'static str) {
    if !mutable_in_playing {
        (false, "bitrate is not advertised mutable in PLAYING")
    } else if !continuous {
        (
            false,
            "live mutation interrupted encoded or receiver continuity",
        )
    } else if !bitrate_response_effective {
        (
            false,
            "encoded output did not follow the requested bitrate changes",
        )
    } else {
        (true, "live mutation passed the bounded characterization")
    }
}

fn worker_timed_out(elapsed: Duration) -> bool {
    elapsed >= WORKER_TIMEOUT
}

impl FrameProgress {
    fn record(&self, started: Instant, bytes: usize, keyframe: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::record_locked(
            &mut state,
            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            u64::try_from(bytes).unwrap_or(u64::MAX),
            keyframe,
        );
    }

    #[cfg(test)]
    fn record_at(&self, at_ms: u64, bytes: u64, keyframe: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::record_locked(&mut state, at_ms, bytes, keyframe);
    }

    fn record_locked(state: &mut FrameProgressState, at_ms: u64, bytes: u64, keyframe: bool) {
        if let Some(previous) = state.last_ms.replace(at_ms) {
            state.max_gap_ms = state.max_gap_ms.max(at_ms.saturating_sub(previous));
        }
        state.frames = state.frames.saturating_add(1);
        state.bytes = state.bytes.saturating_add(bytes);

        if keyframe {
            if state.first_keyframe_in_phase_ms.is_none() {
                state.first_keyframe_in_phase_ms = Some(at_ms);
            }
            if state
                .last_keyframe_ms
                .is_some_and(|previous| at_ms.saturating_sub(previous) < KEYFRAME_BURST_WINDOW_MS)
            {
                state.keyframe_bursts = state.keyframe_bursts.saturating_add(1);
            }
            state.last_keyframe_ms = Some(at_ms);
            state.keyframes = state.keyframes.saturating_add(1);
        }
    }

    fn begin_phase(&self, started_ms: u64) -> PhaseStart {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.max_gap_ms = 0;
        state.first_keyframe_in_phase_ms = None;
        PhaseStart {
            started_ms,
            frames: state.frames,
            bytes: state.bytes,
            keyframes: state.keyframes,
            keyframe_bursts: state.keyframe_bursts,
        }
    }

    fn end_phase(&self, start: PhaseStart, now_ms: u64) -> PhaseMeasurement {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        PhaseMeasurement {
            frames: state.frames.saturating_sub(start.frames),
            bytes: state.bytes.saturating_sub(start.bytes),
            keyframes: state.keyframes.saturating_sub(start.keyframes),
            keyframe_bursts: state.keyframe_bursts.saturating_sub(start.keyframe_bursts),
            first_keyframe_after_phase_start_ms: state
                .first_keyframe_in_phase_ms
                .map(|at_ms| at_ms.saturating_sub(start.started_ms)),
            max_gap_ms: state.max_gap_ms,
            silent_ms: state
                .last_ms
                .map_or(now_ms, |last_ms| now_ms.saturating_sub(last_ms)),
        }
    }

    fn frames(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .frames
    }
}

#[derive(Default, Serialize)]
struct StateChanges {
    pipeline: u64,
    encoder: u64,
}

#[derive(Serialize)]
struct PhaseReport {
    requested_bitrate_kbps: u32,
    applied_bitrate_kbps: u32,
    property_set_ms: u64,
    observed_video_kbps: u64,
    encoded: PhaseMeasurement,
    receiver: PhaseMeasurement,
    state_changes: StateChanges,
    keyframe_near_mutation: bool,
    continuous: bool,
}

#[derive(Serialize)]
struct CharacterizationReport<'a> {
    encoder: &'a str,
    bitrate_mutable_in_playing: bool,
    resolution: &'static str,
    frame_rate: u32,
    phase_duration_ms: u64,
    phases: Vec<PhaseReport>,
    bitrate_response_effective: bool,
    safe_for_live_budgeting: bool,
    verdict: &'static str,
}

pub(crate) fn run(requested_encoder: Option<&str>) -> Result<()> {
    let encoders: Vec<_> = match requested_encoder {
        Some(encoder) if ENCODERS.contains(&encoder) => vec![encoder],
        Some(encoder) => bail!(
            "unsupported characterization encoder '{encoder}' (expected {})",
            ENCODERS.join(" or ")
        ),
        None => ENCODERS
            .into_iter()
            .filter(|encoder| gst::ElementFactory::find(encoder).is_some())
            .collect(),
    };
    if encoders.is_empty() {
        bail!("no H.265 hardware encoder is available for characterization");
    }

    let mut all_safe = true;
    let mut failures = Vec::new();
    for encoder in encoders {
        match run_bounded_worker(encoder) {
            Ok(true) => {}
            Ok(false) => {
                all_safe = false;
                failures.push(format!("{encoder}: failed the safety gate"));
            }
            Err(error) => {
                all_safe = false;
                failures.push(format!("{encoder}: {error:#}"));
            }
        }
    }

    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("[bitrate-characterization] {failure}");
        }
    }
    if !all_safe {
        bail!(
            "live bitrate mutation is not safe across the available H.265 encoders; host budgeting remains disabled"
        );
    }
    Ok(())
}

pub(crate) fn run_worker(encoder: &str) -> Result<()> {
    if !ENCODERS.contains(&encoder) {
        bail!(
            "unsupported characterization encoder '{encoder}' (expected {})",
            ENCODERS.join(" or ")
        );
    }
    if gst::ElementFactory::find(encoder).is_none() {
        bail!("encoder '{encoder}' is unavailable");
    }

    let report = characterize(encoder)?;
    let safe = report.safe_for_live_budgeting;
    println!(
        "[bitrate-characterization] {}",
        serde_json::to_string(&report)?
    );
    if !safe {
        bail!("{encoder} is not safe for live bitrate budgeting");
    }
    Ok(())
}

fn run_bounded_worker(encoder: &str) -> Result<bool> {
    let executable = std::env::current_exe().context("could not locate the Orange executable")?;
    let mut child = Command::new(executable)
        .arg("characterize-bitrate-worker")
        .args(["--encoder", encoder])
        .spawn()
        .with_context(|| format!("could not start {encoder} characterization worker"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => {}
            Err(error) => {
                let cleanup = terminate_worker(&mut child, encoder);
                return match cleanup {
                    Ok(()) => Err(error).with_context(|| {
                        format!("could not inspect {encoder} characterization worker")
                    }),
                    Err(cleanup_error) => Err(anyhow::anyhow!(
                        "could not inspect {encoder} characterization worker: {error}; cleanup failed: {cleanup_error:#}"
                    )),
                };
            }
        }
        if worker_timed_out(started.elapsed()) {
            terminate_worker(&mut child, encoder)?;
            bail!(
                "characterization exceeded its {} second limit",
                WORKER_TIMEOUT.as_secs()
            );
        }
        thread::sleep(WORKER_POLL_INTERVAL);
    }
}

fn terminate_worker(child: &mut Child, encoder: &str) -> Result<()> {
    child
        .kill()
        .with_context(|| format!("could not stop {encoder} characterization worker"))?;
    let deadline = Instant::now() + WORKER_KILL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(WORKER_POLL_INTERVAL),
            Ok(None) => {
                bail!(
                    "{encoder} characterization worker did not exit within {} seconds of termination",
                    WORKER_KILL_TIMEOUT.as_secs()
                )
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not reap {encoder} characterization worker"));
            }
        }
    }
}

fn characterize(encoder_factory: &str) -> Result<CharacterizationReport<'_>> {
    let description = format!(
        "videotestsrc is-live=true pattern=snow \
         ! video/x-raw,format=BGRA,width=1920,height=1080,framerate=60/1 \
         ! d3d11upload ! d3d11convert \
         ! video/x-raw(memory:D3D11Memory),format=NV12,width=1920,height=1080,framerate=60/1 \
         ! queue max-size-buffers=3 leaky=downstream \
         ! {encoder_factory} name=characterize-encoder bitrate={SELECTED_BITRATE_KBPS} \
         ! h265parse name=encoded-output \
         ! queue max-size-buffers=200 \
         ! d3d11h265dec \
         ! identity name=receiver-output \
         ! fakesink sync=false"
    );
    let pipeline = gst::Pipeline::with_name("bitrate-characterization");
    let chain = gst::parse::bin_from_description(&description, true)
        .with_context(|| format!("could not build {encoder_factory} characterization pipeline"))?;
    pipeline.add(&chain)?;

    let encoder = chain
        .by_name("characterize-encoder")
        .context("characterization pipeline has no encoder")?;
    configure_encoder(&encoder, encoder_factory, Codec::H265, 60);
    let mutable_in_playing = encoder
        .find_property("bitrate")
        .context("encoder has no bitrate property")?
        .flags()
        .contains(gst::PARAM_FLAG_MUTABLE_PLAYING);

    let encoded = Arc::new(FrameProgress::default());
    let receiver = Arc::new(FrameProgress::default());
    let started = Instant::now();
    let encoded_for_probe = encoded.clone();
    let encoded_probe_started = started;
    let _encoded_probe = chain
        .by_name("encoded-output")
        .context("characterization pipeline has no encoded output")?
        .static_pad("src")
        .context("encoded output has no source pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                encoded_for_probe.record(
                    encoded_probe_started,
                    buffer.size(),
                    !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                );
            }
            gst::PadProbeReturn::Ok
        })
        .context("could not attach encoded-frame probe")?;
    let receiver_for_probe = receiver.clone();
    let receiver_probe_started = started;
    let _receiver_probe = chain
        .by_name("receiver-output")
        .context("characterization pipeline has no receiver output")?
        .static_pad("src")
        .context("receiver output has no source pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                receiver_for_probe.record(receiver_probe_started, buffer.size(), false);
            }
            gst::PadProbeReturn::Ok
        })
        .context("could not attach receiver-frame probe")?;

    let bus = pipeline
        .bus()
        .context("characterization pipeline has no bus")?;
    let mut set_state = |state| set_pipeline_state(&pipeline, state);
    crate::start_pipeline_with(&mut set_state)?;

    let run_result = (|| -> Result<Vec<PhaseReport>> {
        observe_until(&bus, Instant::now() + WARMUP_DURATION, &pipeline, &encoder)?;
        if encoded.frames() == 0 || receiver.frames() == 0 {
            bail!("pipeline made no encoded or receiver progress during warmup");
        }
        let mut phases = Vec::with_capacity(4);
        for (index, target) in [
            SELECTED_BITRATE_KBPS,
            SELECTED_BITRATE_KBPS / 2,
            SELECTED_BITRATE_KBPS / 4,
            SELECTED_BITRATE_KBPS,
        ]
        .into_iter()
        .enumerate()
        {
            let phase_started = Instant::now();
            let phase_started_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let deadline = phase_started + PHASE_DURATION;
            let encoded_start = encoded.begin_phase(phase_started_ms);
            let receiver_start = receiver.begin_phase(phase_started_ms);
            let property_set_ms = if index == 0 {
                0
            } else {
                let set_started = Instant::now();
                encoder.set_property("bitrate", target);
                set_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
            };
            let applied = encoder.property::<u32>("bitrate");
            let state_changes = observe_until(&bus, deadline, &pipeline, &encoder)?;
            let ended_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let encoded_measurement = encoded.end_phase(encoded_start, ended_ms);
            let receiver_measurement = receiver.end_phase(receiver_start, ended_ms);
            let duration_ms = phase_started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            let observed_video_kbps = encoded_measurement
                .bytes
                .saturating_mul(8)
                .checked_div(duration_ms.max(1))
                .unwrap_or_default();
            let keyframe_near_mutation =
                index > 0 && keyframe_near_mutation(&encoded_measurement, property_set_ms);
            let continuous = applied == target
                && property_set_ms <= MAX_PROPERTY_SET_MS
                && state_changes.pipeline == 0
                && state_changes.encoder == 0
                && !keyframe_near_mutation
                && phase_pair_is_continuous(&encoded_measurement, &receiver_measurement);
            phases.push(PhaseReport {
                requested_bitrate_kbps: target,
                applied_bitrate_kbps: applied,
                property_set_ms,
                observed_video_kbps,
                encoded: encoded_measurement,
                receiver: receiver_measurement,
                state_changes,
                keyframe_near_mutation,
                continuous,
            });
        }
        Ok(phases)
    })();

    let stop_result = set_state(gst::State::Null);
    let phases = match (run_result, stop_result) {
        (Ok(phases), Ok(())) => phases,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(run_error), Err(stop_error)) => {
            return Err(anyhow::anyhow!(
                "{run_error:#}; cleanup failed: {stop_error:#}"
            ));
        }
    };
    let continuous = phases.iter().all(|phase| phase.continuous);
    let bitrate_response_effective = bitrate_response_is_effective([
        phases[0].observed_video_kbps,
        phases[1].observed_video_kbps,
        phases[2].observed_video_kbps,
        phases[3].observed_video_kbps,
    ]);
    let (safe_for_live_budgeting, verdict) =
        characterization_verdict(mutable_in_playing, continuous, bitrate_response_effective);

    Ok(CharacterizationReport {
        encoder: encoder_factory,
        bitrate_mutable_in_playing: mutable_in_playing,
        resolution: "1920x1080",
        frame_rate: 60,
        phase_duration_ms: PHASE_DURATION.as_millis() as u64,
        phases,
        bitrate_response_effective,
        safe_for_live_budgeting,
        verdict,
    })
}

fn set_pipeline_state(pipeline: &gst::Pipeline, target: gst::State) -> Result<()> {
    pipeline.set_state(target)?;
    let (state_result, current, pending) = pipeline.state(Some(gst::ClockTime::from_seconds(5)));
    state_result?;
    if current != target {
        bail!("pipeline did not reach {target:?} (current: {current:?}, pending: {pending:?})");
    }
    Ok(())
}

fn observe_until(
    bus: &gst::Bus,
    deadline: Instant,
    pipeline: &gst::Pipeline,
    encoder: &gst::Element,
) -> Result<StateChanges> {
    let mut changes = StateChanges::default();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.min(Duration::from_millis(100));
        let Some(message) = bus.timed_pop(gst::ClockTime::from_nseconds(
            timeout.as_nanos().min(u128::from(u64::MAX)) as u64,
        )) else {
            continue;
        };
        inspect_message(&message, pipeline, encoder, &mut changes)?;
    }
    drain_bus_into(bus, pipeline, encoder, &mut changes)?;
    Ok(changes)
}

fn drain_bus_into(
    bus: &gst::Bus,
    pipeline: &gst::Pipeline,
    encoder: &gst::Element,
    changes: &mut StateChanges,
) -> Result<()> {
    while let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) {
        inspect_message(&message, pipeline, encoder, changes)?;
    }
    Ok(())
}

fn inspect_message(
    message: &gst::Message,
    pipeline: &gst::Pipeline,
    encoder: &gst::Element,
    changes: &mut StateChanges,
) -> Result<()> {
    match message.view() {
        gst::MessageView::Error(error) => {
            bail!("{} ({})", error.error(), error.debug().unwrap_or_default())
        }
        gst::MessageView::Eos(_) => bail!("pipeline reached EOS during characterization"),
        gst::MessageView::StateChanged(_) => {
            if let Some(source) = message.src() {
                if source.name() == pipeline.name() {
                    changes.pipeline += 1;
                } else if source.name() == encoder.name() {
                    changes.encoder += 1;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn phase_measurement_reports_progress_silence_and_keyframe_bursts() {
        let progress = FrameProgress::default();
        progress.record_at(100, 1_000, true);
        let phase = progress.begin_phase(110);

        progress.record_at(120, 2_000, false);
        progress.record_at(140, 3_000, true);
        progress.record_at(300, 4_000, true);
        let report = progress.end_phase(phase, 1_000);

        assert_eq!(report.frames, 3);
        assert_eq!(report.bytes, 9_000);
        assert_eq!(report.keyframes, 2);
        assert_eq!(report.keyframe_bursts, 2);
        assert_eq!(report.first_keyframe_after_phase_start_ms, Some(30));
        assert_eq!(report.max_gap_ms, 160);
        assert_eq!(report.silent_ms, 700);
    }

    #[test]
    fn continuity_gate_rejects_stalls_missing_frames_and_keyframe_bursts() {
        let healthy = PhaseMeasurement {
            frames: 240,
            bytes: 1_000_000,
            keyframes: 2,
            keyframe_bursts: 0,
            first_keyframe_after_phase_start_ms: Some(1_000),
            max_gap_ms: 20,
            silent_ms: 10,
        };
        assert!(phase_is_continuous(&healthy));

        for unhealthy in [
            PhaseMeasurement {
                frames: 0,
                ..healthy
            },
            PhaseMeasurement {
                max_gap_ms: 1_001,
                ..healthy
            },
            PhaseMeasurement {
                silent_ms: 1_001,
                ..healthy
            },
            PhaseMeasurement {
                keyframe_bursts: 1,
                ..healthy
            },
        ] {
            assert!(!phase_is_continuous(&unhealthy));
        }

        let lagging_receiver = PhaseMeasurement {
            frames: 120,
            ..healthy
        };
        assert!(!phase_pair_is_continuous(&healthy, &lagging_receiver));

        let short_restart = PhaseMeasurement {
            max_gap_ms: 300,
            ..healthy
        };
        assert!(!phase_is_continuous(&short_restart));
    }

    #[test]
    fn bitrate_response_gate_requires_both_decreases_and_the_rebound() {
        assert!(bitrate_response_is_effective([
            23_814, 10_906, 7_235, 22_784
        ]));
        assert!(!bitrate_response_is_effective([
            23_814, 23_814, 23_814, 23_814
        ]));
        assert!(!bitrate_response_is_effective([
            23_814, 10_906, 7_235, 10_000
        ]));
        assert!(!bitrate_response_is_effective([
            22_800, 7_900, 7_900, 228_000
        ]));
        assert!(!bitrate_response_is_effective([
            228_000, 114_000, 57_000, 228_000
        ]));
        assert!(!bitrate_response_is_effective([0, 0, 0, 0]));
    }

    #[test]
    fn keyframe_window_includes_time_spent_setting_the_property() {
        let measurement = PhaseMeasurement {
            frames: 210,
            bytes: 1_000_000,
            keyframes: 1,
            keyframe_bursts: 0,
            first_keyframe_after_phase_start_ms: Some(300),
            max_gap_ms: 20,
            silent_ms: 10,
        };

        assert!(keyframe_near_mutation(&measurement, 100));
        assert!(!keyframe_near_mutation(&measurement, 49));
    }

    #[test]
    fn missing_playing_mutability_always_fails_the_safety_verdict() {
        let (safe, verdict) = characterization_verdict(false, true, true);

        assert!(!safe);
        assert_eq!(verdict, "bitrate is not advertised mutable in PLAYING");
    }

    #[test]
    fn worker_timeout_has_an_exact_upper_bound() {
        assert!(!worker_timed_out(WORKER_TIMEOUT - Duration::from_millis(1)));
        assert!(worker_timed_out(WORKER_TIMEOUT));
    }

    #[test]
    fn worker_termination_reaps_a_blocked_child_within_the_kill_bound() {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();

        terminate_worker(&mut child, "test-encoder").unwrap();

        assert!(started.elapsed() < WORKER_KILL_TIMEOUT);
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn phase_measurement_detects_a_keyframe_burst_across_the_mutation_boundary() {
        let progress = FrameProgress::default();
        progress.record_at(900, 1_000, true);
        let phase = progress.begin_phase(1_000);

        progress.record_at(1_050, 1_000, true);
        let report = progress.end_phase(phase, 1_100);

        assert_eq!(report.keyframes, 1);
        assert_eq!(report.keyframe_bursts, 1);
        assert_eq!(report.first_keyframe_after_phase_start_ms, Some(50));
    }
}
