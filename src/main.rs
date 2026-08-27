//! orange - low-overhead window streaming for friends.
//!
//! Milestone 1: prove the Rust/GStreamer integration by listing capturable
//! windows and recording one to a file, using the same GPU-resident pipeline
//! that streaming will use.

mod pipeline;
mod targets;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gstreamer as gst;
use gst::prelude::*;
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
        /// Window handle from `orange list`.
        #[arg(long)]
        hwnd: isize,
        #[arg(long, default_value = "orange-capture.mkv")]
        out: String,
        #[arg(long, default_value = "av1")]
        codec: String,
        /// Kilobits per second.
        #[arg(long, default_value_t = 30_000)]
        bitrate: u32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Downscale on the GPU, e.g. 1920x1080.
        #[arg(long)]
        scale: Option<String>,
    },
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
            codec,
            bitrate,
            fps,
            seconds,
            scale,
        } => {
            let settings = CaptureSettings {
                hwnd,
                codec: Codec::parse(&codec)?,
                bitrate,
                fps,
                scale: scale.as_deref().map(parse_scale).transpose()?,
            };
            cmd_record(settings, &out, seconds)
        }
    }
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

fn cmd_record(settings: CaptureSettings, out: &str, seconds: u64) -> Result<()> {
    println!(
        "Recording hwnd {} as {:?} at {} kbps -> {out}",
        settings.hwnd, settings.codec, settings.bitrate
    );

    let pipeline = pipeline::build_record_pipeline(&settings, out)?;
    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("pipeline without bus");
    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let mut error = None;

    // Poll the bus rather than blocking forever: a window that never redraws
    // produces no frames, and we would otherwise hang with no explanation.
    while started.elapsed() < deadline {
        let remaining = deadline.saturating_sub(started.elapsed());
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(
            remaining.as_millis().min(200) as u64,
        )) else {
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

    // Clean EOS so the muxer writes its headers; without this the file is
    // unplayable.
    pipeline.send_event(gst::event::Eos::new());
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;

    if let Some(err) = error {
        return Err(err);
    }

    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        anyhow::bail!(
            "captured nothing ({out} is empty or missing). Windows Graphics Capture \
             only produces frames when the window redraws - is it minimised or idle?"
        );
    }
    println!(
        "Wrote {out} ({:.1} MB in {:.1}s)",
        size as f64 / 1_048_576.0,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
