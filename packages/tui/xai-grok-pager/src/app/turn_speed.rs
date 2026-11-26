//! Purpose: This module measures the output speed of the model for one turn.
//!
//! The meter gives two values. The live value is a moving average of the
//! stream, for the turn status row. The turn value is the average of the full
//! turn, for the "Worked for" marker.
//!
//! Note: The live value is an estimate. The stream carries text, not token
//! counts, so the meter converts characters to tokens with `CHARS_PER_TOKEN`.
//! The turn value uses the token counts that the shell reports.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The live value is the average of the stream across this window.
const LIVE_WINDOW: Duration = Duration::from_secs(3);
/// The meter gives no live value before the measured span has this length.
const MIN_LIVE_SPAN: Duration = Duration::from_millis(300);
/// The readout keeps one live value for this time, to prevent flicker.
const DISPLAY_HOLD: Duration = Duration::from_millis(400);
/// The estimated number of characters in one output token.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Purpose: This struct holds the speed measurements of one agent view.
/// Note: `take_turn_tokens_per_sec` and `reset` clear all the measurements.
#[derive(Debug, Default)]
pub(crate) struct TurnSpeedMeter {
    /// The arrival time and the estimated tokens of each recent stream chunk.
    samples: VecDeque<(Instant, f64)>,
    /// The arrival time of the first chunk after an empty window.
    anchor: Option<Instant>,
    /// The last live value that the readout showed, and its time.
    shown: Option<(Instant, Option<f64>)>,
    /// The output tokens of the completed responses in this turn.
    turn_output_tokens: u64,
    /// The decode time of the completed responses in this turn.
    turn_decode_s: f64,
}

impl TurnSpeedMeter {
    /// Purpose: This function records one live stream chunk.
    /// Input: The number of characters in the chunk. The arrival time.
    /// Note: The first chunk after an empty window only starts the clock. The
    /// model made its tokens before the clock started, so they do not count.
    pub(crate) fn record_stream_chars(&mut self, chars: usize, now: Instant) {
        self.prune(now);
        if self.anchor.is_none() {
            self.anchor = Some(now);
            self.samples.push_back((now, 0.0));
            return;
        }
        self.samples
            .push_back((now, chars as f64 / CHARS_PER_TOKEN));
    }

    /// Purpose: This function adds one completed response to the turn value.
    /// Input: The output tokens and the decode speed that the shell reported.
    /// Note: The function ignores a response that has no valid speed.
    pub(crate) fn record_response(&mut self, output_tokens: u64, tokens_per_sec: f64) {
        if output_tokens == 0 || !tokens_per_sec.is_finite() || tokens_per_sec <= 0.0 {
            return;
        }
        self.turn_output_tokens += output_tokens;
        self.turn_decode_s += output_tokens as f64 / tokens_per_sec;
    }

    /// Purpose: This function gives the live value for the status row.
    /// Input: The current time.
    /// Output: The moving average in tokens per second, or `None` when there
    /// is no live stream.
    /// Note: The function gives the same value for `DISPLAY_HOLD`.
    pub(crate) fn live_tokens_per_sec(&mut self, now: Instant) -> Option<f64> {
        if let Some((shown_at, value)) = self.shown
            && now.duration_since(shown_at) < DISPLAY_HOLD
        {
            return value;
        }
        let value = self.measure_live(now);
        self.shown = Some((now, value));
        value
    }

    /// Purpose: This function gives the turn value and clears the meter.
    /// Output: The total output tokens divided by the total decode time, or
    /// `None` when the turn had no measured response.
    pub(crate) fn take_turn_tokens_per_sec(&mut self) -> Option<f64> {
        let tokens_per_sec =
            (self.turn_decode_s > 0.0).then(|| self.turn_output_tokens as f64 / self.turn_decode_s);
        self.reset();
        tokens_per_sec
    }

    /// Clear all the measurements at a turn boundary.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Purpose: This function calculates the moving average at `now`.
    /// Method:
    ///     1. Remove the samples that are older than `LIVE_WINDOW`.
    ///     2. Start the span at the anchor or at the window start, whichever
    ///        is later.
    ///     3. Divide the tokens in the window by the span.
    fn measure_live(&mut self, now: Instant) -> Option<f64> {
        self.prune(now);
        let anchor = self.anchor?;
        let window_start = now
            .checked_sub(LIVE_WINDOW)
            .map_or(anchor, |start| start.max(anchor));
        let span = now.duration_since(window_start);
        if span < MIN_LIVE_SPAN {
            return None;
        }
        let tokens: f64 = self.samples.iter().map(|(_, tokens)| tokens).sum();
        (tokens > 0.0).then(|| tokens / span.as_secs_f64())
    }

