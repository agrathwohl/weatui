//! Frame ring buffer and timeline cursor.
//!
//! Frames hold the polar field rather than a rasterised grid, so panning and
//! zooming re-sample the original data instead of stretching pixels.

use crate::radar::RadarField;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone)]
pub struct RadarFrame {
    pub captured_at: DateTime<Utc>,
    pub field: Arc<dyn RadarField>,
    /// True for HRRR model output, false for observed NEXRAD. The display must
    /// keep these distinguishable so a prediction is never read as a
    /// measurement.
    pub projected: bool,
}

impl std::fmt::Debug for RadarFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RadarFrame")
            .field("captured_at", &self.captured_at)
            .field("site", &self.field.source_label())
            .field("projected", &self.projected)
            .finish()
    }
}

pub struct FrameRing {
    frames: VecDeque<RadarFrame>,
    capacity: usize,
    cursor: usize,
    playing: bool,
    follow_live: bool,
}

impl FrameRing {
    pub fn new(capacity: usize) -> Self {
        FrameRing {
            frames: VecDeque::new(),
            capacity: capacity.max(1),
            cursor: 0,
            playing: true,
            follow_live: true,
        }
    }

    /// Inserted by validity time rather than appended, because observations and
    /// forecasts arrive from independent tasks: a volume fetched after a
    /// forecast batch is still older than it, and appending would put the past
    /// to the right of the future on a time axis.
    ///
    /// Frames arriving while the user is scrubbing history must not yank the
    /// view forward, so the cursor only tracks the newest frame when following.
    pub fn push(&mut self, frame: RadarFrame) {
        // The realtime volume is re-polled while it assembles, so the same
        // captured_at arrives repeatedly with more chunks each time. Replacing
        // keeps the fullest assembly; inserting would stutter playback with
        // duplicates and evict real history.
        if let Some(existing) =
            self.frames.iter_mut().find(|f| f.captured_at == frame.captured_at)
        {
            *existing = frame;
            return;
        }
        let at = self
            .frames
            .iter()
            .position(|f| f.captured_at > frame.captured_at)
            .unwrap_or(self.frames.len());
        self.frames.insert(at, frame);
        if at <= self.cursor && self.cursor + 1 < self.frames.len() {
            self.cursor += 1;
        }

        while self.frames.len() > self.capacity {
            self.frames.pop_front();
            self.cursor = self.cursor.saturating_sub(1);
        }
        if self.follow_live {
            self.cursor = self.frames.len().saturating_sub(1);
        }
    }

