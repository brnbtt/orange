//! orange - low-overhead window streaming for friends.

mod auth;
mod connection;
mod encoder_characterization;
mod media_diagnostics;
mod overlay;
mod peer;
mod pipeline;
mod targets;
#[cfg(test)]
mod test_support;
mod text;
mod webrtc;
mod window;

use orange_signal as signal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video::prelude::VideoOverlayExtManual;
use pipeline::{CaptureSettings, Codec};
use std::io::{self, Write};
use std::time::{Duration, Instant};

const fn watch_window_title() -> &'static str {
    "orange - viewer"
}

#[derive(Parser)]
#[command(name = "orange", about = "Low-overhead window streaming for friends")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List windows that can be captured.
    List {
        /// Emit JSON, for the tray UI to consume.
        #[arg(long)]
        json: bool,
    },
    /// Record a window to a file. Diagnostic for the capture path.
    Record {
        #[arg(long)]
        hwnd: isize,
        #[arg(long, default_value = "orange-capture.mkv")]
        out: String,
        #[command(flatten)]
        quality: QualityArgs,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Send a window over WebRTC and receive it back in the same process.
    /// Diagnostic for the transport path, with no signalling involved.
    Loopback {
        #[arg(long)]
        hwnd: isize,
        /// Render the received stream in a window instead of writing a file.
        #[arg(long)]
        show: bool,
        #[arg(long, default_value = "orange-loopback.mkv")]
        out: String,
        #[command(flatten)]
        quality: QualityArgs,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Characterize H.265 bitrate changes while the encoder is PLAYING.
    CharacterizeBitrate {
        /// Test one encoder; omitted tests each available H.265 encoder independently.
        #[arg(long)]
        encoder: Option<String>,
    },
    #[command(hide = true)]
    CharacterizeBitrateWorker {
        #[arg(long)]
        encoder: String,
    },
    /// Run the signalling relay. Carries handshakes only, never video.
    Serve {
        #[arg(long, default_value = "0.0.0.0:9000")]
        addr: String,
    },
    /// Share a window. Prints a code for viewers to join with.
    Host {
        #[arg(long)]
        hwnd: isize,
        #[arg(long, default_value = "ws://127.0.0.1:9000/ws", env = "ORANGE_SERVER")]
        server: String,
        /// Discord ids allowed to see this stream in their friends list and be
        /// handed the code without being told it. The roster lives with the
        /// tray, which owns preferences; this process is only the messenger.
        #[arg(long, value_delimiter = ',')]
        visible_to: Vec<String>,
        #[command(flatten)]
        quality: QualityArgs,
    },
    /// Sign in with Discord, so viewers and hosts see names instead of ids.
    Login {
        #[arg(long, default_value = "ws://127.0.0.1:9000/ws", env = "ORANGE_SERVER")]
        server: String,
    },
    /// Forget the stored Discord session.
    Logout,
    /// Watch a shared window by code.
    Watch {
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "ws://127.0.0.1:9000/ws", env = "ORANGE_SERVER")]
        server: String,
        /// Write to a file instead of rendering. For headless verification.
        #[arg(long)]
        out: Option<String>,
        /// Offset this viewer from earlier viewer windows.
        #[arg(long, default_value_t = 0)]
        cascade: u32,
        /// Playback behavior for an ordinary viewer or the local live monitor.
        #[arg(long, value_enum, default_value_t = PlaybackKind::Friend)]
        profile: PlaybackKind,
    },
    /// Open the viewer window with a synthetic stream. Design harness.
    ///
    /// Same window, same overlay, same sink as `watch`, but with no capture,
    /// no encode and no network - so the controls can be iterated on in
    /// seconds rather than by waiting out a full loopback.
    Preview {
        /// Stream resolution to design against, e.g. 3840x2160. This is the
        /// overlay's coordinate space, so the controls scale with it.
        #[arg(long, default_value = "1920x1080")]
        size: String,
        /// The viewer window's size on screen.
        #[arg(long, default_value = "1280x720")]
        window: String,
        /// videotestsrc pattern: smpte, ball, snow, black, white, gradient.
        #[arg(long, default_value = "smpte")]
        pattern: String,
        /// Use a still image as the backdrop instead, so the controls can be
        /// judged over real content rather than colour bars.
        #[arg(long)]
        image: Option<String>,
        /// Synthetic preview rate. Real capture follows the source display.
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Hold the controls open instead of fading after pointer activity stops.
        #[arg(long)]
        pin: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PlaybackKind {
    Friend,
    Monitor,
}

#[derive(clap::Args)]
struct QualityArgs {
    /// Disable audio capture.
    #[arg(long)]
    no_audio: bool,
    #[arg(long, default_value = "h265")]
    codec: String,
    /// Override the resolution/FPS-derived video bitrate, in kilobits per second.
    #[arg(long)]
    bitrate: Option<u32>,
    /// Frames per second, capped at 120. Defaults to the captured window's
    /// display refresh, capped the same way.
    #[arg(long)]
    fps: Option<u32>,
    /// Downscale on the GPU, e.g. 1920x1080.
    #[arg(long)]
    scale: Option<String>,
}

impl QualityArgs {
    fn settings(&self, hwnd: isize) -> Result<CaptureSettings> {
        // Capped before the bitrate is derived from it, so an uncapped display
        // refresh cannot inflate the bitrate for frames that never get encoded.
        let fps = capture_fps(self.fps.or_else(|| window::target_refresh_rate(hwnd)))?;
        let requested = if self.codec.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(Codec::parse(&self.codec)?)
        };
        let (codec, encoder) = pipeline::select_encoder(requested)?;
        if requested.is_none() && encoder != "mfh265enc" {
            println!("Encoder auto-selected {codec:?} via {encoder} zero-copy fallback.");
        }
        let scale = self.scale.as_deref().map(parse_scale).transpose()?;
        let (width, height) = scale
            .or_else(|| targets::capture_dimensions(hwnd))
            .unwrap_or((1920, 1080));
        let (recommended_bitrate, constrained) =
            pipeline::recommended_video_bitrate(width, height, fps);
        let bitrate = self.bitrate.unwrap_or(recommended_bitrate);
        if self.bitrate.is_none() {
            println!(
                "Video bitrate set automatically to {bitrate} kbps for {width}x{height} at {fps} fps."
            );
            if constrained {
                let message = format!(
                    "Automatic quality is limited to {} Mbps at {width}x{height} and {fps} fps",
                    bitrate / 1_000
                );
                println!(
                    "[quality-status] {}",
                    serde_json::json!({
                        "event": "automatic-bitrate-constrained",
                        "message": message,
                    })
                );
                eprintln!(
                    "[quality] automatic video bitrate capped at {bitrate} kbps; {width}x{height} at {fps} fps is quality-constrained"
                );
            }
        }
        Ok(CaptureSettings {
            hwnd,
            codec,
            encoder,
            bitrate,
            fps,
            scale,
            // Scope audio to the captured window's process, so voice chat and
            // music stay out of the stream. Whole-screen sharing captures
            // everything, which is the expected behaviour there.
            audio_pid: if self.no_audio {
                None
            } else if hwnd == 0 {
                Some(0)
            } else {
                targets::pid_for_hwnd(hwnd)
            },
        })
    }
}

