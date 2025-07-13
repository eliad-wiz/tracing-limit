#![deny(warnings)]

use std::fmt;
use std::{cmp::Ordering, time::Duration};

use dashmap::DashMap;
use derive_builder::Builder;
use tracing_core::{
    Callsite, Event, Kind, Level, Metadata, Subscriber,
    callsite::{DefaultCallsite, Identifier},
    field::{Field, Value, Visit},
    span,
    subscriber::Interest,
};
use tracing_subscriber::layer::{Context, Layer};

#[cfg(test)]
#[macro_use]
extern crate tracing;

#[cfg(not(test))]
use std::time::Instant;

#[cfg(test)]
use mock_instant::global::Instant;

const RATE_LIMIT_FIELD: &str = "internal_log_rate_limit";
const RATE_LIMIT_SECS_FIELD: &str = "internal_log_rate_secs";
const RATE_LIMIT_COUNT_FIELD: &str = "internal_log_rate_count";

const MESSAGE_FIELD: &str = "message";
const RATELIMITED_MESSAGE_FIELD: &str = "ratelimited_message";
const FILTERED_COUNT_FIELD: &str = "filtered_count";

const RATE_LIMIT_STARTED_MESSAGE: &str = "event is being rate limited";
const RATE_LIMIT_STOPPED_MESSAGE: &str = "event stopped being rate limited";

// These fields will cause events to be independently rate limited by the values
// for these keys
const COMPONENT_ID_FIELD: &str = "component_id";
const RATELIMIT_UID: &str = "ratelimit_uid";

#[derive(Eq, PartialEq, Hash, Clone)]
struct RateKeyIdentifier {
    callsite: Identifier,
    rate_limit_key_values: RateLimitedSpanKeys,
}

#[derive(Builder)]
#[builder(default, pattern = "owned")]
pub struct RateLimitConfiguration {
    /// enable rate-limiting automatically for every callsite
    auto_enabled: bool,

    /// optional function to decide whether the event should be ratelimitted
    #[allow(clippy::type_complexity)]
    should_enable_ratelimit: Option<Box<dyn Fn(&Event) -> bool + Send + Sync>>,

    /// max repetitions before rate limit starts
    threshold: u64,

    /// rate limit duration (after that it gets reset)
    duration: Duration,
}

impl Default for RateLimitConfiguration {
    fn default() -> Self {
        Self {
            auto_enabled: false,
            should_enable_ratelimit: None,
            threshold: 1,
            duration: Duration::from_secs(10),
        }
    }
}

pub struct RateLimitedLayer<S, L>
where
    L: Layer<S> + Sized,
    S: Subscriber,
{
    events: DashMap<RateKeyIdentifier, State>,
    inner: L,
    config: RateLimitConfiguration,
    _subscriber: std::marker::PhantomData<S>,
}

impl<S, L> RateLimitedLayer<S, L>
where
    L: Layer<S> + Sized,
    S: Subscriber,
{
    pub fn new(layer: L) -> Self {
        RateLimitedLayer {
            events: Default::default(),
            config: Default::default(),
            inner: layer,
            _subscriber: std::marker::PhantomData,
        }
    }

    pub fn with_config(mut self, config: RateLimitConfiguration) -> Self {
        self.config = config;
        self
    }

    fn should_ratelimit_event(&self, event: &Event) -> bool {
        if self.config.auto_enabled {
            return true;
        }

        if let Some(should_enable_ratelimit) = self.config.should_enable_ratelimit.as_ref() {
            if should_enable_ratelimit(event) {
                return true;
            }
        }

        false
    }
}

