//! Debounced reporting of how each tile shows its stream (`flushViewReport` /
//! `scheduleViewReport`), so the hub can match each stream's resolution to its tile.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use xrmc_core::view_size::ViewRequest;

/// Default wait after a change (the Electron app waits 250 ms).
pub const DEFAULT_DELAY: Duration = Duration::from_millis(250);
/// A report is sent at most this long after the first unsent change, even while
/// things keep changing (e.g. dragging the window edge).
pub const MAX_DELAY: Duration = Duration::from_millis(1000);

/// What changed since the last report: stream id → requirement, `None` = no longer shown.
pub type ViewDiff = Vec<(String, Option<ViewRequest>)>;

#[derive(Debug, Default)]
pub struct ViewReporter {
    sent: BTreeMap<String, ViewRequest>,
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    first: Instant,
    due: Instant,
    last: BTreeMap<String, ViewRequest>,
}

impl ViewReporter {
    pub fn new() -> Self {
        Self::default()
    }

    fn diff(&self, current: &BTreeMap<String, ViewRequest>) -> ViewDiff {
        let mut out: ViewDiff = current
            .iter()
            .filter(|(id, req)| self.sent.get(*id) != Some(*req))
            .map(|(id, req)| (id.clone(), Some(req.clone())))
            .collect();
        out.extend(
            self.sent
                .keys()
                .filter(|id| !current.contains_key(*id))
                .map(|id| (id.clone(), None)),
        );
        out
    }

    /// Call every UI frame with the current requirements. Returns a diff to send
    /// once things have been stable for `delay` (or [`MAX_DELAY`] has passed).
    pub fn update(
        &mut self,
        now: Instant,
        current: &BTreeMap<String, ViewRequest>,
        delay: Duration,
    ) -> Option<ViewDiff> {
        if self.diff(current).is_empty() {
            self.pending = None;
            return None;
        }
        match &mut self.pending {
            None => {
                self.pending = Some(Pending {
                    first: now,
                    due: now + delay,
                    last: current.clone(),
                });
            }
            Some(p) => {
                if &p.last != current {
                    p.last = current.clone();
                    p.due = (now + delay).min(p.first + MAX_DELAY.max(delay));
                }
            }
        }
        let due = self.pending.as_ref().map(|p| p.due)?;
        if now >= due {
            return Some(self.flush(current));
        }
        None
    }

    /// Send everything that changed now (e.g. right before starting a stream).
    pub fn flush(&mut self, current: &BTreeMap<String, ViewRequest>) -> ViewDiff {
        let diff = self.diff(current);
        self.sent = current.clone();
        self.pending = None;
        diff
    }

    /// When the UI must wake up to send a pending report.
    pub fn due(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(w: f64) -> ViewRequest {
        ViewRequest {
            tile_w: w,
            tile_h: w / 2.0,
            ..Default::default()
        }
    }

    fn map(items: &[(&str, f64)]) -> BTreeMap<String, ViewRequest> {
        items
            .iter()
            .map(|(k, w)| (k.to_string(), req(*w)))
            .collect()
    }

    #[test]
    fn reports_after_the_delay_and_only_changes() {
        let mut r = ViewReporter::new();
        let t0 = Instant::now();
        let cur = map(&[("A", 640.0), ("B", 640.0)]);
        assert!(r.update(t0, &cur, DEFAULT_DELAY).is_none());
        assert_eq!(r.due(), Some(t0 + DEFAULT_DELAY));
        assert!(
            r.update(t0 + Duration::from_millis(100), &cur, DEFAULT_DELAY)
                .is_none()
        );
        let d = r
            .update(t0 + Duration::from_millis(260), &cur, DEFAULT_DELAY)
            .unwrap();
        assert_eq!(d.len(), 2);
        // Nothing changed: nothing to send.
        assert!(
            r.update(t0 + Duration::from_secs(1), &cur, DEFAULT_DELAY)
                .is_none()
        );
        assert!(r.due().is_none());

        let cur2 = map(&[("A", 960.0)]);
        let t1 = t0 + Duration::from_secs(2);
        assert!(r.update(t1, &cur2, DEFAULT_DELAY).is_none());
        let d = r.update(t1 + DEFAULT_DELAY, &cur2, DEFAULT_DELAY).unwrap();
        assert_eq!(
            d,
            vec![("A".to_string(), Some(req(960.0))), ("B".to_string(), None)]
        );
    }

    #[test]
    fn keeps_debouncing_while_changing_but_not_forever() {
        let mut r = ViewReporter::new();
        let t0 = Instant::now();
        let mut sent = None;
        for i in 0..20u64 {
            let now = t0 + Duration::from_millis(i * 100);
            let cur = map(&[("A", 600.0 + i as f64 * 8.0)]);
            if let Some(d) = r.update(now, &cur, DEFAULT_DELAY) {
                sent = Some((i, d));
                break;
            }
        }
        let (i, d) = sent.expect("sent while still changing");
        assert_eq!(i, 10, "after MAX_DELAY");
        assert_eq!(d[0].1.as_ref().unwrap().tile_w, 680.0);
    }

    #[test]
    fn flush_sends_immediately_and_reverting_cancels() {
        let mut r = ViewReporter::new();
        let t0 = Instant::now();
        let cur = map(&[("A", 640.0)]);
        assert_eq!(r.flush(&cur).len(), 1);
        assert!(r.flush(&cur).is_empty());
        let changed = map(&[("A", 1280.0)]);
        assert!(r.update(t0, &changed, DEFAULT_DELAY).is_none());
        // Back to what was sent before the delay passed: nothing to report.
        assert!(
            r.update(t0 + Duration::from_millis(100), &cur, DEFAULT_DELAY)
                .is_none()
        );
        assert!(r.due().is_none());
    }
}
