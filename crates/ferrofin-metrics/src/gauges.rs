//! Mirror primitives for async-/DB-sourced gauges.
//!
//! Observable callbacks are synchronous and run on the scrape path (rule 7):
//! they may not block, query the DB, or take a runtime handle. So any value that
//! comes from an async source is mirrored into an atomic ([`GaugeCell`]) or a
//! locked map ([`GaugeMap`]) by the background sampler; the callback only reads
//! the mirror.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// A single-value mirror for one observable gauge. Cheap to clone (shared
/// atomic); the sampler holds a clone and [`set`](GaugeCell::set)s it each tick.
#[derive(Clone)]
pub struct GaugeCell(Arc<AtomicI64>);

impl GaugeCell {
    /// Updates the mirrored value; the next scrape observes it.
    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

/// A `label -> value` mirror for one observable gauge that fans a single label
/// dimension out into multiple series (e.g. `ferrofin_playback_streams{method=…}`).
/// The label value set must be bounded (rule 5).
#[derive(Clone)]
pub struct GaugeMap {
    inner: Arc<Mutex<HashMap<String, i64>>>,
}

impl GaugeMap {
    /// Replaces the whole `label -> value` set; the next scrape observes one
    /// series per entry.
    pub fn set(&self, values: HashMap<String, i64>) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) = values;
    }
}

/// Registers a scalar observable gauge backed by a fresh [`GaugeCell`], carrying
/// the fixed `attrs`.
pub(crate) fn register_cell(
    meter: &Meter,
    name: &'static str,
    desc: &'static str,
    attrs: Vec<KeyValue>,
) -> GaugeCell {
    let cell = Arc::new(AtomicI64::new(0));
    let read = Arc::clone(&cell);
    meter
        .i64_observable_gauge(name)
        .with_description(desc)
        .with_callback(move |obs| obs.observe(read.load(Ordering::Relaxed), &attrs))
        .build();
    GaugeCell(cell)
}

/// Registers a one-label observable gauge backed by a fresh [`GaugeMap`]; each
/// map entry becomes a series `name{<label>=<key>}`.
pub(crate) fn register_map(
    meter: &Meter,
    name: &'static str,
    desc: &'static str,
    label: &'static str,
) -> GaugeMap {
    let inner = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
    let read = Arc::clone(&inner);
    meter
        .i64_observable_gauge(name)
        .with_description(desc)
        .with_callback(move |obs| {
            let guard = read.lock().unwrap_or_else(PoisonError::into_inner);
            for (key, value) in guard.iter() {
                obs.observe(*value, &[KeyValue::new(label, key.clone())]);
            }
        })
        .build();
    GaugeMap { inner }
}