impl<S, L> Layer<S> for RateLimitedLayer<S, L>
where
    L: Layer<S>,
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    #[inline]
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.inner.register_callsite(metadata)
    }

    #[inline]
    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.inner.enabled(metadata, ctx)
    }

    // keep track of any span fields we use for grouping rate limiting
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        {
            let span = ctx.span(id).expect("Span not found, this is a bug");
            let mut extensions = span.extensions_mut();

            if extensions.get_mut::<RateLimitedSpanKeys>().is_none() {
                let mut fields = RateLimitedSpanKeys::default();
                attrs.record(&mut fields);
                extensions.insert(fields);
            };
        }
        self.inner.on_new_span(attrs, id, ctx);
    }

    // keep track of any span fields we use for grouping rate limiting
    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        {
            let span = ctx.span(id).expect("Span not found, this is a bug");
            let mut extensions = span.extensions_mut();

            match extensions.get_mut::<RateLimitedSpanKeys>() {
                Some(fields) => {
                    values.record(fields);
                }
                None => {
                    let mut fields = RateLimitedSpanKeys::default();
                    values.record(&mut fields);
                    extensions.insert(fields);
                }
            };
        }
        self.inner.on_record(id, values, ctx);
    }

    #[inline]
    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(span, follows, ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Visit the event, grabbing the limit status if one is defined. If we can't find a rate limit field, or the rate limit
        // is set as false, then we let it pass through untouched.
        let mut limit_visitor = LimitVisitor::default();
        event.record(&mut limit_visitor);

        let limit_exists =
            self.should_ratelimit_event(event) || limit_visitor.limit.unwrap_or_default();
        if !limit_exists {
            return self.inner.on_event(event, ctx);
        }

        let limit_threshold = match limit_visitor.limit_count {
            Some(count) => count,
            None => self.config.threshold,
        };

        let limit_duration = match limit_visitor.limit_secs {
            Some(limit_secs) => Duration::from_secs(limit_secs), // override the cli limit
            None => self.config.duration,
        };

        // Visit all of the spans in the scope of this event, looking for specific fields that we use to differentiate
        // rate-limited events. This ensures that we don't rate limit an event's _callsite_, but the specific usage of a
        // callsite, since multiple copies of the same component could be running, etc.
        let rate_limit_key_values = {
            let mut keys = RateLimitedSpanKeys::default();
            event.record(&mut keys);

            ctx.lookup_current()
                .into_iter()
                .flat_map(|span| span.scope().from_root())
                .fold(keys, |mut keys, span| {
                    let extensions = span.extensions();
                    if let Some(span_keys) = extensions.get::<RateLimitedSpanKeys>() {
                        keys.merge(span_keys);
                    }
                    keys
                })
        };

        // Build the key to represent this event, given its span fields, and see if we're already rate limiting it. If
        // not, we'll initialize an entry for it.
        let metadata = event.metadata();
        let id = RateKeyIdentifier {
            callsite: metadata.callsite(),
            rate_limit_key_values,
        };

        let mut state = self.events.entry(id).or_insert_with(|| {
            let mut message_visitor = MessageVisitor::default();
            event.record(&mut message_visitor);

            let message = message_visitor
                .message
                .unwrap_or_else(|| metadata.name().into());

            State::new(message, limit_threshold, limit_duration)
        });

        // Update our rate limiting state for this event, and see if we should still be rate limiting it.
        //
        // When this is the first time seeing the event, we emit it like we normally would. The second time we see it in
        // the limit period, we emit a new event to indicate that the original event is being actively rate limited
        // Otherwise, we don't emit anything.
        let previous_count = state.increment_count();
        if state.should_limit() {
            match previous_count.cmp(&limit_threshold) {
                Ordering::Less => {} // event will be emitted later
                Ordering::Equal => {
                    self.send_rate_limit_started_event(&ctx, metadata, &state);
                    return;
                }
                Ordering::Greater => {
                    return;
                }
            }
        } else {
            // If we saw this event 3 or more times total, emit an event that indicates the total number of times we
            // rate limited the event in the limit period.
            if previous_count > limit_threshold {
                let filtered_count = previous_count - limit_threshold;

                self.send_rate_limit_stopped_event(&ctx, metadata, &state, filtered_count);
                state.reset();
            } else if state.expired() {
                // TODO: unify checks
                state.reset();
            }

            // We're not rate limiting anymore, so we also emit the current event as normal.. but we update our rate
            // limiting state since this is effectively equivalent to seeing the event again for the first time.
        }

        // drop state after we're done updating it, so that we don't hold the lock while calling the inner layer
        drop(state);

        self.inner.on_event(event, ctx);
    }

    #[inline]
    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    #[inline]
    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    #[inline]
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }

    #[inline]
    fn on_id_change(&self, old: &span::Id, new: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_id_change(old, new, ctx);
    }

    #[inline]
    fn on_layer(&mut self, subscriber: &mut S) {
        self.inner.on_layer(subscriber);
    }
}

