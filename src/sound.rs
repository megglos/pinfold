//! Benchmark sounds: short tones that Pinfold makes at run time. No sound
//! files, and no sound of the Windows sound scheme (those mean "error" or
//! "message" in other programs).

use std::sync::OnceLock;

use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

use crate::bench::Sound;

const RATE: u32 = 44_100;

/// One note: frequency in Hz, length in ms.
type Note = (f64, u32);

// Notes (equal temperament).
const C4: f64 = 261.63;
const E4: f64 = 329.63;
const C5: f64 = 523.25;
const E5: f64 = 659.25;
const G5: f64 = 783.99;
const A5: f64 = 880.00;

struct Spec {
    notes: &'static [Note],
    /// Peak level, 0 to 1.
    volume: f64,
    /// Level of the second harmonic. A little makes the tone less thin.
    overtone: f64,
}

fn spec(sound: Sound) -> Spec {
    match sound {
        // Two rising notes: "go".
        Sound::Start => Spec { notes: &[(C5, 110), (G5, 220)], volume: 0.35, overtone: 0.15 },
        // One short, soft blip: "next profile".
        Sound::Switch => Spec { notes: &[(A5, 70)], volume: 0.2, overtone: 0.1 },
        // Two falling, lower notes: "look at this".
        Sound::Problem => Spec { notes: &[(E4, 160), (C4, 280)], volume: 0.4, overtone: 0.3 },
        // Major chord upwards: "finished".
        Sound::Done => {
            Spec { notes: &[(C5, 100), (E5, 100), (G5, 300)], volume: 0.35, overtone: 0.15 }
        }
    }
}

/// A mono 16-bit PCM WAV file with the notes one after the other. Each note
/// has a 5 ms attack and an exponential decay, thus there are no clicks.
fn wav(notes: &[Note], volume: f64, overtone: f64) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for &(freq, ms) in notes {
        let n = (RATE as u64 * ms as u64 / 1000) as usize;
        let attack = (RATE as f64 * 0.005) as usize;
        for i in 0..n {
            let t = i as f64 / RATE as f64;
            let rise = (i as f64 / attack as f64).min(1.0);
            // Decay to about 2 % at the end of the note.
            let fall = (-4.0 * i as f64 / n as f64).exp();
            let fade = if i + attack > n { (n - i) as f64 / attack as f64 } else { 1.0 };
            let tau = std::f64::consts::TAU;
            let wave = ((tau * freq * t).sin() + overtone * (2.0 * tau * freq * t).sin())
                / (1.0 + overtone);
            let v = wave * volume * rise * fall * fade;
            samples.push((v * i16::MAX as f64) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes per sample
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// The WAV data of a sound. Made one time, then kept: an asynchronous play
/// reads the memory while it plays.
fn data(sound: Sound) -> &'static [u8] {
    static CELLS: [OnceLock<Vec<u8>>; 4] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let index = match sound {
        Sound::Start => 0,
        Sound::Switch => 1,
        Sound::Problem => 2,
        Sound::Done => 3,
    };
    CELLS[index].get_or_init(|| {
        let s = spec(sound);
        wav(s.notes, s.volume, s.overtone)
    })
}

/// Play a sound. Start and Switch do not wait: the capture continues.
/// Problem and Done wait until the end, because the process can stop next.
pub fn play(sound: Sound) {
    let mut flags = SND_MEMORY | SND_NODEFAULT;
    if matches!(sound, Sound::Start | Sound::Switch) {
        flags |= SND_ASYNC;
    }
    unsafe { PlaySoundW(data(sound).as_ptr().cast(), std::ptr::null_mut(), flags) };
}

/// Length of a sound in ms.
pub fn length_ms(sound: Sound) -> u32 {
    spec(sound).notes.iter().map(|n| n.1).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Sound; 4] = [Sound::Start, Sound::Switch, Sound::Problem, Sound::Done];

    #[test]
    fn each_sound_is_a_valid_short_wav() {
        for sound in ALL {
            let w = data(sound);
            assert_eq!(&w[0..4], b"RIFF");
            assert_eq!(&w[8..16], b"WAVEfmt ");
            assert_eq!(&w[36..40], b"data");
            let len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
            assert_eq!(w.len(), 44 + len);
            let ms = len as u64 / 2 * 1000 / RATE as u64;
            assert!(ms > 50 && ms <= 500, "{sound:?}: {ms} ms");
            assert!(ms.abs_diff(length_ms(sound) as u64) <= 1, "{sound:?}");
        }
    }

    #[test]
    fn sounds_are_not_too_loud_and_start_and_end_quietly() {
        for sound in ALL {
            let s = spec(sound);
            let w = wav(s.notes, s.volume, s.overtone);
            let samples: Vec<i16> =
                w[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            let peak = samples.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(peak as f64 <= s.volume * i16::MAX as f64 + 1.0, "{sound:?}");
            assert!(peak > 1000, "{sound:?} is audible");
            assert!(samples[0].unsigned_abs() < 100, "{sound:?} starts without a click");
            assert!(samples.last().unwrap().unsigned_abs() < 100, "{sound:?} ends without a click");
        }
    }

    #[test]
    fn start_rises_and_problem_falls() {
        let first = |s: Sound| spec(s).notes[0].0;
        let last = |s: Sound| spec(s).notes.last().unwrap().0;
        assert!(last(Sound::Start) > first(Sound::Start));
        assert!(last(Sound::Done) > first(Sound::Done));
        assert!(last(Sound::Problem) < first(Sound::Problem));
        assert!(first(Sound::Problem) < first(Sound::Start), "problem is lower");
    }
}