    /// Remove the samples that left the window. An empty window stops the clock.
    fn prune(&mut self, now: Instant) {
        while let Some((at, _)) = self.samples.front()
            && now.duration_since(*at) >= LIVE_WINDOW
        {
            self.samples.pop_front();
        }
        if self.samples.is_empty() {
            self.anchor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn live_value_is_none_without_a_stream() {
        let mut meter = TurnSpeedMeter::default();
        assert_eq!(meter.live_tokens_per_sec(Instant::now()), None);
    }

    #[test]
    fn live_value_averages_the_stream_since_the_first_chunk() {
        let t0 = Instant::now();
        let mut meter = TurnSpeedMeter::default();
        // The first chunk starts the clock. Then 40 chars arrive each 100ms,
        // which is 10 estimated tokens each 100ms.
        meter.record_stream_chars(400, t0);
        for i in 1..=10 {
            meter.record_stream_chars(40, t0 + ms(100 * i));
        }
        let tps = meter.live_tokens_per_sec(t0 + ms(1_000)).unwrap();
        assert!((tps - 100.0).abs() < 1e-6, "got {tps}");
    }

    #[test]
    fn live_value_waits_for_the_minimum_span() {
        let t0 = Instant::now();
        let mut meter = TurnSpeedMeter::default();
        meter.record_stream_chars(40, t0);
        meter.record_stream_chars(40, t0 + ms(50));
        assert_eq!(meter.live_tokens_per_sec(t0 + ms(100)), None);
    }

    #[test]
    fn live_value_follows_the_window_and_ends_after_a_stall() {
        let t0 = Instant::now();
        let mut meter = TurnSpeedMeter::default();
        meter.record_stream_chars(40, t0);
        // 2 seconds at 100 tok/s, then 3 seconds at 20 tok/s.
        for i in 1..=20 {
            meter.record_stream_chars(40, t0 + ms(100 * i));
        }
        for i in 1..=30 {
            meter.record_stream_chars(8, t0 + ms(2_000 + 100 * i));
        }
        // The window now holds only the slow part of the stream.
        let tps = meter.live_tokens_per_sec(t0 + ms(5_050)).unwrap();
        assert!((tps - 20.0).abs() < 1.0, "got {tps}");
        // The stream stops. The window drains, and the value goes away.
        assert_eq!(meter.live_tokens_per_sec(t0 + ms(8_100)), None);
        // A new stream starts a new clock.
        meter.record_stream_chars(40, t0 + ms(9_000));
        meter.record_stream_chars(40, t0 + ms(9_500));
        let tps = meter.live_tokens_per_sec(t0 + ms(9_500)).unwrap();
        assert!((tps - 20.0).abs() < 1e-6, "got {tps}");
    }

    #[test]
    fn live_value_holds_between_refreshes() {
        let t0 = Instant::now();
        let mut meter = TurnSpeedMeter::default();
        meter.record_stream_chars(40, t0);
        meter.record_stream_chars(400, t0 + ms(500));
        let first = meter.live_tokens_per_sec(t0 + ms(500));
        meter.record_stream_chars(4_000, t0 + ms(600));
        assert_eq!(meter.live_tokens_per_sec(t0 + ms(600)), first);
        assert_ne!(meter.live_tokens_per_sec(t0 + ms(1_000)), first);
    }

    #[test]
    fn turn_value_weights_each_response_by_its_decode_time() {
        let mut meter = TurnSpeedMeter::default();
        // 100 tokens in 1 second, then 300 tokens in 6 seconds: 400 tokens in
        // 7 seconds.
        meter.record_response(100, 100.0);
        meter.record_response(300, 50.0);
        let tps = meter.take_turn_tokens_per_sec().unwrap();
        assert!((tps - 400.0 / 7.0).abs() < 1e-6, "got {tps}");
        // The take cleared the meter.
        assert_eq!(meter.take_turn_tokens_per_sec(), None);
    }

    #[test]
    fn turn_value_ignores_responses_without_a_speed() {
        let mut meter = TurnSpeedMeter::default();
        meter.record_response(0, 50.0);
        meter.record_response(100, 0.0);
        meter.record_response(100, f64::NAN);
        assert_eq!(meter.take_turn_tokens_per_sec(), None);
    }
}