    /// Projections are always the newest entries. They must be discarded before
    /// a fresh observation is appended, otherwise last cycle's extrapolation
    /// would sit earlier in the track than data observed after it.
    /// Projected frames are not necessarily contiguous at the back: a fresh
    /// observation can land after an overtaken forecast frame, so popping from
    /// the back would leave stale model output interleaved with real radar.
    pub fn drop_projected(&mut self) {
        let cursor = self.cursor;
        let mut index = 0usize;
        let mut removed_at_or_before_cursor = 0usize;
        self.frames.retain(|f| {
            if f.projected && index <= cursor {
                removed_at_or_before_cursor += 1;
            }
            index += 1;
            !f.projected
        });
        let last = self.frames.len().saturating_sub(1);
        self.cursor = cursor.saturating_sub(removed_at_or_before_cursor).min(last);
        if self.follow_live {
            self.cursor = last;
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn current(&self) -> Option<&RadarFrame> {
        self.frames.get(self.cursor)
    }

    pub fn frames(&self) -> impl Iterator<Item = &RadarFrame> {
        self.frames.iter()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_following_live(&self) -> bool {
        self.follow_live
    }

    pub fn toggle_play(&mut self) {
        self.playing = !self.playing;
    }

    pub fn step_forward(&mut self) {
        if self.frames.is_empty() {
            return;
        }
        let last = self.frames.len() - 1;
        if self.cursor < last {
            self.cursor += 1;
        }
        self.follow_live = self.cursor == last;
    }

    pub fn step_back(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.follow_live = false;
    }

    pub fn jump_oldest(&mut self) {
        self.cursor = 0;
        self.follow_live = self.frames.len() <= 1;
    }

    pub fn jump_newest(&mut self) {
        self.cursor = self.frames.len().saturating_sub(1);
        self.follow_live = true;
    }

    /// Advance during playback, wrapping to the oldest frame at the end so the
    /// loop repeats the way a radar loop is expected to.
    ///
    /// Playback is not scrubbing: `follow_live` tracks whether the cursor sits
    /// on the newest frame, so a single-frame ring stays live rather than
    /// reporting itself as historical after the first wrap.
    pub fn advance_playback(&mut self) {
        if !self.playing || self.frames.is_empty() {
            return;
        }
        self.cursor = if self.cursor + 1 >= self.frames.len() { 0 } else { self.cursor + 1 };
        self.follow_live = self.cursor + 1 == self.frames.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Coords;
    use crate::radar::testing::DiskField;

    fn frame(minute: u32) -> RadarFrame {
        RadarFrame {
            captured_at: DateTime::from_timestamp(1_700_000_000 + minute as i64 * 60, 0)
                .unwrap()
                .to_utc(),
            field: Arc::new(DiskField {
                centre: Coords { lat: 35.0, lon: -97.0 },
                radius_km: 50.0,
                dbz: 40.0,
            }),
            projected: false,
        }
    }

    #[test]
    fn empty_ring_has_no_current_frame() {
        let r = FrameRing::new(4);
        assert!(r.is_empty());
        assert!(r.current().is_none());
    }

    #[test]
    fn ring_evicts_oldest_beyond_capacity() {
        let mut r = FrameRing::new(3);
        for i in 0..5 {
            r.push(frame(i));
        }
        assert_eq!(r.len(), 3);
        assert_eq!(r.frames().next().unwrap().captured_at, frame(2).captured_at);
    }

    #[test]
    fn following_live_tracks_the_newest_frame() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        assert_eq!(r.cursor(), 1);
        assert_eq!(r.current().unwrap().captured_at, frame(1).captured_at);
    }

    #[test]
    fn scrubbing_back_stops_following_and_new_frames_do_not_yank_the_view() {
        let mut r = FrameRing::new(8);
        r.push(frame(0));
        r.push(frame(1));
        r.step_back();
        assert!(!r.is_following_live());
        r.push(frame(2));
        assert_eq!(r.current().unwrap().captured_at, frame(0).captured_at);
    }

    #[test]
    fn eviction_while_scrubbing_keeps_the_cursor_on_the_same_frame() {
        let mut r = FrameRing::new(3);
        r.push(frame(0));
        r.push(frame(1));
        r.push(frame(2));
        r.jump_oldest();
        assert_eq!(r.current().unwrap().captured_at, frame(0).captured_at);
        r.push(frame(3));
        assert_eq!(r.current().unwrap().captured_at, frame(1).captured_at);
    }

    #[test]
    fn step_forward_stops_at_the_newest_and_resumes_following() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        r.jump_oldest();
        r.step_forward();
        assert_eq!(r.cursor(), 1);
        assert!(r.is_following_live());
        r.step_forward();
        assert_eq!(r.cursor(), 1);
    }

    #[test]
    fn step_back_saturates_at_the_oldest_frame() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.step_back();
        r.step_back();
        assert_eq!(r.cursor(), 0);
    }

    #[test]
    fn jump_newest_restores_live_following() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        r.jump_oldest();
        assert!(!r.is_following_live());
        r.jump_newest();
        assert!(r.is_following_live());
        assert_eq!(r.cursor(), 1);
    }

