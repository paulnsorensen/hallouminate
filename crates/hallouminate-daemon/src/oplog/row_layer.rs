use std::fmt;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tracing::field::{Empty, Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Metadata, Span, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, Filter, Layer};
use tracing_subscriber::registry::{LookupSpan, SpanRef};

use super::{
    DaemonOpFields, Millis, OpLog, OperationRecord, Outcome, Reason, RecordKind, RequestId,
    SCHEMA_VERSION, Stages,
};

/// Target of the root span of one daemon operation.
pub const TARGET: &str = "hallouminate_daemon::oplog";
/// Name of the root span of one daemon operation.
pub const ROOT_SPAN: &str = "daemon_op";
/// Name of a stage span under a root span from [`root_span`].
pub const STAGE_SPAN: &str = "stage";
/// `op` of a request line that did not parse.
pub const UNKNOWN_OP: &str = "unknown";

const CODE_SEPARATOR: &str = ",";

/// Opens the root span of one daemon operation.
///
/// The span records its outcome through [`record_outcome`] and its warnings
/// through [`record_warnings`]. The [`layer`] writes the `daemon_op` row when
/// the span closes.
pub fn root_span(request_id: &RequestId, op: &str, corpus: Option<&str>) -> Span {
    tracing::info_span!(
        target: TARGET,
        ROOT_SPAN,
        request_id = request_id.as_str(),
        op,
        corpus,
        outcome = Empty,
        reason = Empty,
        warning_count = Empty,
        warning_codes = Empty,
    )
}

/// Records `outcome` on a span from [`root_span`].
///
/// The operation duration of the row ends at the first call, even when the
/// span closes later.
pub fn record_outcome(span: &Span, outcome: Outcome) {
    let (name, reason) = outcome_names(outcome);
    span.record("outcome", name);
    if let Some(reason) = reason {
        span.record("reason", reason);
    }
}

/// Records the warning count and the warning codes on a span from
/// [`root_span`].
pub fn record_warnings(span: &Span, count: u64, codes: &[String]) {
    span.record("warning_count", count);
    span.record("warning_codes", codes.join(CODE_SEPARATOR).as_str());
}

/// Opens the stage span `name` under the current span.
///
/// The stage runs from this call until the last handle of the returned span
/// drops; the span need not be entered. The [`layer`] adds the elapsed time
/// to the stage `name` of the enclosing root span from [`root_span`]. A
/// stage that opens while another stage of the same root span is open
/// counts toward neither the row nor the open stage, so the stage durations
/// never sum to more than the operation duration.
///
/// Code that cannot reach this function opens the same span with
/// `tracing::info_span!(target: TARGET, STAGE_SPAN, stage = name)`, using the
/// string values of [`TARGET`] and [`STAGE_SPAN`].
pub fn stage(name: &'static str) -> Span {
    tracing::info_span!(target: TARGET, STAGE_SPAN, stage = name)
}

/// Builds the layer that writes one `daemon_op` row through `log` when a span
/// from [`root_span`] closes.
///
/// The layer sees only spans and events with target [`TARGET`].
pub fn layer<S>(log: OpLog) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    RowLayer { log }.with_filter(RowFilter(
        Targets::new().with_target(TARGET, tracing::Level::INFO),
    ))
}

struct RowLayer {
    log: OpLog,
}

/// Per-layer filter that never gives a callsite of [`TARGET`] the interest
/// `always`.
///
/// With `always`, `tracing` skips `enabled` before a new span. The span then
/// takes this thread's per-layer filter state from the last `enabled` call,
/// which can disable this layer when no span or event followed that call.
struct RowFilter(Targets);

impl<S> Filter<S> for RowFilter {
    fn enabled(&self, metadata: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        self.0.would_enable(metadata.target(), metadata.level())
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        match self.0.would_enable(metadata.target(), metadata.level()) {
            true => Interest::sometimes(),
            false => Interest::never(),
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::INFO)
    }
}

struct Pending {
    start: Instant,
    end: Option<Instant>,
    end_ts: Option<DateTime<Utc>>,
    fields: RowFields,
    stages: Stages,
    stage_slot: StageSlot,
}

enum StageSlot {
    Free,
    Taken,
}

struct OpenStage {
    name: String,
    start: Instant,
}

#[derive(Default)]
struct StageName(Option<String>);

