use std::{fmt, io::IsTerminal};

use tracing::Subscriber;
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

struct PrefixedFormat<F> {
    prefix: String,
    inner: F,
}

impl<S, N, F> FormatEvent<S, N> for PrefixedFormat<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        write!(writer, "{} ", self.prefix)?;
        self.inner.format_event(ctx, writer, event)
    }
}

/// Initialize the tracing subscriber with a process-identifying prefix.
///
/// Every log line will start with `prefix` (e.g. `pishoo/1234`,
/// `pishoo-worker:alice/5678`), regardless of which module or third-party
/// library emitted the event.
///
/// Returns a [`tracing_appender::non_blocking::WorkerGuard`] that **must** be
/// held alive for the lifetime of the process to ensure log flushing.
pub fn init_tracing(prefix: &str) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{
        EnvFilter, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
    };

    let (stderr, guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
                .with_writer(stderr)
                .event_format(PrefixedFormat {
                    prefix: prefix.to_owned(),
                    inner: tracing_subscriber::fmt::format(),
                }),
        )
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    guard
}