/// Resolve the capture rate. `None` means nothing asked for one and the display
/// did not report a usable refresh rate.
///
/// The cap applies to a display's refresh rate as much as to an explicit
/// `--fps`, because the display is where the high rates came from.
fn capture_fps(requested: Option<u32>) -> Result<u32> {
    let fps = requested.unwrap_or(60);
    if fps == 0 {
        anyhow::bail!("fps must be greater than zero");
    }
    Ok(fps.min(pipeline::MAX_FPS))
}

fn parse_scale(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .context("scale must look like 1920x1080")?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn format_error_chain(error: &anyhow::Error) -> String {
    format!("Error: {error:#}").replace(['\r', '\n'], " ")
}

fn write_error_chain(error: &anyhow::Error) -> io::Result<()> {
    writeln!(io::stderr().lock(), "{}", format_error_chain(error))
}

fn main() {
    if let Err(error) = entry() {
        let _ = write_error_chain(&error);
        std::process::exit(1);
    }
}

fn entry() -> Result<()> {
    // Set before GStreamer or our native window code can present any UI.
    if let Err(error) = window::set_taskbar_identity() {
        eprintln!("[window] could not set taskbar identity: {error}");
    }
    let cli = Cli::parse();
    let _diagnostics = media_diagnostics::DiagnosticWriter::new();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    // Before anything creates a window or asks Windows about the screen.
    window::set_dpi_aware();
    gst::init()?;

    match cli.command {
        Command::List { json } => cmd_list(json),
        Command::Record {
            hwnd,
            out,
            quality,
            seconds,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Recording hwnd {hwnd} as {:?} at {} kbps / {} fps -> {out}",
                settings.codec, settings.bitrate, settings.fps
            );
            let pipeline = pipeline::build_record_pipeline(&settings, &out)?;
            run_pipeline(&pipeline, seconds)?;
            report_file(&out)
        }
        Command::Loopback {
            hwnd,
            show,
            out,
            quality,
            seconds,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Loopback hwnd {hwnd} as {:?} at {} kbps / {} fps",
                settings.codec, settings.bitrate, settings.fps
            );
            let output = if show {
                let playback = window::PlaybackWindow::spawn(
                    "orange - loopback",
                    window::PlaybackProfile::FriendViewer { cascade: 0 },
                )?;
                webrtc::Output::Window(playback)
            } else {
                webrtc::Output::File(out.clone())
            };
            webrtc::run_loopback(&settings, output, seconds)?;
            if show {
                Ok(())
            } else {
                report_file(&out)
            }
        }
        Command::CharacterizeBitrate { encoder } => {
            encoder_characterization::run(encoder.as_deref())
        }
        Command::CharacterizeBitrateWorker { encoder } => {
            encoder_characterization::run_worker(&encoder)
        }
        Command::Serve { addr } => runtime()?.block_on(signal::serve(&addr)),
        Command::Login { server } => runtime()?.block_on(async {
            let session = auth::login(&server).await?;
            println!("Signed in as {}", session.name);
            Ok(())
        }),
        Command::Logout => {
            auth::clear_session()?;
            println!("Signed out.");
            Ok(())
        }
        Command::Host {
            hwnd,
            server,
            visible_to,
            quality,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Hosting hwnd {hwnd} as {:?} at {} kbps / {} fps",
                settings.codec, settings.bitrate, settings.fps
            );
            runtime()?.block_on(peer::run_host(&settings, &server, visible_to))
        }
        Command::Watch {
            code,
            server,
            out,
            cascade,
            profile,
        } => {
            let output = match out {
                Some(path) => webrtc::Output::File(path),
                None => {
                    let playback_profile = match profile {
                        PlaybackKind::Friend => window::PlaybackProfile::FriendViewer { cascade },
                        PlaybackKind::Monitor => window::PlaybackProfile::LiveMonitor,
                    };
                    let playback =
                        window::PlaybackWindow::spawn(watch_window_title(), playback_profile)?;
                    webrtc::Output::Window(playback)
                }
            };
            runtime()?.block_on(peer::run_watch(&code, &server, output))
        }
        Command::Preview {
            size,
            window: window_size,
            pattern,
            image,
            fps,
            pin,
        } => {
            let (vw, vh) = parse_scale(&size)?;
            let (ww, wh) = parse_scale(&window_size)?;
            if fps == 0 {
                anyhow::bail!("fps must be greater than zero");
            }

            // Declared before the pipeline so its Drop runs only after the
            // sink and callbacks are stopped and released.
            let playback_owner =
                window::PlaybackWindow::spawn_preview("orange - preview", ww as i32, wh as i32)?;
            let playback = playback_owner.handle();
            {
                let mut state = playback.overlay().lock().unwrap();
                state.pinned = pin;
                // Stand-in values so the status cluster has something to lay
                // out. The real viewer fills these from the stream.
                state.host = Some(String::from("brnbtt"));
                state.bitrate_kbps = Some(29_000);
                state.viewers = Some(3);
            }

            println!("Preview: {vw}x{vh} at {fps} fps in a {ww}x{wh} window.");
            println!(
                "Move the mouse to wake the controls{}. Esc or the X closes.",
                if pin { " (pinned open)" } else { "" }
            );

            let pipeline =
                build_preview_pipeline((vw, vh), fps, &pattern, image.as_deref(), &playback)?;
            run_until_closed(&pipeline, &playback)
        }
    }
}