impl Visit for StageName {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "stage" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

#[derive(Default)]
struct RowFields {
    request_id: Option<RequestId>,
    op: Option<String>,
    corpus: Option<String>,
    outcome: Option<String>,
    reason: Option<String>,
    warning_count: u64,
    warning_codes: Vec<String>,
}

impl Visit for RowFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "request_id" => self.request_id = value.parse().ok(),
            "op" => self.op = Some(value.to_owned()),
            "corpus" => self.corpus = Some(value.to_owned()),
            "outcome" => self.outcome = Some(value.to_owned()),
            "reason" => self.reason = Some(value.to_owned()),
            "warning_codes" => {
                let mut codes = Vec::new();
                for code in value.split(CODE_SEPARATOR) {
                    if !code.is_empty() {
                        codes.push(code.to_owned());
                    }
                }
                self.warning_codes = codes;
            }
            _other => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "warning_count" {
            self.warning_count = value;
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

impl Pending {
    fn into_record(self) -> OperationRecord {
        let Pending {
            start,
            end,
            end_ts,
            fields:
                RowFields {
                    request_id,
                    op,
                    corpus,
                    outcome,
                    reason,
                    warning_count,
                    warning_codes,
                },
            stages,
            stage_slot: _,
        } = self;
        let end = end.unwrap_or_else(Instant::now);
        OperationRecord {
            schema_version: SCHEMA_VERSION,
            ts: end_ts.unwrap_or_else(Utc::now),
            request_id: request_id.unwrap_or_else(RequestId::generate),
            op: op.unwrap_or_else(|| UNKNOWN_OP.to_owned()),
            outcome: parse_outcome(outcome.as_deref(), reason.as_deref()),
            duration_ms: Millis::from_duration(end.duration_since(start)),
            corpus,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            kind: RecordKind::DaemonOp(DaemonOpFields {
                stages,
                warning_count,
                warning_codes,
            }),
        }
    }
}

fn root_of<'a, S>(span: &SpanRef<'a, S>) -> Option<SpanRef<'a, S>>
where
    S: Subscriber + for<'l> LookupSpan<'l>,
{
    span.scope()
        .skip(1)
        .find(|ancestor| ancestor.name() == ROOT_SPAN)
}

fn open_stage<S>(attrs: &Attributes<'_>, span: &SpanRef<'_, S>)
where
    S: Subscriber + for<'l> LookupSpan<'l>,
{
    let mut name = StageName::default();
    attrs.record(&mut name);
    let StageName(Some(name)) = name else {
        return;
    };
    let Some(root) = root_of(span) else {
        return;
    };
    let mut extensions = root.extensions_mut();
    let Some(pending) = extensions.get_mut::<Pending>() else {
        return;
    };
    match &pending.stage_slot {
        StageSlot::Taken => {
            return;
        }
        StageSlot::Free => {
            pending.stage_slot = StageSlot::Taken;
        }
    }
    drop(extensions);
    span.extensions_mut().insert(OpenStage {
        name,
        start: Instant::now(),
    });
}

fn close_stage<S>(stage: OpenStage, span: &SpanRef<'_, S>)
where
    S: Subscriber + for<'l> LookupSpan<'l>,
{
    let OpenStage { name, start } = stage;
    let elapsed = Millis::from_duration(start.elapsed());
    let Some(root) = root_of(span) else {
        return;
    };
    let mut extensions = root.extensions_mut();
    let Some(pending) = extensions.get_mut::<Pending>() else {
        return;
    };
    pending.stages.add(&name, elapsed);
    pending.stage_slot = StageSlot::Free;
}

impl<S> Layer<S> for RowLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        match attrs.metadata().name() {
            ROOT_SPAN => {
                let mut fields = RowFields::default();
                attrs.record(&mut fields);
                span.extensions_mut().insert(Pending {
                    start: Instant::now(),
                    end: None,
                    end_ts: None,
                    fields,
                    stages: Stages::default(),
                    stage_slot: StageSlot::Free,
                });
            }
            STAGE_SPAN => open_stage(attrs, &span),
            _other => {}
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(pending) = extensions.get_mut::<Pending>() else {
            return;
        };
        values.record(&mut pending.fields);
        if pending.end.is_none() && pending.fields.outcome.is_some() {
            pending.end = Some(Instant::now());
            pending.end_ts = Some(Utc::now());
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let stage = span.extensions_mut().remove::<OpenStage>();
        if let Some(stage) = stage {
            close_stage(stage, &span);
            return;
        }
        let Some(pending) = span.extensions_mut().remove::<Pending>() else {
            return;
        };
        self.log.append_and_enforce_cap(&pending.into_record());
    }
}

fn outcome_names(outcome: Outcome) -> (&'static str, Option<&'static str>) {
    match outcome {
        Outcome::Ok => ("ok", None),
        Outcome::InvalidParams => ("invalid_params", None),
        Outcome::Internal => ("internal", None),
        Outcome::Retryable => ("retryable", None),
        Outcome::Cancelled(Reason::Shutdown) => ("cancelled", Some("shutdown")),
        Outcome::Cancelled(Reason::Timeout) => ("cancelled", Some("timeout")),
        Outcome::Unavailable => ("unavailable", None),
    }
}

fn parse_outcome(outcome: Option<&str>, reason: Option<&str>) -> Outcome {
    match (outcome, reason) {
        (Some("ok"), _reason) => Outcome::Ok,
        (Some("invalid_params"), _reason) => Outcome::InvalidParams,
        (Some("retryable"), _reason) => Outcome::Retryable,
        (Some("unavailable"), _reason) => Outcome::Unavailable,
        (Some("cancelled"), Some("timeout")) => Outcome::Cancelled(Reason::Timeout),
        (Some("cancelled"), _reason) => Outcome::Cancelled(Reason::Shutdown),
        (Some(_internal_or_unknown), _reason) => Outcome::Internal,
        (None, _reason) => Outcome::Internal,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use hallouminate_config::LoggingConfig;
    use tracing::callsite::{Callsite, Identifier};
    use tracing::field::FieldSet;
    use tracing::metadata::Kind;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn production_subscriber(dir: &Path) -> impl Subscriber + Send + Sync {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(EnvFilter::new("hallouminate=info")),
            )
            .with(layer(OpLog::in_dir(dir, &LoggingConfig::default())))
    }

    fn single_row(dir: &Path) -> OperationRecord {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read log dir") {
            let path = entry.expect("log dir entry").path();
            for line in std::fs::read_to_string(&path)
                .expect("read segment")
                .lines()
            {
                rows.push(serde_json::from_str::<OperationRecord>(line).expect("row json"));
            }
        }
        let [row] = <[OperationRecord; 1]>::try_from(rows).expect("exactly one row");
        row
    }

    struct ElsewhereCallsite;

    static ELSEWHERE_CALLSITE: ElsewhereCallsite = ElsewhereCallsite;

    /// Metadata that both filters reject, like a dependency's
    /// `log::log_enabled!` call that asks `enabled` and emits nothing.
    static ELSEWHERE: Metadata<'static> = Metadata::new(
        "rejected",
        "elsewhere",
        tracing::Level::INFO,
        None,
        None,
        None,
        FieldSet::new(&[], Identifier(&ELSEWHERE_CALLSITE)),
        Kind::EVENT,
    );

