use std::fmt;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

pub struct EventFormat {
    pub execd: bool,
}
impl<S, N> FormatEvent<S, N> for EventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        if !self.execd {
            return tracing_subscriber::fmt::format().format_event(ctx, writer, event);
        }
        // Execd treats unprefixed stderr as ERROR. Keep the legacy protocol so
        // ordinary startup/info messages do not appear as Telegraf failures.
        let prefix = match *event.metadata().level() {
            Level::ERROR => "E!",
            Level::WARN => "W!",
            Level::INFO => "I!",
            Level::DEBUG => "D!",
            Level::TRACE => "T!",
        };
        write!(writer, "{prefix} ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn execd_stderr_preserves_all_five_severity_prefixes() {
        let output = Capture::default();
        let capture = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(EventFormat { execd: true })
            .with_max_level(Level::TRACE)
            .with_writer(move || capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("error");
            tracing::warn!("warning");
            tracing::info!(count = 42, "info");
            tracing::debug!("debug");
            tracing::trace!("trace");
        });
        let bytes = output.0.lock().unwrap();
        let lines: Vec<_> = std::str::from_utf8(&bytes).unwrap().lines().collect();
        assert_eq!(
            lines,
            [
                "E! error",
                "W! warning",
                "I! info count=42",
                "D! debug",
                "T! trace"
            ]
        );
    }
}
