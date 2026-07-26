//! In-process pub/sub for live deployment progress and log lines, plus the
//! ring buffer that backfills a freshly opened view.
//!
//! Deployment output has two consumers with different needs: the database, so a
//! deployment opened tomorrow still shows what happened, and any watcher
//! attached right now. Broadcast channels serve the second without making the
//! first wait.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use nudo_proto::deployment;
use tokio::sync::broadcast;

/// One thing that happened during a deployment.
#[derive(Debug, Clone)]
pub enum DeploymentEvent {
    /// The deployment moved to a new status.
    Status(deployment::Status),
    /// A line of build or deploy output.
    Output { line: String, stderr: bool },
    /// The deployment finished. Sent last, so a watcher knows to stop.
    Finished(deployment::Status),
}

/// A log line read from a target's journal.
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub at: chrono::DateTime<chrono::Utc>,
    pub message: String,
    pub priority: String,
    pub unit: String,
    /// journald cursor, so a dropped connection can resume rather than
    /// re-reading from the start.
    pub cursor: String,
}

/// How many events a slow subscriber may fall behind before it is dropped.
///
/// Bounded on purpose: a browser tab that stops reading must not make the
/// deploy engine buffer without limit. A dropped subscriber reconnects and
/// backfills from the ring buffer.
const CHANNEL_CAPACITY: usize = 512;

/// Live event fan-out.
#[derive(Clone)]
pub struct Bus {
    deployments: Arc<RwLock<HashMap<String, broadcast::Sender<DeploymentEvent>>>>,
    logs: Arc<RwLock<HashMap<String, broadcast::Sender<LogEvent>>>>,
    log_buffers: Arc<RwLock<HashMap<String, VecDeque<LogEvent>>>>,
    /// How many lines to retain per service for backfill.
    buffer_capacity: usize,
}

impl Bus {
    pub fn new(buffer_capacity: usize) -> Self {
        Self {
            deployments: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            log_buffers: Arc::new(RwLock::new(HashMap::new())),
            buffer_capacity: buffer_capacity.max(1),
        }
    }

