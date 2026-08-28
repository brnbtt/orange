//! orange - low-overhead window streaming for friends.

mod overlay;
mod peer;
mod window;
mod pipeline;
mod targets;
mod webrtc;

use orange_signal as signal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gst::prelude::*;
use gstreamer as gst;
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
    List,
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
        #[arg(long, default_value = "ws://127.0.0.1:9000")]
        server: String,
        #[command(flatten)]
        quality: QualityArgs,
    },
    /// Watch a shared window by code.
    Watch {
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "ws://127.0.0.1:9000")]
        server: String,
        /// Write to a file instead of rendering. For headless verification.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(clap::Args)]
struct QualityArgs {
    /// Disable audio capture.
    #[arg(long)]
    no_audio: bool,
    #[arg(long, default_value = "av1")]
    codec: String,
    /// Kilobits per second.
    #[arg(long, default_value_t = 25_000)]
    bitrate: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Downscale on the GPU, e.g. 1920x1080.
    #[arg(long)]
    scale: Option<String>,
}

impl QualityArgs {
    fn settings(&self, hwnd: isize) -> Result<CaptureSettings> {
        Ok(CaptureSettings {
            hwnd,
            codec: Codec::parse(&self.codec)?,
            bitrate: self.bitrate,
            fps: self.fps,
            scale: self.scale.as_deref().map(parse_scale).transpose()?,
            // Scope audio to the captured window's process, so voice chat and
            // music stay out of the stream.
            audio_pid: if self.no_audio { None } else { targets::pid_for_hwnd(hwnd) },
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
    gst::init()?;

    match cli.command {
        Command::List => cmd_list(),
        Command::Record {
            hwnd,
            out,
            quality,
            seconds,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Recording hwnd {hwnd} as {:?} at {} kbps -> {out}",
                settings.codec, settings.bitrate
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
                "Loopback hwnd {hwnd} as {:?} at {} kbps",
                settings.codec, settings.bitrate
            );
            let output = if show {
                let overlay = std::sync::Arc::new(std::sync::Mutex::new(overlay::OverlayState::default()));
                let win = window::spawn("orange - loopback", 1280, 720, overlay.clone())?;
                webrtc::Output::Window { hwnd: win.hwnd, overlay }
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
        Command::Host {
            hwnd,
            server,
            quality,
        } => {
            let settings = quality.settings(hwnd)?;
            println!(
                "Hosting hwnd {hwnd} as {:?} at {} kbps",
                settings.codec, settings.bitrate
            );
            runtime()?.block_on(peer::run_host(&settings, &server))
        }
        Command::Watch { code, server, out } => {
            let output = match out {
                Some(path) => webrtc::Output::File(path),
                None => {
                    let overlay = std::sync::Arc::new(std::sync::Mutex::new(overlay::OverlayState::default()));
                    let win = window::spawn(&format!("orange - {code}"), 1280, 720, overlay.clone())?;
                    webrtc::Output::Window { hwnd: win.hwnd, overlay }
                }
            };
            runtime()?.block_on(peer::run_watch(&code, &server, output))
        }
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

fn cmd_list() -> Result<()> {
    let windows = targets::list_windows()?;
    if windows.is_empty() {
        println!("No capturable windows found.");
        return Ok(());
    }
    println!("{:<12} {:>11}  {:<24} {}", "HWND", "SIZE", "PROCESS", "TITLE");
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
