//! Bounded, camera-owned latest-frame cache used by the authenticated frame API.
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::Notify;

pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5);
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CAMERAS: usize = 32;
pub const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    rgb: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Camera/source acquisition time. This is intentionally separate from the
    /// time the detector or HTTP request happens to consume the frame.
    pub captured_at: DateTime<Utc>,
    pub acquired_at: Instant,
    pub generation: u64,
    source_generation: u64,
    pub asset: Arc<Mutex<Option<item_core::assets::RawAssetMetadata>>>,
    // Keeps the allocation charged while any request/freeze still owns this Arc.
    #[allow(dead_code)]
    _reservation: Arc<Reservation>,
}

impl FrameSnapshot {
    pub fn rgb(&self) -> &[u8] {
        self.rgb.as_slice()
    }
}

#[derive(Debug)]
struct Reservation {
    bytes: usize,
    total: Arc<AtomicUsize>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLookupError {
    CameraNotFound,
    CameraOffline,
    FrameUnavailable,
    FrameStale,
}

#[derive(Debug)]
struct CameraSlot {
    state: String,
    generation: u64,
    received: bool,
    frame: Option<Arc<FrameSnapshot>>,
}

#[derive(Clone)]
pub struct FrameCache {
    inner: Arc<Mutex<CacheInner>>,
    notify: Arc<Notify>,
    ttl: Duration,
    charged: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct CacheInner {
    cameras: HashMap<String, CameraSlot>,
    next_generation: u64,
}

impl FrameCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner {
                cameras: HashMap::new(),
                next_generation: 1,
            })),
            notify: Arc::new(Notify::new()),
            ttl: ttl.min(DEFAULT_CACHE_TTL),
            charged: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn register(&self, camera_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "cache lock poisoned")?;
        if !inner.cameras.contains_key(camera_id) && inner.cameras.len() >= MAX_CAMERAS {
            return Err("camera cache capacity exceeded".into());
        }
        inner
            .cameras
            .entry(camera_id.to_string())
            .or_insert(CameraSlot {
                state: "starting".into(),
                generation: 0,
                received: false,
                frame: None,
            });
        Ok(())
    }

    /// Changes source state and invalidates the previous source generation.
    /// The returned token must be supplied by that source when publishing.
    pub fn set_state(&self, camera_id: &str, state: &str) -> u64 {
        let mut generation = 0;
        if let Ok(mut inner) = self.inner.lock() {
            let next = inner.next_generation;
            inner.next_generation = inner.next_generation.wrapping_add(1);
            if let Some(slot) = inner.cameras.get_mut(camera_id) {
                slot.state = state.to_string();
                slot.generation = next;
                generation = next;
                slot.frame.take();
                slot.received = false;
            }
        }
        self.notify.notify_waiters();
        generation
    }

    /// Publish a frame from the currently-open source. `acquired_at` must be
    /// captured immediately after source receipt, before cloning/encoding/detect.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        camera_id: &str,
        source_generation: u64,
        rgb: Vec<u8>,
        width: u32,
        height: u32,
        captured_at: DateTime<Utc>,
        acquired_at: Instant,
    ) -> bool {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(3));
        if expected != Some(rgb.len() as u64)
            || rgb.capacity() > MAX_FRAME_BYTES
            || width == 0
            || height == 0
        {
            return false;
        }
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(slot) = inner.cameras.get(camera_id) else {
            return false;
        };
        if slot.state != "running" || slot.generation != source_generation {
            return false;
        }
        let charge = rgb.capacity();
        let mut current = self.charged.load(Ordering::Acquire);
        loop {
            if current.saturating_add(charge) > MAX_TOTAL_BYTES {
                return false;
            }
            match self.charged.compare_exchange_weak(
                current,
                current + charge,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        let generation = inner.next_generation;
        inner.next_generation = inner.next_generation.wrapping_add(1);
        let reservation = Arc::new(Reservation {
            bytes: charge,
            total: Arc::clone(&self.charged),
        });
        let frame = Arc::new(FrameSnapshot {
            rgb: Arc::new(rgb),
            width,
            height,
            captured_at,
            acquired_at,
            generation,
            source_generation,
            asset: Arc::new(Mutex::new(None)),
            _reservation: reservation,
        });
        if let Some(slot) = inner.cameras.get_mut(camera_id) {
            slot.frame = Some(frame);
            slot.received = true;
        }
        drop(inner);
        self.notify.notify_waiters();
        true
    }

    pub fn purge_expired(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let ttl = self.ttl;
            for slot in inner.cameras.values_mut() {
                if slot
                    .frame
                    .as_ref()
                    .is_some_and(|f| f.acquired_at.elapsed() > ttl)
                {
                    slot.frame.take();
                }
            }
        }
    }

    pub fn clear(&self, camera_id: &str) {
        self.set_state(camera_id, "reconnecting");
    }

    pub fn state(&self, camera_id: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()?
            .cameras
            .get(camera_id)
            .map(|s| s.state.clone())
    }

    pub fn status(&self, camera_id: &str) -> Option<(String, Option<u64>, bool)> {
        let inner = self.inner.lock().ok()?;
        let slot = inner.cameras.get(camera_id)?;
        let age = slot.frame.as_ref().map(|f| f.acquired_at.elapsed());
        Some((
            slot.state.clone(),
            age.map(|a| a.as_millis() as u64),
            slot.state == "running" && age.is_some_and(|a| a <= self.ttl),
        ))
    }

    pub fn known(&self, camera_id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .is_some_and(|i| i.cameras.contains_key(camera_id))
    }

    fn fresh(
        &self,
        camera_id: &str,
        max_age: Duration,
    ) -> Result<Arc<FrameSnapshot>, FrameLookupError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| FrameLookupError::FrameUnavailable)?;
        let slot = inner
            .cameras
            .get(camera_id)
            .ok_or(FrameLookupError::CameraNotFound)?;
        if slot.state != "running" {
            return Err(FrameLookupError::CameraOffline);
        }
        let Some(frame) = slot.frame.clone() else {
            return Err(if slot.received {
                FrameLookupError::FrameStale
            } else {
                FrameLookupError::FrameUnavailable
            });
        };
        if frame.acquired_at.elapsed() > self.ttl || frame.acquired_at.elapsed() > max_age {
            return Err(FrameLookupError::FrameStale);
        }
        Ok(frame)
    }

    pub fn validate_snapshot(
        &self,
        camera_id: &str,
        frame: &FrameSnapshot,
        max_age: Duration,
    ) -> Result<(), FrameLookupError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| FrameLookupError::FrameUnavailable)?;
        let slot = inner
            .cameras
            .get(camera_id)
            .ok_or(FrameLookupError::CameraNotFound)?;
        if slot.state != "running" || slot.generation != frame.source_generation {
            return Err(FrameLookupError::CameraOffline);
        }
        if frame.acquired_at.elapsed() > max_age.min(self.ttl) {
            return Err(FrameLookupError::FrameStale);
        }
        Ok(())
    }

    pub async fn wait_fresh(
        &self,
        camera_id: &str,
        max_age: Duration,
        wait: Duration,
    ) -> Result<Arc<FrameSnapshot>, FrameLookupError> {
        let deadline = Instant::now() + wait;
        loop {
            // Register interest before checking, so a producer notification cannot
            // be lost between the check and awaiting it.
            let notified = self.notify.notified();
            match self.fresh(camera_id, max_age.min(self.ttl)) {
                Ok(frame) => return Ok(frame),
                Err(FrameLookupError::CameraNotFound | FrameLookupError::CameraOffline) => {
                    return self.fresh(camera_id, max_age);
                }
                Err(_) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return self.fresh(camera_id, max_age);
                    }
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return self.fresh(camera_id, max_age);
                    }
                }
            }
        }
    }

    pub fn cache_bytes(&self) -> usize {
        self.charged.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> FrameCache {
        let c = FrameCache::new(Duration::from_millis(20));
        c.register("cam").unwrap();
        c
    }

    #[tokio::test]
    async fn validates_rgb_and_uses_supplied_acquisition_time() {
        let c = cache();
        let generation = c.set_state("cam", "running");
        let acquired = Instant::now() - Duration::from_millis(30);
        assert!(!c.publish("cam", generation, vec![0; 1], 1, 1, Utc::now(), acquired));
        assert!(!c.publish("cam", generation, vec![0; 3], 2, 2, Utc::now(), acquired));
        assert!(c.publish("cam", generation, vec![0; 3], 1, 1, Utc::now(), acquired));
        assert!(matches!(
            c.wait_fresh("cam", Duration::from_secs(1), Duration::ZERO)
                .await,
            Err(FrameLookupError::FrameStale)
        ));
    }

    #[tokio::test]
    async fn old_source_token_cannot_publish_after_reconnect() {
        let c = cache();
        let old = c.set_state("cam", "running");
        c.set_state("cam", "reconnecting");
        let new = c.set_state("cam", "running");
        assert!(!c.publish("cam", old, vec![0; 3], 1, 1, Utc::now(), Instant::now()));
        assert!(c.publish("cam", new, vec![0; 3], 1, 1, Utc::now(), Instant::now()));
    }

    #[tokio::test]
    async fn waiters_get_new_frame_and_idle_expiry_remains_stale() {
        let c = FrameCache::new(Duration::from_millis(100));
        c.register("cam").unwrap();
        let token = c.set_state("cam", "running");
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let cache = c.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .wait_fresh(
                        "cam",
                        Duration::from_millis(100),
                        Duration::from_millis(500),
                    )
                    .await
            }));
        }
        tokio::task::yield_now().await;
        assert!(c.publish("cam", token, vec![0; 3], 1, 1, Utc::now(), Instant::now()));
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
        c.purge_expired();
        assert_eq!(c.cache_bytes(), 0);
        assert!(matches!(
            c.wait_fresh("cam", Duration::from_secs(60), Duration::ZERO)
                .await,
            Err(FrameLookupError::FrameStale)
        ));
        c.set_state("cam", "stopped");
        assert!(matches!(
            c.wait_fresh("cam", Duration::from_secs(60), Duration::ZERO)
                .await,
            Err(FrameLookupError::CameraOffline)
        ));
    }

    #[tokio::test]
    async fn snapshot_revalidation_rejects_disconnect_even_when_frame_is_held() {
        let c = cache();
        let token = c.set_state("cam", "running");
        c.publish("cam", token, vec![0; 3], 1, 1, Utc::now(), Instant::now());
        let held = c
            .wait_fresh("cam", Duration::from_secs(1), Duration::ZERO)
            .await
            .unwrap();
        c.set_state("cam", "reconnecting");
        c.set_state("cam", "running");
        assert_eq!(
            c.validate_snapshot("cam", &held, Duration::from_secs(1)),
            Err(FrameLookupError::CameraOffline)
        );
    }

    #[tokio::test]
    async fn outstanding_snapshot_counts_until_last_arc_drops() {
        let c = cache();
        let token = c.set_state("cam", "running");
        let frame = vec![0; 3];
        assert!(c.publish("cam", token, frame, 1, 1, Utc::now(), Instant::now()));
        let held = c
            .wait_fresh("cam", Duration::from_secs(1), Duration::ZERO)
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(25));
        c.purge_expired();
        assert_eq!(c.cache_bytes(), 3);
        drop(held);
        assert_eq!(c.cache_bytes(), 0);
    }
}