    /// Subscribes to a deployment's events.
    ///
    /// Subscribing before the deployment produces anything is the normal case —
    /// the dashboard opens the stream as soon as it has the id.
    pub fn watch_deployment(&self, deployment_id: &str) -> broadcast::Receiver<DeploymentEvent> {
        let mut channels = self.deployments.write().expect("bus lock");
        channels
            .entry(deployment_id.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publishes a deployment event. Succeeds whether or not anyone is watching.
    pub fn publish_deployment(&self, deployment_id: &str, event: DeploymentEvent) {
        let sender = {
            let channels = self.deployments.read().expect("bus lock");
            channels.get(deployment_id).cloned()
        };
        if let Some(sender) = sender {
            // An error means every receiver has gone; that is not a failure.
            let _ = sender.send(event);
        }
    }

    /// Drops a finished deployment's channel so the map does not grow without
    /// bound over the process's life.
    pub fn close_deployment(&self, deployment_id: &str) {
        self.deployments
            .write()
            .expect("bus lock")
            .remove(deployment_id);
    }

    /// Subscribes to a service's log stream.
    pub fn watch_logs(&self, service_id: &str) -> broadcast::Receiver<LogEvent> {
        let mut channels = self.logs.write().expect("bus lock");
        channels
            .entry(service_id.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publishes a log line and records it for backfill.
    pub fn publish_log(&self, service_id: &str, event: LogEvent) {
        {
            let mut buffers = self.log_buffers.write().expect("bus lock");
            let buffer = buffers.entry(service_id.to_string()).or_default();
            if buffer.len() >= self.buffer_capacity {
                buffer.pop_front();
            }
            buffer.push_back(event.clone());
        }

        let sender = {
            let channels = self.logs.read().expect("bus lock");
            channels.get(service_id).cloned()
        };
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    /// The most recent buffered lines for a service, oldest first.
    ///
    /// This is what stops a fresh page load from showing an empty log pane while
    /// waiting for the next line to be written.
    pub fn backfill(&self, service_id: &str, limit: usize) -> Vec<LogEvent> {
        let buffers = self.log_buffers.read().expect("bus lock");
        let Some(buffer) = buffers.get(service_id) else {
            return Vec::new();
        };

        let skip = buffer.len().saturating_sub(limit);
        buffer.iter().skip(skip).cloned().collect()
    }

    /// Buffered lines after a given journald cursor, for resuming a dropped
    /// connection without replaying what the client already has.
    pub fn backfill_after_cursor(&self, service_id: &str, cursor: &str) -> Vec<LogEvent> {
        let buffers = self.log_buffers.read().expect("bus lock");
        let Some(buffer) = buffers.get(service_id) else {
            return Vec::new();
        };

        match buffer.iter().position(|event| event.cursor == cursor) {
            Some(index) => buffer.iter().skip(index + 1).cloned().collect(),
            // The cursor has already aged out of the buffer, so the best we can
            // do is give the client everything we still hold rather than
            // silently nothing.
            None => buffer.iter().cloned().collect(),
        }
    }

    /// Whether anything is currently reading a service's logs, so the reader
    /// task can stop tailing when nobody is watching.
    pub fn has_log_watchers(&self, service_id: &str) -> bool {
        self.logs
            .read()
            .expect("bus lock")
            .get(service_id)
            .is_some_and(|sender| sender.receiver_count() > 0)
    }

    /// Drops a service's buffered lines, on delete.
    pub fn forget_service(&self, service_id: &str) {
        self.logs.write().expect("bus lock").remove(service_id);
        self.log_buffers
            .write()
            .expect("bus lock")
            .remove(service_id);
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new(2000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(message: &str, cursor: &str) -> LogEvent {
        LogEvent {
            at: chrono::Utc::now(),
            message: message.to_string(),
            priority: "6".to_string(),
            unit: "bot.service".to_string(),
            cursor: cursor.to_string(),
        }
    }

    #[tokio::test]
    async fn a_watcher_receives_events_published_after_it_subscribes() {
        let bus = Bus::default();
        let mut watcher = bus.watch_deployment("dep_1");

        bus.publish_deployment(
            "dep_1",
            DeploymentEvent::Status(deployment::Status::Building),
        );
        bus.publish_deployment(
            "dep_1",
            DeploymentEvent::Output {
                line: "compiling".to_string(),
                stderr: false,
            },
        );

        assert!(matches!(
            watcher.recv().await.expect("event"),
            DeploymentEvent::Status(deployment::Status::Building)
        ));
        assert!(matches!(
            watcher.recv().await.expect("event"),
            DeploymentEvent::Output { line, stderr: false } if line == "compiling"
        ));
    }

    #[tokio::test]
    async fn publishing_with_nobody_watching_is_not_an_error() {
        // The engine must not care whether a dashboard is open.
        let bus = Bus::default();
        bus.publish_deployment(
            "dep_nobody",
            DeploymentEvent::Status(deployment::Status::Queued),
        );
    }

    #[tokio::test]
    async fn two_watchers_of_one_deployment_both_see_every_event() {
        let bus = Bus::default();
        let mut first = bus.watch_deployment("dep_1");
        let mut second = bus.watch_deployment("dep_1");

        bus.publish_deployment(
            "dep_1",
            DeploymentEvent::Finished(deployment::Status::Succeeded),
        );

        assert!(matches!(
            first.recv().await.expect("event"),
            DeploymentEvent::Finished(deployment::Status::Succeeded)
        ));
        assert!(matches!(
            second.recv().await.expect("event"),
            DeploymentEvent::Finished(deployment::Status::Succeeded)
        ));
    }

    #[tokio::test]
    async fn watchers_of_different_deployments_are_isolated() {
        let bus = Bus::default();
        let mut mine = bus.watch_deployment("dep_mine");
        let _theirs = bus.watch_deployment("dep_theirs");

        bus.publish_deployment(
            "dep_theirs",
            DeploymentEvent::Status(deployment::Status::Failed),
        );
        assert!(
            mine.try_recv().is_err(),
            "events must not cross deployments"
        );
    }

    #[tokio::test]
    async fn closing_a_deployment_releases_its_channel() {
        let bus = Bus::default();
        let mut watcher = bus.watch_deployment("dep_1");
        bus.close_deployment("dep_1");

        // The sender is gone, so the watcher ends rather than hanging.
        assert!(watcher.recv().await.is_err());
    }

    #[tokio::test]
    async fn log_lines_reach_a_live_watcher() {
        let bus = Bus::default();
        let mut watcher = bus.watch_logs("svc_1");

        bus.publish_log("svc_1", log("started", "c1"));
        assert_eq!(watcher.recv().await.expect("event").message, "started");
    }

    #[test]
    fn backfill_returns_recent_lines_oldest_first() {
        // Otherwise a fresh page load shows an empty pane.
        let bus = Bus::default();
        for i in 0..5 {
            bus.publish_log("svc_1", log(&format!("line-{i}"), &format!("c{i}")));
        }

        let recent = bus.backfill("svc_1", 3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].message, "line-2");
        assert_eq!(recent[2].message, "line-4");
    }

    #[test]
    fn backfill_of_an_unknown_service_is_empty_rather_than_an_error() {
        assert!(Bus::default().backfill("svc_nope", 10).is_empty());
    }

    #[test]
    fn asking_for_more_backfill_than_exists_returns_what_there_is() {
        let bus = Bus::default();
        bus.publish_log("svc_1", log("only", "c1"));
        assert_eq!(bus.backfill("svc_1", 100).len(), 1);
    }

    #[test]
    fn the_ring_buffer_drops_the_oldest_lines_past_its_capacity() {
        // A chatty service must not grow the control plane's memory without
        // bound.
        let bus = Bus::new(3);
        for i in 0..10 {
            bus.publish_log("svc_1", log(&format!("line-{i}"), &format!("c{i}")));
        }

        let held = bus.backfill("svc_1", 100);
        assert_eq!(held.len(), 3);
        assert_eq!(held[0].message, "line-7");
        assert_eq!(held[2].message, "line-9");
    }

    #[test]
    fn a_zero_capacity_buffer_still_holds_one_line() {
        // Zero would make backfill useless and is never what a caller means.
        let bus = Bus::new(0);
        bus.publish_log("svc_1", log("kept", "c1"));
        assert_eq!(bus.backfill("svc_1", 10).len(), 1);
    }

    #[test]
    fn resuming_from_a_cursor_returns_only_what_came_after_it() {
        let bus = Bus::default();
        for i in 0..5 {
            bus.publish_log("svc_1", log(&format!("line-{i}"), &format!("c{i}")));
        }

        let resumed = bus.backfill_after_cursor("svc_1", "c2");
        assert_eq!(resumed.len(), 2, "no duplicates of what the client has");
        assert_eq!(resumed[0].message, "line-3");
        assert_eq!(resumed[1].message, "line-4");
    }

    #[test]
    fn resuming_from_the_newest_cursor_returns_nothing_yet() {
        let bus = Bus::default();
        bus.publish_log("svc_1", log("line", "c1"));
        assert!(bus.backfill_after_cursor("svc_1", "c1").is_empty());
    }

    #[test]
    fn a_cursor_that_has_aged_out_falls_back_to_the_whole_buffer() {
        // Better to re-show some lines than to silently show none.
        let bus = Bus::new(3);
        for i in 0..6 {
            bus.publish_log("svc_1", log(&format!("line-{i}"), &format!("c{i}")));
        }

        let resumed = bus.backfill_after_cursor("svc_1", "c0");
        assert_eq!(resumed.len(), 3);
        assert_eq!(resumed[0].message, "line-3");
    }

    #[tokio::test]
    async fn watcher_presence_is_reported_so_tailing_can_stop() {
        let bus = Bus::default();
        assert!(!bus.has_log_watchers("svc_1"));

        let watcher = bus.watch_logs("svc_1");
        assert!(bus.has_log_watchers("svc_1"));

        drop(watcher);
        assert!(
            !bus.has_log_watchers("svc_1"),
            "a journalctl tail should not outlive its last reader"
        );
    }

    #[tokio::test]
    async fn forgetting_a_service_clears_its_buffer_and_ends_its_watchers() {
        let bus = Bus::default();
        let mut watcher = bus.watch_logs("svc_1");
        bus.publish_log("svc_1", log("line", "c1"));

        bus.forget_service("svc_1");

        assert!(bus.backfill("svc_1", 10).is_empty());
        // Drain what was already queued, then the channel must end.
        while watcher.try_recv().is_ok() {}
        assert!(watcher.recv().await.is_err());
    }

    #[test]
    fn buffers_are_kept_per_service() {
        let bus = Bus::default();
        bus.publish_log("svc_a", log("a", "c1"));
        bus.publish_log("svc_b", log("b", "c1"));

        assert_eq!(bus.backfill("svc_a", 10)[0].message, "a");
        assert_eq!(bus.backfill("svc_b", 10)[0].message, "b");
    }

    #[tokio::test]
    async fn a_subscriber_that_falls_too_far_behind_is_dropped_rather_than_buffered_forever() {
        let bus = Bus::default();
        let mut slow = bus.watch_deployment("dep_1");

        // Overrun the channel without reading.
        for i in 0..(CHANNEL_CAPACITY + 10) {
            bus.publish_deployment(
                "dep_1",
                DeploymentEvent::Output {
                    line: format!("line-{i}"),
                    stderr: false,
                },
            );
        }

        // The receiver is told it lagged instead of the sender blocking.
        assert!(matches!(
            slow.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