    #[test]
    fn a_single_frame_ring_stays_live_across_playback_wraps() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        assert!(r.is_following_live());
        for _ in 0..5 {
            r.advance_playback();
            assert!(r.is_following_live(), "one frame is always the newest frame");
            assert_eq!(r.cursor(), 0);
        }
    }

    #[test]
    fn playback_reaching_the_newest_frame_reports_live_again() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        r.push(frame(2));
        r.advance_playback();
        assert_eq!(r.cursor(), 0);
        assert!(!r.is_following_live());
        r.advance_playback();
        r.advance_playback();
        assert_eq!(r.cursor(), 2);
        assert!(r.is_following_live());
    }

    #[test]
    fn playback_wraps_around_to_the_oldest_frame() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        r.push(frame(2));
        assert_eq!(r.cursor(), 2);
        r.advance_playback();
        assert_eq!(r.cursor(), 0);
    }

    #[test]
    fn paused_playback_does_not_advance() {
        let mut r = FrameRing::new(4);
        r.push(frame(0));
        r.push(frame(1));
        r.jump_oldest();
        r.toggle_play();
        assert!(!r.is_playing());
        r.advance_playback();
        assert_eq!(r.cursor(), 0);
    }

    /// Regression: a fresh observation can land after an overtaken forecast
    /// frame, so dropping only from the back left stale model output
    /// interleaved with real radar forever.
    #[test]
    fn drop_projected_removes_forecasts_buried_between_observations() {
        let mut r = FrameRing::new(10);
        r.push(frame(0));
        r.push(projected_frame(5));
        r.push(frame(10));
        r.push(projected_frame(15));
        r.push(frame(20));

        r.drop_projected();
        assert_eq!(r.len(), 3);
        assert!(
            r.frames().all(|f| !f.projected),
            "a projected frame survived between observations"
        );
    }

    /// Regression: the realtime volume is re-polled while it assembles, and
    /// each poll used to insert a duplicate frame at the same captured_at,
    /// stuttering playback and evicting real history.
    #[test]
    fn pushing_the_same_captured_at_replaces_rather_than_duplicates() {
        let mut r = FrameRing::new(10);
        r.push(frame(0));
        r.push(frame(5));
        r.push(frame(5));
        r.push(frame(5));
        assert_eq!(r.len(), 2, "re-polling one volume must not grow the ring");
    }

    #[test]
    fn replacement_keeps_the_newer_assembly_of_the_volume() {
        let mut r = FrameRing::new(10);
        let mut early = frame(5);
        early.projected = true;
        r.push(early);
        r.push(frame(5));
        assert_eq!(r.len(), 1);
        assert!(r.frames().all(|f| !f.projected), "the later push must win");
    }

    fn projected_frame(minute: u32) -> RadarFrame {
        RadarFrame { projected: true, ..frame(minute) }
    }

    /// Projections share the ring with observations, so a capacity sized only
    /// to the history count silently evicts that many observed volumes as soon
    /// as projections are appended.
    #[test]
    fn capacity_covering_projections_preserves_every_observed_frame() {
        let history = 6;
        let projections = 3;
        let mut r = FrameRing::new(history + projections);
        for i in 0..history {
            r.push(frame(i as u32));
        }
        for i in 0..projections {
            r.push(projected_frame((history + i) as u32));
        }
        assert_eq!(r.frames().filter(|f| !f.projected).count(), history);
        assert_eq!(r.frames().next().unwrap().captured_at, frame(0).captured_at);
    }

    #[test]
    fn undersized_capacity_would_drop_history_which_is_why_it_is_padded() {
        let mut r = FrameRing::new(6);
        for i in 0..6 {
            r.push(frame(i));
        }
        for i in 0..3 {
            r.push(projected_frame(6 + i));
        }
        assert_eq!(
            r.frames().filter(|f| !f.projected).count(),
            3,
            "documents the eviction that padding the capacity avoids"
        );
    }

    #[test]
    fn dropping_projections_restores_room_for_the_next_observation() {
        let mut r = FrameRing::new(9);
        for i in 0..6 {
            r.push(frame(i));
        }
        for i in 0..3 {
            r.push(projected_frame(6 + i));
        }
        r.drop_projected();
        assert_eq!(r.len(), 6);
        assert!(r.frames().all(|f| !f.projected));
        assert_eq!(r.frames().next().unwrap().captured_at, frame(0).captured_at);
    }

    /// Observations and forecasts come from independent tasks, so a volume can
    /// arrive after a forecast batch while being older than every frame in it.
    #[test]
    fn frames_are_ordered_by_time_regardless_of_arrival_order() {
        let mut r = FrameRing::new(16);
        r.push(projected_frame(100));
        r.push(projected_frame(115));
        r.push(frame(10));
        r.push(frame(5));

        let times: Vec<_> = r.frames().map(|f| f.captured_at).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "not chronological: {times:?}");
        assert_eq!(times[0], frame(5).captured_at);
        assert_eq!(times[3], projected_frame(115).captured_at);
    }

    #[test]
    fn a_late_observation_does_not_land_after_the_forecast() {
        let mut r = FrameRing::new(16);
        for i in 0..3 {
            r.push(frame(i));
        }
        r.push(projected_frame(60));
        r.push(frame(3));

        let kinds: Vec<bool> = r.frames().map(|f| f.projected).collect();
        assert_eq!(kinds, vec![false, false, false, false, true]);
    }

    #[test]
    fn inserting_before_the_cursor_keeps_it_on_the_same_frame() {
        let mut r = FrameRing::new(16);
        r.push(frame(10));
        r.push(frame(20));
        r.jump_oldest();
        let held = r.current().unwrap().captured_at;
        r.push(frame(5));
        assert_eq!(r.current().unwrap().captured_at, held, "cursor must not drift");
    }

    #[test]
    fn dropping_projections_still_works_with_ordered_insertion() {
        let mut r = FrameRing::new(16);
        r.push(projected_frame(90));
        r.push(frame(1));
        r.push(projected_frame(75));
        r.drop_projected();
        assert_eq!(r.len(), 1);
        assert!(r.frames().all(|f| !f.projected));
    }

    #[test]
    fn zero_capacity_is_clamped_so_a_frame_can_still_be_held() {
        let mut r = FrameRing::new(0);
        r.push(frame(0));
        assert_eq!(r.len(), 1);
    }
}
