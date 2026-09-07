//! Short synthesised cues.
//!
//! Sound earns its place here for one reason: streaming happens while you are
//! looking at the game, not at this window. A viewer arriving, or the stream
//! you are watching ending, is invisible unless the client is in front of you.
//!
//! The tones are generated rather than shipped as files. WAV files would be
//! assets to source, licence, embed and keep in step; this file has none
//! of that, and it lets the cues share a vocabulary the way `ui::motion` does
//! for animation. Everything is built from three shapes:
//!
//!   a rising fifth   something began
//!   a falling fifth  something ended
//!   a single note    somebody else arrived or left
//!
//! Friend-start alerts can be muted per friend. All cues are short, and never
//! fire for something the user just clicked and is already watching happen.
//!
//! Two earlier attempts are worth recording, because both failed for reasons
//! that are measurable rather than matters of taste.
//!
//! A pure sine bell has no spectral evolution: its timbre at 150 ms is
//! identical to its timbre at zero. Nothing physical behaves that way, so the
//! ear cannot attribute it to an object and files it under "generic electronic
//! tone". The fix is not a different waveform, it is letting the spectrum move.
//!
//! Then a square wave sliced by 60 Hz amplitude modulation, meant to evoke the
//! scanline in the mark. That is close to the textbook definition of auditory
//! roughness - one asper is a 1 kHz tone fully modulated at 70 Hz - so it was
//! the worst available modulation rate, and it read as a broken speaker. The
//! honest translation is that a scanline is *spatial* periodicity, whose audio
//! analogue is periodicity in the spectrum rather than in time: a comb, which
//! puts evenly spaced peaks and notches across the band and is perfectly
//! smooth in time.

use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

/// CD rate. Nothing here needs it, but every Windows mixer resamples to
/// something, and 44.1 kHz is the one least likely to be resampled badly.
const SAMPLE_RATE: f32 = 44_100.0;

/// Harmonics past this either alias or turn into hiss, so a stack stops early
/// on the higher notes rather than sanding them.
const CEILING_HZ: f32 = 15_000.0;

/// D minor, which is arbitrary, but picking one key stops the cues from
/// clashing when two land close together - a viewer joining the moment you go
/// live, say. Every pitch sits between 500 and 1300 Hz, because laptop speakers
/// roll off hard below about 450 and anything above 1500 starts reading as an
/// alarm.
const D5: f32 = 587.33;
const A5: f32 = 880.00;
/// The chord the session cues run through, plus the one note outside it that
/// belongs to somebody else arriving.
const F5: f32 = 698.46;
const C6: f32 = 1046.50;
const D6: f32 = 1174.66;
/// A minor ninth over D5. Sounded together these two beat against each other,
/// which is the interval every operating system reaches for when it needs to
/// say "read this" - and it keeps the alert out of the register a laptop cannot
/// reproduce, where a low tone would simply have gone missing.
const EF6: f32 = 1244.51;

/// Session cues carry information the user is waiting for, so they sit above
/// the peer cues, which are ambient.
///
/// Peak amplitudes as a fraction of full scale. The first attempt used a third
/// of these, on the theory that a cue with no mute switch should be shy.
/// Measured, that gave three milliseconds above a tenth of full scale for a
/// stream going live and none at all for a viewer arriving: not restraint, just
/// broken. A Windows notification peaks near -8 dB; these sit a little under.
const SESSION_GAIN: f32 = 0.42;
const PEER_GAIN: f32 = 0.34;
const ALERT_GAIN: f32 = 0.45;

/// One component of a struck sound: where it sits relative to the fundamental,
/// and how loud it starts.
#[derive(Clone, Copy)]
struct Partial {
    ratio: f32,
    gain: f32,
}

const fn partial(ratio: f32, gain: f32) -> Partial {
    Partial { ratio, gain }
}

/// A harmonic series with the stiffness a real string or bar has, which pushes
/// each partial slightly sharp of where arithmetic would put it. Too small to
/// hear as detuning, big enough to remove the computed quality of an exact
/// stack.
const STIFF: &[Partial] = &[
    partial(1.000, 1.00),
    partial(2.004, 0.42),
    partial(3.014, 0.18),
    partial(4.032, 0.09),
    partial(5.062, 0.04),
];