    impl Callsite for ElsewhereCallsite {
        fn set_interest(&self, _interest: Interest) {}

        fn metadata(&self) -> &Metadata<'_> {
            &ELSEWHERE
        }
    }

    #[test]
    fn stage_after_an_unanswered_enabled_call_reaches_the_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        tracing::subscriber::with_default(production_subscriber(dir.path()), || {
            let root = root_span(&RequestId::generate(), "ground", None);
            let _entered = root.enter();
            tracing::dispatcher::get_default(|dispatch| dispatch.enabled(&ELSEWHERE));
            drop(stage("fuse"));
            record_outcome(&root, Outcome::Ok);
        });
        let row = single_row(dir.path());
        let RecordKind::DaemonOp(DaemonOpFields {
            stages,
            warning_count: _,
            warning_codes: _,
        }) = &row.kind
        else {
            panic!("not a daemon_op row: {row:?}");
        };
        assert!(stages.as_map().contains_key("fuse"), "{row:?}");
    }

    #[test]
    fn duration_ends_when_the_outcome_is_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let after_outcome = Duration::from_millis(300);
        tracing::subscriber::with_default(production_subscriber(dir.path()), || {
            let root = root_span(&RequestId::generate(), "ping", None);
            record_outcome(&root, Outcome::Ok);
            std::thread::sleep(after_outcome);
        });
        let row = single_row(dir.path());
        assert!(
            row.duration_ms < Millis::from_duration(after_outcome),
            "duration includes the time after the outcome: {row:?}"
        );
    }

    #[test]
    fn timestamp_is_taken_when_the_outcome_is_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut close_time = None;
        tracing::subscriber::with_default(production_subscriber(dir.path()), || {
            let root = root_span(&RequestId::generate(), "ping", None);
            record_outcome(&root, Outcome::Ok);
            std::thread::sleep(Duration::from_millis(50));
            close_time = Some(Utc::now());
        });
        let close_time = close_time.expect("closure ran");
        let row = single_row(dir.path());
        assert!(
            row.ts <= close_time - chrono::Duration::milliseconds(25),
            "ts follows the span close, not the outcome: {row:?} close {close_time}"
        );
    }

    #[test]
    fn outcome_names_round_trip() {
        for outcome in [
            Outcome::Ok,
            Outcome::InvalidParams,
            Outcome::Internal,
            Outcome::Retryable,
            Outcome::Cancelled(Reason::Shutdown),
            Outcome::Cancelled(Reason::Timeout),
            Outcome::Unavailable,
        ] {
            let (name, reason) = outcome_names(outcome);
            assert_eq!(parse_outcome(Some(name), reason), outcome);
        }
        assert_eq!(parse_outcome(None, None), Outcome::Internal);
    }
}
