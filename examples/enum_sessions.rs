//! Отладочный пример: перечисляет активные аудиосессии устройства по умолчанию.
//!
//! ```text
//! cargo run --example enum_sessions
//! ```

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, ISimpleAudioVolume,
    IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

fn main() -> anyhow::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    let enumerator: IMMDeviceEnumerator =
        unsafe { windows::Win32::System::Com::CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)? };
    let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let sessions = unsafe { manager.GetSessionEnumerator()? };
    let count = unsafe { sessions.GetCount()? };

    println!("Активных сессий: {count}");
    for i in 0..count {
        let control = unsafe { sessions.GetSession(i)? };
        let control2: IAudioSessionControl2 = control.cast()?;
        let volume: ISimpleAudioVolume = control.cast()?;

        let pid = unsafe { control2.GetProcessId().unwrap_or(0) };
        let path = make_it_sound_same::audio::process::process_image_path(pid)
            .unwrap_or_else(|| "<не определён>".into());
        let vol = unsafe { volume.GetMasterVolume().unwrap_or(f32::NAN) };
        let mute = unsafe { volume.GetMute().map(|b| b.as_bool()).unwrap_or(false) };
        // S_OK (0) — сессия системных звуков; S_FALSE (1) — обычное приложение.
        let system_hr = unsafe { control2.IsSystemSoundsSession() };
        let system = system_hr == windows::Win32::Foundation::S_OK;

        println!(
            "  [{i}] pid={pid} vol={:5.1}% {} hr={:#x} {} {path}",
            vol * 100.0,
            if mute { "[mute]" } else { "      " },
            system_hr.0 as u32,
            if system { "(системные звуки)" } else { "" },
        );
    }
    Ok(())
}
