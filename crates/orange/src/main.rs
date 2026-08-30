//! orange - low-overhead window streaming for friends.

mod auth;
mod media_diagnostics;
mod overlay;
mod peer;
mod pipeline;
mod targets;
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
use std::time::{Duration, Instant};

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
        /// overlay's coordinate space, so the bar's proportions follow it.
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
    /// Kilobits per second.
    #[arg(long, default_value_t = 25_000)]
    bitrate: u32,
    /// Frames per second. Defaults to the captured window's display refresh.
    #[arg(long)]
    fps: Option<u32>,
    /// Downscale on the GPU, e.g. 1920x1080.
    #[arg(long)]
    scale: Option<String>,
}

impl QualityArgs {
    fn settings(&self, hwnd: isize) -> Result<CaptureSettings> {
        let fps = self
            .fps
            .or_else(|| window::target_refresh_rate(hwnd))
            .unwrap_or(60);
        if fps == 0 {
            anyhow::bail!("fps must be greater than zero");
        }
        Ok(CaptureSettings {
            hwnd,
            codec: Codec::parse(&self.codec)?,
            bitrate: self.bitrate,
            fps,
            scale: self.scale.as_deref().map(parse_scale).transpose()?,
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

fn parse_scale(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .context("scale must look like 1920x1080")?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
            quality,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Hosting hwnd {hwnd} as {:?} at {} kbps / {} fps",
                settings.codec, settings.bitrate, settings.fps
            );
            runtime()?.block_on(peer::run_host(&settings, &server))
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
                    let playback = window::PlaybackWindow::spawn(
                        &format!("orange - {code}"),
                        playback_profile,
                    )?;
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

            let playback =
                window::PlaybackWindow::spawn_preview("orange - preview", ww as i32, wh as i32)?;
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
            run_until_closed(&pipeline, playback.hwnd())
        }
    }
}

/// A stand-in for the decoded stream, feeding the real overlay and sink.
///
/// The tail of this pipeline is deliberately identical to the one
/// `webrtc::build_receive_branch` assembles, so the harness cannot flatter the
/// design in ways the real viewer will not reproduce.
fn build_preview_pipeline(
    size: (u32, u32),
    fps: u32,
    pattern: &str,
    image: Option<&str>,
    playback: &window::PlaybackWindow,
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
    overlay::attach(&composition, playback);

    let sink = gst::ElementFactory::make("d3d11videosink")
        // Unlike the real viewer, sync to the clock: there is no live source
        // to pace this pipeline, and without it the sink spins a core.
        .property("sync", true)
        .property("force-aspect-ratio", true)
        .build()?;
    let overlay_iface = sink
        .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
        .context("d3d11videosink does not implement GstVideoOverlay")?;
    // SAFETY: `hwnd` is our own window, alive for as long as this runs.
    unsafe { overlay_iface.set_window_handle(playback.hwnd() as usize) };

    pipeline.add_many([source.upcast_ref(), &composition, &sink])?;
    gst::Element::link_many([source.upcast_ref(), &composition, &sink])?;
    Ok(pipeline)
}

/// Run until the viewer window goes away, rather than for a fixed duration.
fn run_until_closed(pipeline: &gst::Pipeline, hwnd: isize) -> Result<()> {
    pipeline.set_state(gst::State::Playing)?;
    window::reveal(hwnd);

    let bus = pipeline.bus().expect("pipeline without bus");
    let mut error = None;

    while window::is_alive(hwnd) {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
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

    pipeline.set_state(gst::State::Null)?;
    match error {
        Some(err) => Err(err),
        None => Ok(()),
    }
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
    println!(
        "{:<12} {:>11}  {:<24} {}",
        "HWND", "SIZE", "PROCESS", "TITLE"
    );
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
    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("pipeline without bus");
    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let mut error = None;

    while started.elapsed() < deadline {
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
    pipeline.set_state(gst::State::Null)?;

    match error {
        Some(err) => Err(err),
        None => Ok(()),
    }
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
