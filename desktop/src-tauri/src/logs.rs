use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

#[derive(Clone, Serialize)]
pub struct LogLine {
    pub id: u64,
    pub at_ms: u64,
    pub level: String,
    pub target: String,
    pub message: String,
}

struct Buffer {
    next: u64,
    lines: VecDeque<LogLine>,
}

static LOGS: LazyLock<Mutex<Buffer>> = LazyLock::new(|| {
    Mutex::new(Buffer {
        next: 1,
        lines: VecDeque::new(),
    })
});

pub struct LogCapture;

pub fn recent() -> Vec<LogLine> {
    LOGS.lock().expect("logs").lines.iter().cloned().collect()
}

impl<S> Layer<S> for LogCapture
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::INFO {
            return;
        }
        let mut visitor = Collector::default();
        event.record(&mut visitor);
        let mut message = visitor.message;
        if !visitor.fields.is_empty() {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&visitor.fields.join(" "));
        }
        if message.is_empty() {
            message = event.metadata().name().to_owned();
        }
        let mut guard = LOGS.lock().expect("logs");
        let id = guard.next;
        guard.next += 1;
        guard.lines.push_back(LogLine {
            id,
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
            level: level.to_string(),
            target: event.metadata().target().to_owned(),
            message,
        });
        while guard.lines.len() > 1000 {
            guard.lines.pop_front();
        }
    }
}

#[derive(Default)]
struct Collector {
    message: String,
    fields: Vec<String>,
}

impl Collector {
    fn store(&mut self, name: &str, text: String) {
        if name.eq_ignore_ascii_case("token") || name.eq_ignore_ascii_case("authorization") {
            return;
        }
        if name == "message" {
            self.message = text;
        } else {
            self.fields.push(format!("{name}={text}"));
        }
    }
}

impl Visit for Collector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}").trim_matches('"').to_owned();
        self.store(field.name(), text);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.store(field.name(), value.to_owned());
    }
}