/// How a cue is made. One voice is used for every cue in the set, because a
/// family of sounds is recognisable only when it is the same instrument saying
/// different things - the audio equivalent of having exactly one accent colour.
#[derive(Clone, Copy)]
pub struct Voice {
    /// The partials to stack.
    partials: &'static [Partial],
    /// How much faster the upper partials die than the fundamental. This is the
    /// difference between a stack of oscillators and something that was struck:
    /// real objects shed their high frequencies first.
    damping: f32,
    /// A pitch drop over the first few milliseconds. Struck objects go briefly
    /// sharp under the force of the strike, so a small fall reads as impact
    /// rather than as a slide.
    bend_semitones: f32,
    bend_ms: f32,
    /// A few milliseconds of filtered noise at the onset, before the tone, the
    /// way contact precedes resonance. Mute the tone and this should be a soft
    /// "tk"; if it is identifiable as noise in the mix it is far too loud.
    noise: f32,
    /// A second copy of the upper partials, offset by a fixed number of hertz
    /// rather than a fixed number of cents. Fixed cents would make the high
    /// cues shimmer twice as fast as the low ones and break the family.
    shimmer_hz: f32,
    /// Band limits. The bottom is below what a laptop can move and only eats
    /// excursion; sustained energy between 3 and 4 kHz is what fatigues on the
    /// hundredth hearing.
    highpass_hz: f32,
    lowpass_hz: f32,
    /// Two early reflections. Enough to say the sound happened somewhere,
    /// without saying "reverb".
    room: f32,
}

/// The voice the app ships with.
///
/// Fewer partials surviving than anything else that was tried, a low ceiling,
/// no attack noise and a longer room: almost no high frequency content, so it
/// sits under whatever is already playing rather than cutting through it. That
/// is the whole point of it, and also its one risk - the version of this that
/// was auditioned was judged too easy to miss, which is why the notes below are
/// half as long again as they were.
pub const VOICE: Voice = Voice {
    partials: STIFF,
    damping: 1.2,
    bend_semitones: 0.7,
    bend_ms: 18.0,
    noise: 0.0,
    shimmer_hz: 4.0,
    highpass_hz: 200.0,
    lowpass_hz: 4_900.0,
    room: 0.34,
};

/// One note: pitch, an optional second pitch sounded with it, length, and peak.
#[derive(Clone, Copy)]
struct Note {
    hz: f32,
    with: f32,
    ms: u32,
    gain: f32,
}

const fn note(hz: f32, ms: u32, gain: f32) -> Note {
    Note {
        hz,
        with: 0.0,
        ms,
        gain,
    }
}

const fn dyad(hz: f32, with: f32, ms: u32, gain: f32) -> Note {
    Note { hz, with, ms, gain }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// You went live, or joined someone's stream.
    Live,
    /// Your stream stopped, or the one you were watching ended.
    Ended,
    /// Somebody started watching you.
    Joined,
    /// Somebody stopped watching you.
    Left,
    /// A friend started streaming.
    FriendLive,
    /// Something failed.
    Alert,
}

/// The D minor chord, root to octave, and the same four degrees back down for
/// an ending. Onsets are what the ear notices, so four of them register far
/// harder than one long note of the same total length - which is what makes
/// this audible over a game where a single tone was not.
///
/// Minor rather than major on purpose. A major arpeggio is cheerful in a way
/// nothing else in this app is; minor reads as serious and technical, which is
/// the same register as near-black, one accent colour and captions with no
/// exclamation marks in them.
///
/// The run is quick and the landing is long: three short steps and then a note
/// that stays, so the cue arrives somewhere rather than just stopping.
const LIVE: &[Note] = &[
    note(D5, 55, SESSION_GAIN),
    note(F5, 55, SESSION_GAIN),
    note(A5, 55, SESSION_GAIN),
    note(D6, 140, SESSION_GAIN),
];
const ENDED: &[Note] = &[
    note(D6, 55, SESSION_GAIN),
    note(A5, 55, SESSION_GAIN),
    note(F5, 55, SESSION_GAIN),
    note(D5, 140, SESSION_GAIN),
];
/// A single step rather than a run, and on C, which is the one note here that
/// is outside the chord the session cues are built from. Someone else arriving
/// is not your session changing, and it should not sound like a small version
/// of it - it should sound like a different kind of event entirely.
const JOINED: &[Note] = &[note(C6, 70, PEER_GAIN), note(D6, 150, PEER_GAIN)];
const LEFT: &[Note] = &[note(D6, 70, PEER_GAIN), note(C6, 150, PEER_GAIN)];
const FRIEND_LIVE: &[Note] = &[note(D5, 70, PEER_GAIN), note(A5, 120, PEER_GAIN)];