/// A stand-in for the decoded stream, feeding the real overlay and sink.
///
/// After its synthetic raw source, this uses the viewer's real overlay element
/// and D3D11 sink. It deliberately bypasses RTP, parsing, and decoding.
fn build_preview_pipeline(
    size: (u32, u32),
    fps: u32,
    pattern: &str,
    image: Option<&str>,
    playback: &window::PlaybackWindowHandle,
) -> Result<gst::Pipeline> {
    let (w, h) = size;

    let source = match image {
        // GStreamer's parser treats backslashes as escapes, so a Windows path
        // arrives mangled. Forward slashes survive and Windows accepts them.
        Some(path) => format!(
            "filesrc location=\"{}\" ! decodebin ! imagefreeze",
            path.replace('\\', "/")
        ),
        None => format!("videotestsrc pattern={pattern}"),
    };
    let description = format!(
        "{source} ! videoconvert ! videoscale \
         ! video/x-raw,width={w},height={h},framerate={fps}/1,pixel-aspect-ratio=1/1 \
         ! d3d11upload"
    );

    let pipeline = gst::Pipeline::new();
    let source = gst::parse::bin_from_description(&description, true)
        .with_context(|| format!("failed to build the preview source: {description}"))?;

    let composition = gst::ElementFactory::make("overlaycomposition")
        .build()
        .context("overlaycomposition missing")?;
    overlay::attach(&composition, playback)?;

    let sink = gst::ElementFactory::make("d3d11videosink")
        // Unlike the real viewer, sync to the clock: there is no live source
        // to pace this pipeline, and without it the sink spins a core.
        .property("sync", true)
        .property("force-aspect-ratio", true)
        .build()?;
    let overlay_iface = sink
        .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
        .context("d3d11videosink does not implement GstVideoOverlay")?;
    let hwnd = playback.hwnd().context("playback window is unavailable")?;
    // SAFETY: this helper is called only with the preview session's passive
    // handle. Its unique owner is declared before the returned pipeline, and
    // run_until_closed sets that pipeline to Null before the owner can drop.
    unsafe { overlay_iface.set_window_handle(hwnd as usize) };

    pipeline.add_many([source.upcast_ref(), &composition, &sink])?;
    gst::Element::link_many([source.upcast_ref(), &composition, &sink])?;
    Ok(pipeline)
}