impl<S, L> RateLimitedLayer<S, L>
where
    S: Subscriber,
    L: Layer<S>,
{
    fn send_rate_limit_started_event(
        &self,
        ctx: &Context<S>,
        _org_metadata: &'static Metadata<'static>,
        state: &State,
    ) {
        // define our record
        // (just like info!(), but without actually sending the event, since
        // we want it to go directly to the inner layer)
        static CALLSITE: DefaultCallsite = {
            static META: Metadata<'static> = {
                Metadata::new(
                    "event ratelimit",
                    "ratelimit",
                    Level::INFO,
                    Some(file!()),
                    Some(line!()),
                    Some("ratelimit"),
                    ::tracing_core::field::FieldSet::new(
                        &[
                            MESSAGE_FIELD,
                            RATELIMITED_MESSAGE_FIELD,
                            "ratelimit_duration",
                            "ratelimit_threshold",
                        ],
                        ::tracing_core::callsite::Identifier(&CALLSITE),
                    ),
                    Kind::EVENT,
                )
            };
            DefaultCallsite::new(&META)
        };

        // fill all fields
        let metadata = CALLSITE.metadata();
        let fields = metadata.fields();
        let mut iter = fields.iter();

        let duration_sec = state.limit_duration.as_secs();
        let values = [
            (
                &iter.next().unwrap(),
                Some(&RATE_LIMIT_STARTED_MESSAGE as &dyn Value),
            ),
            (&iter.next().unwrap(), Some(&state.message as &dyn Value)),
            (&iter.next().unwrap(), Some(&duration_sec as &dyn Value)),
            (
                &iter.next().unwrap(),
                Some(&state.limit_threshold as &dyn Value),
            ),
        ];
        let valueset = fields.value_set(&values);

        // send event
        let event = Event::new(metadata, &valueset);
        self.inner.on_event(&event, ctx.clone());
    }

    fn send_rate_limit_stopped_event(
        &self,
        ctx: &Context<S>,
        _org_metadata: &'static Metadata<'static>,
        state: &State,
        filtered_count: u64,
    ) {
        // define our record
        // (just like info!(), but without actually sending the event, since
        // we want it to go directly to the inner layer)
        static CALLSITE: DefaultCallsite = {
            static META: Metadata<'static> = {
                Metadata::new(
                    "event ratelimit",
                    "ratelimit",
                    Level::INFO,
                    Some(file!()),
                    Some(line!()),
                    Some("ratelimit"),
                    ::tracing_core::field::FieldSet::new(
                        &[
                            MESSAGE_FIELD,
                            RATELIMITED_MESSAGE_FIELD,
                            "ratelimit_duration",
                            "ratelimit_threshold",
                            FILTERED_COUNT_FIELD,
                        ],
                        ::tracing_core::callsite::Identifier(&CALLSITE),
                    ),
                    Kind::EVENT,
                )
            };
            DefaultCallsite::new(&META)
        };

        // fill all fields
        let metadata = CALLSITE.metadata();
        let fields = metadata.fields();
        let mut iter = fields.iter();
        let duration_sec = state.limit_duration.as_secs();
        let values = [
            (
                &iter.next().unwrap(),
                Some(&RATE_LIMIT_STOPPED_MESSAGE as &dyn Value),
            ),
            (&iter.next().unwrap(), Some(&state.message as &dyn Value)),
            (&iter.next().unwrap(), Some(&duration_sec as &dyn Value)),
            (
                &iter.next().unwrap(),
                Some(&state.limit_threshold as &dyn Value),
            ),
            (&iter.next().unwrap(), Some(&filtered_count as &dyn Value)),
        ];
        let valueset = fields.value_set(&values);

        // send event
        let event = Event::new(metadata, &valueset);
        self.inner.on_event(&event, ctx.clone());
    }
}

#[derive(Debug)]
struct State {
    start: Instant,
    count: u64,
    limit_threshold: u64,
    limit_duration: Duration,
    message: String,
}

impl State {
    fn new(message: String, limit_threshold: u64, limit_duration: Duration) -> Self {
        Self {
            start: Instant::now(),
            count: 0,
            limit_threshold,
            limit_duration,
            message,
        }
    }

    fn reset(&mut self) {
        self.start = Instant::now();
        self.count = 1;
    }

    fn increment_count(&mut self) -> u64 {
        let prev = self.count;
        self.count += 1;
        prev
    }

    fn expired(&self) -> bool {
        self.start.elapsed() >= self.limit_duration
    }

