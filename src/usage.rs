//! Request log and daily token totals.

use crate::util::{day_string, now, write_private};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    /// Pull token counts out of a Claude `usage` or OpenAI/Responses `usage` object.
    pub fn absorb(&mut self, usage: &Value) {
        let n = |k: &str| usage.get(k).and_then(Value::as_u64);
        if let Some(v) = n("input_tokens").or_else(|| n("prompt_tokens")) {
            self.input = self.input.max(v);
        }
        if let Some(v) = n("output_tokens").or_else(|| n("completion_tokens")) {
            self.output = self.output.max(v);
        }
        if let Some(v) = n("cache_read_input_tokens")
            .or_else(|| usage.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64))
            .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64))
        {
            self.cache_read = self.cache_read.max(v);
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.cache_write = self.cache_write.max(v);
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct RequestLog {
    pub ts: u64,
    pub key: String,
    pub provider: String,
    pub account: String,
    pub model: String,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct DayTotals {
    pub requests: u64,
    pub failures: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

/// day -> "account|model" -> totals
type Daily = BTreeMap<String, BTreeMap<String, DayTotals>>;

pub struct Stats {
    path: PathBuf,
    recent: Mutex<VecDeque<RequestLog>>,
    daily: Mutex<Daily>,
    dirty: AtomicBool,
    pub total_requests: AtomicU64,
    pub total_failures: AtomicU64,
    pub in_flight: AtomicU64,
}

const RECENT_CAP: usize = 200;
const KEEP_DAYS: usize = 90;

impl Stats {
    pub fn open(home: &std::path::Path) -> Self {
        let path = home.join("usage.json");
        let daily = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self {
            path,
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
            daily: Mutex::new(daily),
            dirty: AtomicBool::new(false),
            total_requests: AtomicU64::new(0),
            total_failures: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
        }
    }

    pub fn record(&self, log: RequestLog) {
        let failed = log.status >= 400 || log.error.is_some();
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.total_failures.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut daily = self.daily.lock().unwrap();
            let day = daily.entry(day_string(log.ts)).or_default();
            let t = day.entry(format!("{}|{}", log.account, log.model)).or_default();
            t.requests += 1;
            t.failures += failed as u64;
            t.input += log.input;
            t.output += log.output;
            t.cache_read += log.cache_read;
            while daily.len() > KEEP_DAYS {
                let first = daily.keys().next().cloned().unwrap();
                daily.remove(&first);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        let mut recent = self.recent.lock().unwrap();
        if recent.len() == RECENT_CAP {
            recent.pop_front();
        }
        recent.push_back(log);
    }

    pub fn recent(&self, limit: usize) -> Vec<RequestLog> {
        self.recent.lock().unwrap().iter().rev().take(limit).cloned().collect()
    }

    /// Totals over the last `days` days grouped by account and by model.
    pub fn summary(&self, days: usize) -> Value {
        let daily = self.daily.lock().unwrap();
        let cutoff = day_string(now().saturating_sub(days.saturating_sub(1) as u64 * 86400));
        let mut by_account: BTreeMap<String, DayTotals> = BTreeMap::new();
        let mut by_model: BTreeMap<String, DayTotals> = BTreeMap::new();
        let mut by_day: BTreeMap<String, DayTotals> = BTreeMap::new();
        for (day, groups) in daily.range(cutoff..) {
            for (k, t) in groups {
                let (acc, model) = k.split_once('|').unwrap_or((k, ""));
                for (map, key) in [(&mut by_account, acc), (&mut by_model, model), (&mut by_day, day.as_str())] {
                    let e = map.entry(key.to_string()).or_default();
                    e.requests += t.requests;
                    e.failures += t.failures;
                    e.input += t.input;
                    e.output += t.output;
                    e.cache_read += t.cache_read;
                }
            }
        }
        serde_json::json!({ "days": days, "by_account": by_account, "by_model": by_model, "by_day": by_day })
    }

    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let data = serde_json::to_vec(&*self.daily.lock().unwrap()).unwrap_or_default();
        if let Err(e) = write_private(&self.path, &data) {
            tracing::warn!("saving usage: {e}");
        }
    }
}