/// Both pitches at once rather than one after the other. Sounded together a
/// minor ninth beats; played in sequence it is just a wide leap, and the
/// dissonance that makes this unmistakable is gone.
const ALERT: &[Note] = &[
    dyad(D5, EF6, 110, ALERT_GAIN),
    dyad(D5, EF6, 125, ALERT_GAIN),
];

impl Cue {
    fn notes(self) -> &'static [Note] {
        match self {
            Cue::Live => LIVE,
            Cue::Ended => ENDED,
            Cue::Joined => JOINED,
            Cue::Left => LEFT,
            Cue::FriendLive => FRIEND_LIVE,
            Cue::Alert => ALERT,
        }
    }

    /// The rendered WAV, built once and kept for the life of the process.
    ///
    /// `SND_ASYNC` returns before playback finishes and reads the buffer as it
    /// goes, so the bytes have to outlive the call. A static is the only way to
    /// promise that without tracking when the sound stopped.
    fn wave(self) -> &'static [u8] {
        static WAVES: OnceLock<[Vec<u8>; 6]> = OnceLock::new();
        let waves = WAVES.get_or_init(|| {
            [
                render(Cue::Live.notes(), &VOICE),
                render(Cue::Ended.notes(), &VOICE),
                render(Cue::Joined.notes(), &VOICE),
                render(Cue::Left.notes(), &VOICE),
                render(Cue::FriendLive.notes(), &VOICE),
                render(Cue::Alert.notes(), &VOICE),
            ]
        });
        match self {
            Cue::Live => &waves[0],
            Cue::Ended => &waves[1],
            Cue::Joined => &waves[2],
            Cue::Left => &waves[3],
            Cue::FriendLive => &waves[4],
            Cue::Alert => &waves[5],
        }
    }
}

