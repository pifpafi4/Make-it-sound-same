#![windows_subsystem = "windows"]

fn main() -> anyhow::Result<()> {
    // GUI-подсистема Windows: консоль при обычном запуске не открывается.
    // Флаг `--debug` подключает консоль (родительскую или новую) для логов.
    let debug = make_it_sound_same::console::attach_if_debug();
    make_it_sound_same::run(debug)
}
