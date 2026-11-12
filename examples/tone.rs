//! Тест-хелпер: играет синус 440 Гц по 3 секунды дважды.
//!
//! Полезен для проверки дефолта громкости: запустите `miss` и этот пример —
//! `tone.exe` для утилиты «неизвестное приложение», и его громкость должна
//! упасть до 5% (в micшере и в UI).
//!
//! ```text
//! cargo run --example tone
//! ```

use std::time::Duration;

use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

const SAMPLE_RATE: u32 = 44100;
const DURATION_SEC: f32 = 3.0;
const FREQ_HZ: f32 = 440.0;
const AMPLITUDE: f32 = 0.5;

/// Синтезировать WAV (16-bit PCM, mono) в памяти.
fn sine_wav(freq: f32, sample_rate: u32, duration_sec: f32, amplitude: f32) -> Vec<u8> {
    let samples = (sample_rate as f32 * duration_sec) as usize;
    let data_len = (samples * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // размер fmt-чанка
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());

    for n in 0..samples {
        let t = n as f32 / sample_rate as f32;
        let fade = ((duration_sec - t) / duration_sec).clamp(0.0, 1.0).sqrt();
        let value = (t * freq * std::f32::consts::TAU).sin() * amplitude * fade;
        wav.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes());
    }
    wav
}

fn main() {
    let loud = std::env::args().any(|a| a == "--loud");
    let amplitude = if loud { 1.0 } else { AMPLITUDE };
    let wav = sine_wav(FREQ_HZ, SAMPLE_RATE, DURATION_SEC, amplitude);
    println!(
        "Играю синус {FREQ_HZ} Гц × 2 раза по {DURATION_SEC} с (амплитуда {amplitude}){}",
        if loud { " [loud]" } else { "" }
    );
    for round in 1..=2 {
        println!("  раунд {round}: играет…");
        if loud {
            // Режим «громкой игры»: звук стартует асинхронно, затем поднимаем
            // громкость своей сессии до 100% (сессия появляется только с
            // началом воспроизведения) — как это делают некоторые игры.
            unsafe {
                PlaySoundW(
                    PCWSTR::from_raw(wav.as_ptr().cast()),
                    None,
                    SND_FLAGS(SND_MEMORY.0 | SND_NODEFAULT.0 | SND_ASYNC.0),
                );
            }
            std::thread::sleep(Duration::from_millis(300));
            // Сессия создаётся не мгновенно — ретраим до 2 секунд.
            for _ in 0..10 {
                if self_volume_to_max() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            // SND_ASYNC вернулся сразу — ждём окончания звучания (~2.7 с).
            std::thread::sleep(Duration::from_millis(2700));
        } else {
            // SND_SYNC: PlaySoundW вернётся после окончания воспроизведения,
            // буфер живёт до возврата — безопасно.
            unsafe {
                PlaySoundW(
                    PCWSTR::from_raw(wav.as_ptr().cast()),
                    None,
                    SND_FLAGS(SND_MEMORY.0 | SND_NODEFAULT.0),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    println!("Готово.");
}

/// Найти свою аудиосессию и выставить громкость 100%. true — получилось.
fn self_volume_to_max() -> bool {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, ISimpleAudioVolume,
        IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().expect("COM init");
        let enumerator: IMMDeviceEnumerator = windows::Win32::System::Com::CoCreateInstance(
            &MMDeviceEnumerator,
            None,
            CLSCTX_ALL,
        )
        .expect("enumerator");
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).expect("device");
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).expect("manager");
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            return false;
        };
        let count = sessions.GetCount().unwrap_or(0);
        let my_pid = std::process::id();
        for i in 0..count {
            let Ok(control) = sessions.GetSession(i) else { continue };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
            if control2.GetProcessId() != Ok(my_pid) {
                continue;
            }
            if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                if volume.SetMasterVolume(1.0, std::ptr::null()).is_ok() {
                    println!("  собственная громкость сессии → 100%");
                    return true;
                }
            }
        }
        false
    }
}