    fn should_limit(&self) -> bool {
        self.count > self.limit_threshold && !self.expired()
    }
}

#[derive(PartialEq, Eq, Clone, Hash)]
enum TraceValue {
    String(String),
    Int(i64),
    Uint(u64),
    Bool(bool),
}

impl From<bool> for TraceValue {
    fn from(b: bool) -> Self {
        TraceValue::Bool(b)
    }
}

impl From<i64> for TraceValue {
    fn from(i: i64) -> Self {
        TraceValue::Int(i)
    }
}

impl From<u64> for TraceValue {
    fn from(u: u64) -> Self {
        TraceValue::Uint(u)
    }
}

impl From<String> for TraceValue {
    fn from(s: String) -> Self {
        TraceValue::String(s)
    }
}

/// RateLimitedSpanKeys records span keys that we use to rate limit callsites separately by. For
/// example, if a given trace callsite is called from two different components, then they will be
/// rate limited separately.
#[derive(Default, Eq, PartialEq, Hash, Clone)]
struct RateLimitedSpanKeys {
    component_id: Option<TraceValue>,
    ratelimit_uid: Option<TraceValue>,
}

impl RateLimitedSpanKeys {
    fn record(&mut self, field: &Field, value: TraceValue) {
        match field.name() {
            COMPONENT_ID_FIELD => self.component_id = Some(value),
            RATELIMIT_UID => self.ratelimit_uid = Some(value),
            _ => {}
        }
    }

    fn merge(&mut self, other: &Self) {
        if let Some(component_id) = &other.component_id {
            self.component_id = Some(component_id.clone());
        }
        if let Some(ratelimit_uid) = &other.ratelimit_uid {
            self.ratelimit_uid = Some(ratelimit_uid.clone());
        }
    }
}

impl Visit for RateLimitedSpanKeys {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned().into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}").into());
    }
}

#[derive(Default)]
struct LimitVisitor {
    pub limit: Option<bool>,
    pub limit_secs: Option<u64>,
    pub limit_count: Option<u64>,
}

impl Visit for LimitVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == RATE_LIMIT_FIELD {
            self.limit = Some(value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            RATE_LIMIT_SECS_FIELD => {
                // override the cli passed limit
                self.limit_secs = Some(u64::try_from(value).unwrap_or_default());
            }
            RATE_LIMIT_COUNT_FIELD => {
                // override the cli passed limit
                self.limit_count = Some(u64::try_from(value).unwrap_or_default());
            }
            _ => return,
        }
        self.limit = Some(true); // limit if we have these fields
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            RATE_LIMIT_SECS_FIELD => {
                // override the cli passed limit
                self.limit_secs = Some(value);
            }
            RATE_LIMIT_COUNT_FIELD => {
                // override the cli passed limit
                self.limit_count = Some(value);
            }
            _ => return,
        }
        self.limit = Some(true); // limit if we have these fields
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

#[derive(Default)]
struct MessageVisitor {
    pub message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if self.message.is_none() && field.name() == MESSAGE_FIELD {
            self.message = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.message.is_none() && field.name() == MESSAGE_FIELD {
            self.message = Some(format!("{value:?}"));
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use mock_instant::global::MockClock;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    static TEST_MUTEX: Mutex<i32> = Mutex::new(1);

    #[derive(Default, Debug, PartialEq, Eq)]
    struct TestVisitor {
        pub message: Option<String>,
        pub ratelimited_message: Option<String>,
        pub filtered_count: Option<u64>,
    }

    impl From<(&str, Option<&str>, Option<u64>)> for TestVisitor {
        fn from(value: (&str, Option<&str>, Option<u64>)) -> Self {
            Self {
                message: Some(value.0.to_owned()),
                ratelimited_message: value.1.map(|v| v.to_owned()),
                filtered_count: value.2,
            }
        }
    }

    impl Visit for TestVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                MESSAGE_FIELD => self.message = Some(value.to_string()),
                RATELIMITED_MESSAGE_FIELD => self.ratelimited_message = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if self.filtered_count.is_none() && field.name() == FILTERED_COUNT_FIELD {
                self.filtered_count = Some(value);
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            match field.name() {
                MESSAGE_FIELD => self.message = Some(format!("{:?}", value)),
                RATELIMITED_MESSAGE_FIELD => {
                    self.ratelimited_message = Some(format!("{:?}", value))
                }
                _ => {}
            }
        }
    }

    #[derive(Default)]
    struct RecordingLayer<S> {
        events: Arc<Mutex<Vec<TestVisitor>>>,

        _subscriber: std::marker::PhantomData<S>,
    }

    impl<S> RecordingLayer<S> {
        fn new(events: Arc<Mutex<Vec<TestVisitor>>>) -> Self {
            RecordingLayer {
                events,

                _subscriber: std::marker::PhantomData,
            }
        }
    }

    impl<S> Layer<S> for RecordingLayer<S>
    where
        S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }

