use std::fs::File;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Файл-приёмник лога: лениво открывает файл в режиме append на каждую запись.
struct AppendFile(PathBuf);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for AppendFile {
    type Writer = std::io::LineWriter<File>;
    fn make_writer(&'a self) -> Self::Writer {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .unwrap_or_else(|_| File::create(&self.0).expect("не удалось создать файл лога"));
        std::io::LineWriter::new(file)
    }
}

/// Инициализация `tracing`. При `with_console` — дублируем в stderr (режим `--debug`),
/// файл `make-it-sound-same.log` пишется всегда.
pub fn init(log_path: PathBuf, with_console: bool) -> anyhow::Result<()> {
    LOG_PATH.set(log_path.clone()).ok();
    let file = tracing_subscriber::fmt::layer()
        .with_writer(AppendFile(log_path.clone()))
        .with_ansi(false)
        .with_filter(EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(file);
    if with_console {
        let console_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let console = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_filter(console_filter);
        registry.with(console).init();
    } else {
        registry.init();
    }

    tracing::info!("лог: {}", log_path.display());
    Ok(())
}

/// Путь к файлу лога (для UI, «открыть папку» и т.п.).
pub fn log_path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}