/// Play a cue, returning immediately.
///
/// `PlaySound` has one slot per process, so a second cue cuts the first short.
/// For sounds this brief that is the behaviour you want: rapid events collapse
/// into the most recent one rather than queuing into a stutter.
pub fn play(cue: Cue) {
    let wave = cue.wave();
    // SAFETY: the buffer is static, so it stays valid for the whole
    // asynchronous playback. SND_MEMORY means the pointer is data rather than a
    // name, so the PCWSTR type is only how PlaySound spells "some bytes".
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(wave.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// Render a cue to a 16-bit mono WAV.
fn render(notes: &[Note], voice: &Voice) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut noise_seed = 0x5EED_1234u32;
    for note in notes {
        voice_note(&mut buffer, note, voice, &mut noise_seed);
    }
    band_limit(&mut buffer, voice);
    room(&mut buffer, voice);

    let target = notes.iter().fold(0.0f32, |peak, n| peak.max(n.gain));
    normalise(&mut buffer, target);
    wav(&buffer)
}

/// Append one note.
fn voice_note(out: &mut Vec<f32>, note: &Note, voice: &Voice, seed: &mut u32) {
    let count = (SAMPLE_RATE * note.ms as f32 / 1000.0) as usize;
    if count == 0 {
        return;
    }
    // Long enough to let the partials separate, short enough that the shape
    // below still runs the note to silence well before it ends.
    let decay = note.ms as f32 / 1000.0 * 0.6;
    let attack = (SAMPLE_RATE * 0.003) as usize;
    let noise_span = (SAMPLE_RATE * 0.008) as usize;

    for index in 0..count {
        let seconds = index as f32 / SAMPLE_RATE;
        let progress = index as f32 / count as f32;

        let mut value = pitched(note.hz, seconds, decay, voice);
        if note.with > 0.0 {
            value = (value + pitched(note.with, seconds, decay, voice)) * 0.5;
        }

        // Contact before resonance: the noise leads, the tone rises under it.
        if index < noise_span && voice.noise > 0.0 {
            let fade = 1.0 - index as f32 / noise_span as f32;
            value += white(seed) * voice.noise * fade * fade;
        }

        // The amplitude curve is the complement of ease_out_quint, the same
        // function the UI animates with, after a short plateau. The plateau is
        // not decoration: the bare quintic is down to a tenth of its peak
        // inside 30 ms, which is a correct release shape and no note at all.
        // Held first, then released that way, the cue has a body to hear and
        // still reaches exactly zero at the end, so it can never click out.
        const HOLD: f32 = 0.35;
        let shape = if progress < HOLD {
            1.0
        } else {
            let released = 1.0 - (progress - HOLD) / (1.0 - HOLD);
            0.85 * released.powi(5) + 0.15 * released.powi(2)
        };
        let onset = if index < attack {
            0.5 - 0.5 * (std::f32::consts::PI * index as f32 / attack as f32).cos()
        } else {
            1.0
        };
        out.push(value * shape * onset * note.gain);
    }
}

/// One sample of a single pitch: partial stack or FM, plus the pitch drop and
/// the shimmer.
fn pitched(hz: f32, seconds: f32, decay: f32, voice: &Voice) -> f32 {
    // A brief rise settling onto the true pitch. Applied as a multiplier on
    // elapsed phase, which is close enough over a few milliseconds and costs
    // nothing to integrate.
    let bend = if voice.bend_semitones > 0.0 {
        let fall = (-seconds / (voice.bend_ms / 1000.0)).exp();
        2f32.powf(voice.bend_semitones / 12.0 * fall)
    } else {
        1.0
    };
    let base = hz * bend;

    let mut value = 0.0;
    for partial in voice.partials {
        let hz = base * partial.ratio;
        if hz > CEILING_HZ {
            break;
        }
        // Upper partials die first, which is what every struck object does and
        // what a bank of equal-decay oscillators never does. The rate is
        // relative: the fundamental is left alone and shaped only by the
        // amplitude curve, because decaying it here as well would compound with
        // that curve and run the note to silence before anyone heard it.
        let excess = partial.ratio.powf(voice.damping) - 1.0;
        let level = if excess <= 0.001 {
            partial.gain
        } else {
            partial.gain * (-seconds / (decay / excess)).exp()
        };
        value += level * (std::f32::consts::TAU * hz * seconds).sin();
        if voice.shimmer_hz > 0.0 && partial.ratio > 1.0 {
            let beat = std::f32::consts::TAU * (hz + voice.shimmer_hz) * seconds;
            value += level * 0.5 * beat.sin();
        }
    }
    value
}

/// One pole each way. The high pass removes what a laptop cannot move; the low
/// pass takes off the top, which is the single largest difference between a
/// cue that sounds designed and one that sounds generated.
fn band_limit(buffer: &mut [f32], voice: &Voice) {
    let coefficient = |hz: f32| 1.0 - (-std::f32::consts::TAU * hz / SAMPLE_RATE).exp();
    let high = coefficient(voice.highpass_hz);
    let low = coefficient(voice.lowpass_hz);
    let mut below = 0.0;
    let mut smoothed = 0.0;
    for sample in buffer.iter_mut() {
        below += high * (*sample - below);
        let without_low_end = *sample - below;
        smoothed += low * (without_low_end - smoothed);
        *sample = smoothed;
    }
}

/// Two early reflections, at times chosen not to share a common factor so they
/// do not fuse into a single ring. Under about 8 ms a tap colours the tone
/// instead of placing it; past 35 ms it reads as a distinct echo.
fn room(buffer: &mut Vec<f32>, voice: &Voice) {
    if voice.room <= 0.0 {
        return;
    }
    const FIRST: usize = 631;
    const SECOND: usize = 1019;
    let dry = buffer.clone();
    buffer.resize(dry.len() + SECOND + 1, 0.0);
    for (index, sample) in dry.iter().enumerate() {
        buffer[index + FIRST] += sample * voice.room;
        buffer[index + SECOND] += sample * voice.room * 0.55;
    }
}

/// Scale so the *body* of the cue sits at the requested peak.
///
/// Not the loudest single sample: every partial starts in phase, so the first
/// few milliseconds interfere constructively into a spike roughly twice the
/// height of anything after it. Normalising to that spike is what made an
/// earlier version of these cues measure loud and sound quiet - the number you
/// hear is the sustained level, not the transient. A high percentile finds that
/// level and lets the transient stand proud of it, which is what a real strike
/// does anyway; a final check keeps the whole thing inside the rails.
fn normalise(buffer: &mut [f32], target: f32) {
    let mut magnitudes: Vec<f32> = buffer.iter().map(|sample| sample.abs()).collect();
    if magnitudes.is_empty() {
        return;
    }
    let index = magnitudes.len() * 995 / 1000;
    magnitudes.sort_unstable_by(f32::total_cmp);
    let body = magnitudes[index.min(magnitudes.len() - 1)];
    let peak = magnitudes[magnitudes.len() - 1];
    if body <= 0.0 || peak <= 0.0 {
        return;
    }
    let scale = (target / body).min(0.95 / peak);
    for sample in buffer.iter_mut() {
        *sample *= scale;
    }
    // The room tail is appended after the last note, so the buffer no longer
    // ends at zero on its own. A few milliseconds of ramp costs nothing and
    // guarantees it.
    let fade = (SAMPLE_RATE * 0.004) as usize;
    let len = buffer.len();
    for index in 0..fade.min(len) {
        buffer[len - 1 - index] *= index as f32 / fade as f32;
    }
}

fn white(seed: &mut u32) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    (*seed as f32 / 2_147_483_648.0) - 1.0
}

