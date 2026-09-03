use std::collections::BTreeMap;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use crate::control::LogEvent;

pub fn init_tracing(level: &str, format: &str, log_tx: broadcast::Sender<LogEvent>) -> Result<()> {
    let level_filter = parse_level(level);

    let init_result = match parse_format(format) {
        LogFormat::Json => {
            let fmt_layer = fmt::layer()
                .with_writer(std::io::stderr)
                .json()
                .with_current_span(false)
                .with_span_list(false)
                .with_filter(fmt_targets(level_filter));

            tracing_subscriber::registry()
                .with(ControlLogLayer {
                    log_tx: log_tx.clone(),
                })
                .with(fmt_layer)
                .try_init()
        }
        LogFormat::Text => {
            let fmt_layer = fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(true)
                .with_filter(fmt_targets(level_filter));

            tracing_subscriber::registry()
                .with(ControlLogLayer { log_tx })
                .with(fmt_layer)
                .try_init()
        }
    };

    if let Err(err) = init_result {
        let msg = err.to_string();
        if msg.contains("global default trace dispatcher has already been set") {
            return Ok(());
        }
        return Err(err.into());
    }

    Ok(())
}

enum LogFormat {
    Text,
    Json,
}

fn parse_format(format: &str) -> LogFormat {
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => LogFormat::Json,
        _ => LogFormat::Text,
    }
}

fn parse_level(level: &str) -> LevelFilter {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" | "warning" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

fn fmt_targets(level: LevelFilter) -> Targets {
    Targets::new()
        .with_target("tandem", level)
        .with_target("jj_tandem", level)
        .with_default(LevelFilter::WARN)
}

struct ControlLogLayer {
    log_tx: broadcast::Sender<LogEvent>,
}

impl<S> Layer<S> for ControlLogLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();
        let from_tandem = target.starts_with("tandem") || target.starts_with("jj_tandem");
        let is_warn_or_error = matches!(
            *metadata.level(),
            tracing::Level::WARN | tracing::Level::ERROR
        );
        if !from_tandem && !is_warn_or_error {
            return;
        }

        let mut visitor = EventFieldVisitor::default();
        event.record(&mut visitor);

        let msg = visitor
            .message
            .unwrap_or_else(|| metadata.name().to_string());

        let event = LogEvent {
            ts: unix_timestamp(),
            level: metadata.level().to_string().to_ascii_lowercase(),
            target: metadata.target().to_string(),
            msg,
            fields: visitor.fields,
        };

        let _ = self.log_tx.send(event);
    }
}

#[derive(Default)]
struct EventFieldVisitor {
    message: Option<String>,
    fields: BTreeMap<String, String>,
}

impl tracing::field::Visit for EventFieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.record_value(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record_value(field.name(), format!("{value:?}"));
    }
}

impl EventFieldVisitor {
    fn record_value(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = Some(value);
        } else {
            self.fields.insert(name.to_string(), value);
        }
    }
}

fn unix_timestamp() -> String {
    use std::time::SystemTime;

    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", d.as_secs())
}
