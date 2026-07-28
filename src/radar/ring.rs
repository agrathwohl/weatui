//! Frame ring buffer and timeline cursor.
//!
//! Frames hold the polar field rather than a rasterised grid, so panning and
//! zooming re-sample the original data instead of stretching pixels.

use crate::radar::ReflectivityField;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone)]
pub struct RadarFrame {
    pub captured_at: DateTime<Utc>,
    pub field: Arc<dyn ReflectivityField>,
    /// NEXRAD is observed-only. Extrapolated frames must be labelled so the
    /// display never implies a forecast it did not receive.
    pub projected: bool,
}

impl std::fmt::Debug for RadarFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RadarFrame")
            .field("captured_at", &self.captured_at)
            .field("site", &self.field.site_id())
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

    /// Frames arriving while the user is scrubbing history must not yank the
    /// view forward, so the cursor only tracks the newest frame when following.
    pub fn push(&mut self, frame: RadarFrame) {
        self.frames.push_back(frame);
        while self.frames.len() > self.capacity {
            self.frames.pop_front();
            self.cursor = self.cursor.saturating_sub(1);
        }
        if self.follow_live {
            self.cursor = self.frames.len().saturating_sub(1);
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

    #[test]
    fn zero_capacity_is_clamped_so_a_frame_can_still_be_held() {
        let mut r = FrameRing::new(0);
        r.push(frame(0));
        assert_eq!(r.len(), 1);
    }
}