/// Run until the viewer window goes away, rather than for a fixed duration.
fn run_until_closed(
    pipeline: &gst::Pipeline,
    playback: &window::PlaybackWindowHandle,
) -> Result<()> {
    let mut set_state = |state| {
        pipeline
            .set_state(state)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    };
    start_pipeline_with(&mut set_state)?;
    playback.reveal();

    let bus = pipeline.bus().expect("pipeline without bus");
    let mut error = None;

    while playback.is_alive() {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => {
                if playback.is_alive() {
                    error = Some(anyhow::anyhow!(
                        "{} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    ));
                }
                break;
            }
            gst::MessageView::Eos(_) => break,
            _ => {}
        }
    }

    let run_result = match error {
        Some(err) => Err(err),
        None => Ok(()),
    };
    stop_pipeline_with(run_result, &mut set_state)
}

/// Signalling is async; GStreamer is not. One runtime, created only when a
/// networked subcommand needs it.
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start async runtime")
}

fn cmd_list(json: bool) -> Result<()> {
    let windows = targets::list_windows()?;

    if json {
        // The tray consumes this, so keep it stable.
        let items: Vec<_> = windows
            .iter()
            .map(|t| {
                serde_json::json!({
                    "hwnd": t.hwnd,
                    "pid": t.pid,
                    "title": t.title,
                    "process": t.process,
                    "width": t.width,
                    "height": t.height,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items)?);
        return Ok(());
    }

    if windows.is_empty() {
        println!("No capturable windows found.");
        return Ok(());
    }
    println!("{:<12} {:>11}  {:<24} TITLE", "HWND", "SIZE", "PROCESS");
    for t in windows {
        let size = format!("{}x{}", t.width, t.height);
        let title: String = t.title.chars().take(48).collect();
        println!("{:<12} {:>11}  {:<24} {}", t.hwnd, size, t.process, title);
    }
    Ok(())
}

/// Run a pipeline for a fixed duration, then shut it down cleanly.
///
/// The bus is polled rather than waited on indefinitely: Windows Graphics
/// Capture produces no frames for an idle window, so a blocking wait would
/// hang with no explanation.
pub fn run_pipeline(pipeline: &gst::Pipeline, seconds: u64) -> Result<()> {
    run_pipeline_while(pipeline, seconds, None)
}

fn timed_pipeline_should_continue(before_deadline: bool, playback_alive: Option<bool>) -> bool {
    before_deadline && playback_alive != Some(false)
}

pub(crate) fn run_pipeline_while(
    pipeline: &gst::Pipeline,
    seconds: u64,
    playback: Option<&window::PlaybackWindowHandle>,
) -> Result<()> {
    run_pipeline_while_with_shutdown(pipeline, seconds, playback, || Ok(()))
}

fn combine_pipeline_results(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(anyhow::anyhow!("{primary:#}; {cleanup:#}")),
    }
}

fn start_pipeline_with(set_state: &mut impl FnMut(gst::State) -> Result<()>) -> Result<()> {
    match set_state(gst::State::Playing) {
        Ok(()) => Ok(()),
        Err(error) => stop_pipeline_with(Err(error), set_state),
    }
}

fn stop_pipeline_with(
    primary: Result<()>,
    set_state: &mut impl FnMut(gst::State) -> Result<()>,
) -> Result<()> {
    combine_pipeline_results(primary, set_state(gst::State::Null))
}

pub(crate) fn run_pipeline_while_with_shutdown(
    pipeline: &gst::Pipeline,
    seconds: u64,
    playback: Option<&window::PlaybackWindowHandle>,
    shutdown: impl FnOnce() -> Result<()>,
) -> Result<()> {
    run_pipeline_while_with_shutdown_and_state(pipeline, seconds, playback, shutdown, |state| {
        pipeline
            .set_state(state)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    })
}

fn run_pipeline_while_with_shutdown_and_state(
    pipeline: &gst::Pipeline,
    seconds: u64,
    playback: Option<&window::PlaybackWindowHandle>,
    shutdown: impl FnOnce() -> Result<()>,
    mut set_state: impl FnMut(gst::State) -> Result<()>,
) -> Result<()> {
    if let Err(error) = set_state(gst::State::Playing) {
        let shutdown_result = shutdown();
        return stop_pipeline_with(
            combine_pipeline_results(Err(error), shutdown_result),
            &mut set_state,
        );
    }

    let bus = pipeline.bus().expect("pipeline without bus");
    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let mut error = None;

    while timed_pipeline_should_continue(
        started.elapsed() < deadline,
        playback.map(window::PlaybackWindowHandle::is_alive),
    ) {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => {
                error = Some(anyhow::anyhow!(
                    "{} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                ));
                break;
            }
            gst::MessageView::Eos(_) => break,
            _ => {}
        }
    }

    // Clean EOS so muxers write their headers; without it the file is unplayable.
    pipeline.send_event(gst::event::Eos::new());
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let shutdown_result = shutdown();
    let run_result = match error {
        Some(err) => Err(err),
        None => Ok(()),
    };
    stop_pipeline_with(
        combine_pipeline_results(run_result, shutdown_result),
        &mut set_state,
    )
}

fn report_file(path: &str) -> Result<()> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        anyhow::bail!(
            "captured nothing ({path} is empty or missing). Windows Graphics Capture \
             only produces frames when the window redraws - is it minimised or idle?"
        );
    }
    println!("Wrote {path} ({:.1} MB)", size as f64 / 1_048_576.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        capture_fps, format_error_chain, gst, pipeline, run_pipeline_while,
        run_pipeline_while_with_shutdown, run_pipeline_while_with_shutdown_and_state,
        start_pipeline_with, stop_pipeline_with, timed_pipeline_should_continue,
        watch_window_title, Cli, Command, QualityArgs,
    };
    use clap::Parser;
    use gst::prelude::*;
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CLOSE,
    };

    fn host_quality(args: &[&str]) -> QualityArgs {
        match Cli::try_parse_from(args).unwrap().command {
            Command::Host { quality, .. } => quality,
            _ => panic!("expected host command"),
        }
    }

    #[test]
    fn video_bitrate_override_is_optional_and_remains_available_to_the_cli() {
        let automatic = host_quality(&["orange", "host", "--hwnd", "1"]);
        let overridden = host_quality(&["orange", "host", "--hwnd", "1", "--bitrate", "100001"]);

        assert_eq!(automatic.bitrate, None);
        assert_eq!(overridden.bitrate, Some(100_001));
    }

    /// The tray joins the roster with commas and omits the flag entirely for an
    /// empty one, because clap reads `--visible-to` with no value as the start
    /// of the next flag. Both halves of that contract are pinned here.
    #[test]
    fn the_friend_roster_arrives_as_one_comma_separated_flag_or_not_at_all() {
        let visible_to = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Host { visible_to, .. } => visible_to,
            _ => panic!("expected host command"),
        };

        assert!(visible_to(&["orange", "host", "--hwnd", "1"]).is_empty());
        assert_eq!(
            visible_to(&["orange", "host", "--hwnd", "1", "--visible-to", "123,456"]),
            vec!["123".to_string(), "456".to_string()]
        );
    }

    #[test]
    fn a_high_refresh_display_is_capped_rather_than_followed() {
        // A measured 180 Hz host asked for 180 fps and delivered 53.6 to its
        // viewer with zero packets lost, so the rate the display reports is not
        // a rate worth honouring.
        assert_eq!(capture_fps(Some(180)).unwrap(), pipeline::MAX_FPS);
        assert_eq!(capture_fps(Some(240)).unwrap(), pipeline::MAX_FPS);
        assert_eq!(capture_fps(Some(120)).unwrap(), 120);
        assert_eq!(capture_fps(Some(60)).unwrap(), 60);
        // No display refresh and no flag still has to produce a rate.
        assert_eq!(capture_fps(None).unwrap(), 60);
        assert!(capture_fps(Some(0)).is_err());
    }

    #[test]
    fn viewer_window_title_never_contains_the_room_code() {
        let code = "SENSITIVE-ROOM-CODE";
        let title = watch_window_title();

        assert_eq!(title, "orange - viewer");
        assert!(!title.contains(code));
    }

    #[test]
    fn formatted_error_includes_outer_and_inner_causes_on_one_line() {
        let error = anyhow::anyhow!("inner cause").context("outer context");

        let message = format_error_chain(&error);

        assert!(message.starts_with("Error: outer context"));
        assert!(message.contains("inner cause"));
        assert_eq!(message.lines().count(), 1);
    }

    #[test]
    fn timed_pipeline_runs_only_before_deadline_and_while_playback_is_open() {
        assert!(timed_pipeline_should_continue(true, None));
        assert!(timed_pipeline_should_continue(true, Some(true)));
        assert!(!timed_pipeline_should_continue(true, Some(false)));
        assert!(!timed_pipeline_should_continue(false, None));
        assert!(!timed_pipeline_should_continue(false, Some(true)));
    }

    #[test]
    fn window_pipeline_playing_failure_attempts_null_and_combines_errors() {
        let mut states = Vec::new();

        let result = start_pipeline_with(&mut |state| {
            states.push(state);
            match state {
                gst::State::Playing => Err(anyhow::anyhow!("playing failed")),
                gst::State::Null => Err(anyhow::anyhow!("null failed")),
                _ => unreachable!(),
            }
        });

        assert_eq!(states, [gst::State::Playing, gst::State::Null]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("playing failed"));
        assert!(message.contains("null failed"));
    }

    #[test]
    fn window_pipeline_later_error_combines_with_null_failure() {
        let mut states = Vec::new();

        let result = stop_pipeline_with(Err(anyhow::anyhow!("playback failed")), &mut |state| {
            states.push(state);
            Err(anyhow::anyhow!("null failed"))
        });

        assert_eq!(states, [gst::State::Null]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("playback failed"));
        assert!(message.contains("null failed"));
    }

    #[test]
    fn timed_pipeline_playing_failure_shuts_down_workers_before_null() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let shutdown_order = order.clone();
        let state_order = order.clone();

        let result = run_pipeline_while_with_shutdown_and_state(
            &pipeline,
            0,
            None,
            move || {
                shutdown_order.lock().unwrap().push("shutdown");
                Err(anyhow::anyhow!("worker cleanup failed"))
            },
            move |state| {
                state_order.lock().unwrap().push(match state {
                    gst::State::Playing => "playing",
                    gst::State::Null => "null",
                    _ => unreachable!(),
                });
                Err(anyhow::anyhow!(match state {
                    gst::State::Playing => "playing failed",
                    gst::State::Null => "null failed",
                    _ => unreachable!(),
                }))
            },
        );

        assert_eq!(*order.lock().unwrap(), ["playing", "shutdown", "null"]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("playing failed"));
        assert!(message.contains("worker cleanup failed"));
        assert!(message.contains("null failed"));
    }

    #[test]
    fn remaining_peer_review_pipeline_reaches_null_after_cleanup_error() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();

        let result = run_pipeline_while_with_shutdown(&pipeline, 0, None, || {
            Err(anyhow::anyhow!("worker cleanup failed"))
        });

        assert_eq!(pipeline.current_state(), gst::State::Null);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("worker cleanup failed"));
    }

    #[test]
    fn closed_playback_pipeline_reaches_null_before_owner_drop() {
        gst::init().unwrap();
        let owner = crate::window::PlaybackWindow::spawn(
            "orange pipeline cleanup test",
            crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
        )
        .unwrap();
        let playback = owner.handle();
        let hwnd = playback.hwnd().unwrap();
        // SAFETY: the owner remains in this scope, and WM_CLOSE only hides the
        // window while reserving the HWND for pipeline teardown.
        let _ = unsafe {
            SendMessageTimeoutW(
                HWND(hwnd as *mut _),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
        };
        assert!(!playback.is_alive());
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("fakesrc")
            .property("num-buffers", 1i32)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add_many([&source, &sink]).unwrap();
        source.link(&sink).unwrap();

        run_pipeline_while(&pipeline, 30, Some(&playback)).unwrap();

        assert_eq!(pipeline.current_state(), gst::State::Null);
        drop(owner);
    }
}