        fn enabled(&self, _metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
            true
        }

        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = TestVisitor::default();
            event.record(&mut visitor);

            let mut events = self.events.lock().unwrap();
            events.push(visitor);
        }
    }

    #[test]
    fn rate_limits() {
        let _guard = TEST_MUTEX.lock().unwrap();
        MockClock::set_time(Duration::ZERO);
        let events = Default::default();

        let recorder = RecordingLayer::new(Arc::clone(&events));
        let sub = tracing_subscriber::registry::Registry::default().with(
            RateLimitedLayer::new(recorder).with_config(
                RateLimitConfigurationBuilder::default()
                    .duration(Duration::from_secs(1))
                    .build()
                    .unwrap(),
            ),
        );

        tracing::subscriber::with_default(sub, || {
            for i in 0..21 {
                info!(
                    test = i,
                    // message = "Hello world!",
                    internal_log_rate_limit = true,
                    "Hello world!"
                );
                MockClock::advance(Duration::from_millis(100));
            }
        });

        let events = events.lock().unwrap();

        assert_eq!(
            *events,
            vec![
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(9)),
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(9)),
                ("Hello world!", None, None),
            ]
            .into_iter()
            .map(|v| v.into())
            .collect::<Vec<TestVisitor>>()
        );
    }

    #[test]
    fn override_rate_limit_at_callsite() {
        let _guard = TEST_MUTEX.lock().unwrap();
        MockClock::set_time(Duration::ZERO);
        let events = Default::default();

        let recorder = RecordingLayer::new(Arc::clone(&events));
        let sub = tracing_subscriber::registry::Registry::default().with(
            RateLimitedLayer::new(recorder).with_config(
                RateLimitConfigurationBuilder::default()
                    .duration(Duration::from_secs(100))
                    .build()
                    .unwrap(),
            ),
        );
        tracing::subscriber::with_default(sub, || {
            for _ in 0..21 {
                info!(
                    message = "Hello world!",
                    internal_log_rate_limit = true,
                    internal_log_rate_secs = 1
                );
                MockClock::advance(Duration::from_millis(100));
            }
        });

        let events = events.lock().unwrap();

        assert_eq!(
            *events,
            vec![
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(9)),
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(9)),
                ("Hello world!", None, None),
            ]
            .into_iter()
            .map(|v| v.into())
            .collect::<Vec<TestVisitor>>()
        );
    }

    #[test]
    fn override_rate_limit_count_at_callsite() {
        let _guard = TEST_MUTEX.lock().unwrap();
        MockClock::set_time(Duration::ZERO);
        let events = Default::default();

        let recorder = RecordingLayer::new(Arc::clone(&events));
        let sub = tracing_subscriber::registry::Registry::default().with(
            RateLimitedLayer::new(recorder).with_config(
                RateLimitConfigurationBuilder::default()
                    .duration(Duration::from_secs(1))
                    .build()
                    .unwrap(),
            ),
        );
        tracing::subscriber::with_default(sub, || {
            for _ in 0..21 {
                info!(
                    message = "Hello world!",
                    internal_log_rate_limit = true,
                    internal_log_rate_secs = 1,
                    internal_log_rate_count = 3,
                );
                MockClock::advance(Duration::from_millis(100));
            }
        });

        let events = events.lock().unwrap();

        assert_eq!(
            *events,
            vec![
                ("Hello world!", None, None),
                ("Hello world!", None, None),
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(7)),
                ("Hello world!", None, None),
                ("Hello world!", None, None),
                ("Hello world!", None, None),
                (RATE_LIMIT_STARTED_MESSAGE, Some("Hello world!"), None),
                (RATE_LIMIT_STOPPED_MESSAGE, Some("Hello world!"), Some(7)),
                ("Hello world!", None, None),
            ]
            .into_iter()
            .map(|v| v.into())
            .collect::<Vec<TestVisitor>>()
        );
    }

    #[test]
    fn rate_limit_by_span_key() {
        let _guard = TEST_MUTEX.lock().unwrap();
        MockClock::set_time(Duration::ZERO);
        let events = Default::default();

        let recorder = RecordingLayer::new(Arc::clone(&events));
        let sub = tracing_subscriber::registry::Registry::default().with(
            RateLimitedLayer::new(recorder).with_config(
                RateLimitConfigurationBuilder::default()
                    .duration(Duration::from_secs(1))
                    .build()
                    .unwrap(),
            ),
        );
        tracing::subscriber::with_default(sub, || {
            for _ in 0..21 {
                for key in &["foo", "bar"] {
                    for line_number in &[1, 2] {
                        let span =
                            info_span!("span", component_id = &key, ratelimit_uid = &line_number);
                        let _enter = span.enter();
                        info!(
                            message =
                                format!("Hello {} on line_number {}!", key, line_number).as_str(),
                            internal_log_rate_limit = true
                        );
                    }
                }
                MockClock::advance(Duration::from_millis(100));
            }
        });

        let events = events.lock().unwrap();

        assert_eq!(
            *events,
            vec![
                ("Hello foo on line_number 1!", None, None),
                ("Hello foo on line_number 2!", None, None),
                ("Hello bar on line_number 1!", None, None),
                ("Hello bar on line_number 2!", None, None),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    Some(9)
                ),
                ("Hello foo on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    Some(9)
                ),
                ("Hello foo on line_number 2!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    Some(9)
                ),
                ("Hello bar on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    Some(9)
                ),
                ("Hello bar on line_number 2!", None, None),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    Some(9)
                ),
                ("Hello foo on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    Some(9)
                ),
                ("Hello foo on line_number 2!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    Some(9)
                ),
                ("Hello bar on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    Some(9)
                ),
                ("Hello bar on line_number 2!", None, None),
            ]
            .into_iter()
            .map(|v| v.into())
            .collect::<Vec<TestVisitor>>()
        );
    }

    #[test]
    fn rate_limit_by_event_key() {
        let _guard = TEST_MUTEX.lock().unwrap();
        MockClock::set_time(Duration::ZERO);
        let events = Default::default();

        let recorder = RecordingLayer::new(Arc::clone(&events));
        let sub = tracing_subscriber::registry::Registry::default().with(
            RateLimitedLayer::new(recorder).with_config(
                RateLimitConfigurationBuilder::default()
                    .duration(Duration::from_secs(1))
                    .build()
                    .unwrap(),
            ),
        );
        tracing::subscriber::with_default(sub, || {
            for _ in 0..21 {
                for key in &["foo", "bar"] {
                    for line_number in &[1, 2] {
                        info!(
                            message =
                                format!("Hello {} on line_number {}!", key, line_number).as_str(),
                            internal_log_rate_limit = true,
                            component_id = &key,
                            ratelimit_uid = &line_number
                        );
                    }
                }
                MockClock::advance(Duration::from_millis(100));
            }
        });

        let events = events.lock().unwrap();

        assert_eq!(
            *events,
            vec![
                ("Hello foo on line_number 1!", None, None),
                ("Hello foo on line_number 2!", None, None),
                ("Hello bar on line_number 1!", None, None),
                ("Hello bar on line_number 2!", None, None),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    Some(9)
                ),
                ("Hello foo on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    Some(9)
                ),
                ("Hello foo on line_number 2!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    Some(9)
                ),
                ("Hello bar on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    Some(9)
                ),
                ("Hello bar on line_number 2!", None, None),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    None
                ),
                (
                    RATE_LIMIT_STARTED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    None
                ),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 1!"),
                    Some(9)
                ),
                ("Hello foo on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello foo on line_number 2!"),
                    Some(9)
                ),
                ("Hello foo on line_number 2!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 1!"),
                    Some(9)
                ),
                ("Hello bar on line_number 1!", None, None),
                (
                    RATE_LIMIT_STOPPED_MESSAGE,
                    Some("Hello bar on line_number 2!"),
                    Some(9)
                ),
                ("Hello bar on line_number 2!", None, None),
            ]
            .into_iter()
            .map(|v| v.into())
            .collect::<Vec<TestVisitor>>()
        );
    }
}