/// Wrap PCM samples in the 44-byte canonical WAV header.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM header length
    out.extend_from_slice(&1u16.to_le_bytes()); // uncompressed
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    out.extend_from_slice(&((SAMPLE_RATE as u32) * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        out.extend_from_slice(&((clamped * f32::from(i16::MAX)) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUES: [Cue; 6] = [
        Cue::Live,
        Cue::Ended,
        Cue::Joined,
        Cue::Left,
        Cue::FriendLive,
        Cue::Alert,
    ];

    fn samples(cue: Cue) -> Vec<i16> {
        cue.wave()[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect()
    }

    #[test]
    fn every_cue_renders_a_wav_windows_will_accept() {
        for cue in CUES {
            let wave = cue.wave();
            assert_eq!(&wave[0..4], b"RIFF", "{cue:?}");
            assert_eq!(&wave[8..12], b"WAVE", "{cue:?}");
            assert_eq!(&wave[36..40], b"data", "{cue:?}");
            // The two lengths in the header have to agree with the buffer, or
            // PlaySound reads past the end of it.
            let riff = u32::from_le_bytes(wave[4..8].try_into().unwrap()) as usize;
            let data = u32::from_le_bytes(wave[40..44].try_into().unwrap()) as usize;
            assert_eq!(riff, wave.len() - 8, "{cue:?}");
            assert_eq!(data, wave.len() - 44, "{cue:?}");
        }
    }

    #[test]
    fn no_cue_outstays_the_moment_it_marks() {
        // A cue is punctuation, and past some length it starts reading as a
        // jingle. The bound was 240 ms on
        // the reasoning that shorter is politer, until a listening test found
        // the cues easy to miss entirely, which beats the reasoning. Half a
        // second of notes is the point where a cue stops being an aside.
        //
        // The room tail is excluded: it is decaying, not speaking.
        for cue in CUES {
            let total: u32 = cue.notes().iter().map(|note| note.ms).sum();
            assert!(total <= 500, "{cue:?} runs {total} ms");
        }
    }

    #[test]
    fn every_cue_is_loud_enough_and_long_enough_to_notice() {
        // The first version of this file failed here and nothing caught it. The
        // tests checked that cues did not clip - the risk I was watching -
        // while the gains and the decay together left three milliseconds above
        // a tenth of full scale for a stream going live, and none at all for a
        // viewer arriving. Silence passes a test for "not too loud".
        //
        // Measured on the envelope, in five-millisecond windows, rather than by
        // counting samples: a sine spends most of every cycle below its own
        // peak, so counting samples scores a perfectly audible tone at about
        // eight tenths of its real length and says nothing about loudness that
        // anyone could hear.
        //
        // A tenth of full scale is roughly where a short tone stops competing
        // with whatever is already playing, and 40 ms is about the shortest
        // burst that reads as a note rather than as a tick.
        const PERCEPTIBLE: i16 = i16::MAX / 10;
        const WINDOW_MS: usize = 5;
        let window = (SAMPLE_RATE as usize / 1000) * WINDOW_MS;
        for cue in CUES {
            let loud = samples(cue)
                .chunks(window)
                .filter(|slice| {
                    slice
                        .iter()
                        .any(|sample| sample.saturating_abs() > PERCEPTIBLE)
                })
                .count();
            let ms = loud * WINDOW_MS;
            assert!(ms >= 40, "{cue:?} is audible for only {ms} ms");
        }
    }

    #[test]
    fn cues_start_and_end_at_silence() {
        // Any jump from silence is a click, which sounds like a fault in the
        // audio device rather than like a deliberate sound.
        for cue in CUES {
            let samples = samples(cue);
            assert_eq!(samples[0], 0, "{cue:?} opens with a click");
            assert_eq!(samples[samples.len() - 1], 0, "{cue:?} ends with a click");
        }
    }

    #[test]
    fn nothing_clips() {
        // Clipping is what makes a loud sound feel cheap, and a hard clip in a
        // cue this short reads as a fault in the device. The body level is set
        // by the gain constants; this guards the transient that stands above
        // it. See the loudness floor above for what happened when a ceiling was
        // the only bound.
        for cue in CUES {
            let peak = samples(cue)
                .into_iter()
                .map(i16::saturating_abs)
                .max()
                .unwrap_or(0);
            let ceiling = (f32::from(i16::MAX) * 0.96) as i16;
            assert!(peak < ceiling, "{cue:?} peaks at {peak}");
        }
    }

    #[test]
    fn a_stranger_arriving_never_drowns_out_your_own_session() {
        // Peer cues fire while the user is mid-game and are ambient; session
        // cues answer a question they actually asked.
        for cue in [Cue::Joined, Cue::Left, Cue::FriendLive] {
            for note in cue.notes() {
                assert!(note.gain < SESSION_GAIN, "{cue:?}");
            }
        }
    }

    #[test]
    fn the_alert_is_the_only_cue_that_is_dissonant() {
        // Its dissonance is the whole point, and a minor ninth is only
        // dissonant when the two pitches overlap - in sequence it is just a
        // wide leap. Other cues stack pitches too, for body, but only ever at
        // intervals that agree with each other, so nothing else can be mistaken
        // for a warning.
        fn semitones(note: &Note) -> f32 {
            12.0 * (note.with / note.hz).log2().abs()
        }
        for note in Cue::Alert.notes() {
            assert!(
                note.with > 0.0,
                "the alert has to sound two pitches at once"
            );
            let interval = semitones(note);
            assert!(
                (interval - 13.0).abs() < 0.1,
                "expected a minor ninth, got {interval:.1} semitones"
            );
        }
        for cue in [
            Cue::Live,
            Cue::Ended,
            Cue::Joined,
            Cue::Left,
            Cue::FriendLive,
        ] {
            for note in cue.notes().iter().filter(|note| note.with > 0.0) {
                let interval = semitones(note) % 12.0;
                let consonant = interval < 0.1 || (interval - 7.0).abs() < 0.1;
                assert!(consonant, "{cue:?} stacks {interval:.1} semitones");
            }
        }
    }

    #[test]
    fn your_own_session_always_says_more_than_someone_elses_arrival() {
        // How many times a cue speaks is load-bearing rather than decorative,
        // and it is the only part of the vocabulary that survives being heard
        // from another room with the door shut. Your stream starting or ending
        // runs the whole chord; somebody else arriving is a single step. If a
        // peer cue ever grew to match, the two would be told apart only by
        // their pitches, which is exactly what does not carry at a distance.
        let session = Cue::Live.notes().len().min(Cue::Ended.notes().len());
        let peer = Cue::Joined
            .notes()
            .len()
            .max(Cue::Left.notes().len())
            .max(Cue::FriendLive.notes().len());
        assert!(session > peer, "{session} against {peer}");

        // And the two peer cues are a step apart rather than a leap, so they
        // read as a small event next to the run the session cues make.
        for cue in [Cue::Joined, Cue::Left] {
            let notes = cue.notes();
            let step = 12.0 * (notes[1].hz / notes[0].hz).log2();
            assert!(step.abs() < 3.0, "{cue:?} moves {step:.1} semitones");
        }
        let friend_step =
            12.0 * (Cue::FriendLive.notes()[1].hz / Cue::FriendLive.notes()[0].hz).log2();
        assert!(
            (friend_step - 7.0).abs() < 0.1,
            "FriendLive moves {friend_step:.1} semitones"
        );
    }

    #[test]
    fn beginnings_and_endings_run_the_same_chord_in_opposite_directions() {
        // The vocabulary is only learnable if it is consistent: whatever
        // "started" sounds like, "ended" is that backwards. Same for a viewer
        // arriving and leaving.
        for (up, down) in [(Cue::Live, Cue::Ended), (Cue::Joined, Cue::Left)] {
            let rising: Vec<f32> = up.notes().iter().map(|note| note.hz).collect();
            let falling: Vec<f32> = down.notes().iter().map(|note| note.hz).collect();
            assert_eq!(
                rising,
                falling.iter().rev().copied().collect::<Vec<_>>(),
                "{up:?} against {down:?}"
            );
            assert!(rising[0] < rising[rising.len() - 1], "{up:?} has to rise");
        }
    }

    #[test]
    fn friend_live_cue_is_a_short_rising_fifth() {
        let notes = Cue::FriendLive.notes();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].ms, 70);
        assert_eq!(notes[1].ms, 120);
        assert!((notes[0].hz - D5).abs() < 0.01);
        assert!((notes[1].hz - A5).abs() < 0.01);
    }

    #[test]
    fn every_pitch_survives_a_laptop_speaker() {
        // Laptops roll off hard below about 450 Hz - some ship a 250 Hz high
        // pass in the codec to protect the driver - and above 1500 Hz a short
        // tone stops reading as a notification and starts reading as an alarm.
        // The alert used to sit at 349 Hz, where half the machines that matter
        // would simply not have played it.
        for cue in CUES {
            for note in cue.notes() {
                for hz in [note.hz, note.with].into_iter().filter(|hz| *hz > 0.0) {
                    assert!((500.0..=1300.0).contains(&hz), "{cue:?} at {hz} Hz");
                }
            }
        }
    }

    #[test]
    #[ignore = "writes files for listening; run with --ignored"]
    fn export_cues_for_listening() {
        // cargo test -p orange-client export_cues -- --ignored --nocapture
        //
        // A timbre is not something anyone can settle by reading a struct
        // literal. This existed alongside a table of alternatives while the
        // voice was being chosen; the alternatives are gone, but writing the
        // shipping set out is still the only way to hear a change to it without
        // launching the app and provoking each event.
        let directory = std::env::temp_dir().join("orange-cues");
        std::fs::create_dir_all(&directory).unwrap();
        for cue in CUES {
            let path = directory.join(format!("{cue:?}.wav").to_lowercase());
            std::fs::write(&path, cue.wave()).unwrap();
            println!("{}", path.display());
        }
    }
}
