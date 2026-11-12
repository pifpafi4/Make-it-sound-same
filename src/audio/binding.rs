//! Привязка к устройству вывода: активация менеджера сессий, имя устройства.

use anyhow::Context as _;
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eMultimedia, eRender, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator};
use windows::Win32::System::Com::{CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Привязка к устройству: id, человекочитаемое имя, менеджер сессий,
/// endpoint-громкость (для учёта мастер-громкости в слышимом уровне).
#[derive(Clone)]
pub(crate) struct DeviceBinding {
    pub device_id: String,
    pub device_name: String,
    pub manager: IAudioSessionManager2,
    pub endpoint_volume: IAudioEndpointVolume,
}

/// Устройство рендера по умолчанию (роль Multimedia) + id + имя.
pub(crate) fn default_device(
    enumerator: &IMMDeviceEnumerator,
) -> anyhow::Result<(IMMDevice, String, String, IAudioEndpointVolume)> {
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .context("GetDefaultAudioEndpoint")?;
    let id = super::pwstr_to_string(unsafe { device.GetId()? }).context("GetId")?;
    let name = device_friendly_name(&device).unwrap_or_else(|e| {
        tracing::debug!("имя устройства недоступно: {e:#}");
        "Устройство вывода".to_string()
    });
    let endpoint_volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("Activate(IAudioEndpointVolume)")?;
    Ok((device, id, name, endpoint_volume))
}

/// Активировать IAudioSessionManager2 на устройстве.
pub(crate) fn activate_session_manager(device: &IMMDevice) -> anyhow::Result<IAudioSessionManager2> {
    // SAFETY: COM-вызов в аудиопотоке; объект не пересекает потоки.
    unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }
        .context("Activate(IAudioSessionManager2)")
}

/// Человекочитаемое имя устройства из property store.
fn device_friendly_name(device: &IMMDevice) -> anyhow::Result<String> {
    let store: IPropertyStore =
        unsafe { device.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore")?;
    let mut pv: PROPVARIANT =
        unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.context("GetValue")?;
    let text = propvariant_to_string(&pv);
    // SAFETY: PROPVARIANT получен из COM-вызова, освобождаем как требует контракт.
    unsafe {
        let _ = PropVariantClear(&mut pv);
    }
    text.context("PKEY_Device_FriendlyName не является строкой")
}

fn propvariant_to_string(pv: &PROPVARIANT) -> Option<String> {
    // SAFETY: доступ к union-полю PROPVARIANT; vt() читает тег варианта.
    if pv.vt() == VT_LPWSTR {
        let ptr = unsafe { pv.Anonymous.Anonymous.Anonymous.pwszVal };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: указатель валиден, пока PROPVARIANT не очищен.
        let s = unsafe { PCWSTR::from_raw(ptr.as_ptr()).to_string() };
        s.ok()
    } else {
        None
    }
}
