//! Cursor-paginated event polling exposed as a `futures::Stream`.
//!
//! Two entry points:
//!
//! - [`list_events_page`] — single-page `GET /agents/me/events`; useful for
//!   CLI `events poll --cursor …` invocations that want manual control.
//! - [`stream_events`]    — long-running [`Stream`] of [`AgentEvent`] that
//!   chases `has_more` without sleeping and falls back to `poll_interval`
//!   only when it hits the page tip.
//!
//! # Cursor discipline
//!
//! When the server returns `next_cursor: null`, we've reached the tip.
//! On the next poll we reuse the **last-known cursor** — sending `None`
//! again would risk reprocessing the entire backlog. The server treats
//! the cursor as opaque; the client must too.

use std::collections::VecDeque;
use std::time::Duration;

use futures::stream::unfold;
use futures::Stream;
use taskfast_client::api::types::{AgentEvent, AgentEventListResponse};
use taskfast_client::{map_api_error, Result, TaskFastClient};
use tokio::time::sleep;

/// Default knobs. Page size of 100 matches the server's ceiling for this
/// endpoint; a 5-second sleep at the tip balances liveness vs. quota
/// pressure on long-idle agents.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_PAGE_LIMIT: i64 = 100;

#[derive(Debug, Clone, Copy)]
pub struct PollOptions {
    pub poll_interval: Duration,
    pub page_limit: i64,
}

impl Default for PollOptions {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            page_limit: DEFAULT_PAGE_LIMIT,
        }
    }
}

/// One-shot `GET /agents/me/events?cursor=…&limit=…`.
pub async fn list_events_page(
    client: &TaskFastClient,
    cursor: Option<&str>,
    limit: Option<i64>,
) -> Result<AgentEventListResponse> {
    match client.inner().list_agent_events(cursor, limit).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// Internal state threaded through [`unfold`]. Kept private — callers
/// should treat the returned [`Stream`] as opaque.
struct StreamState {
    cursor: Option<String>,
    buffer: VecDeque<AgentEvent>,
    /// Sleep gate. Flipped on when the last fetch returned
    /// `has_more=false` (tip reached). Starts `false` so the very first
    /// fetch is eager, and stays `false` across multi-page backfills so
    /// we drain as fast as the network allows.
    sleep_before_next_fetch: bool,
}

/// Long-running event stream. The returned [`Stream`] never ends on its
/// own — drop it when the consumer is done. Each [`Result`] item is
/// independent: the stream surfaces transient errors but does not abort,
/// so consumers can `.next()` again to retry.
pub fn stream_events(
    client: &TaskFastClient,
    start_cursor: Option<String>,
    opts: PollOptions,
) -> impl Stream<Item = Result<AgentEvent>> + '_ {
    let state = StreamState {
        cursor: start_cursor,
        buffer: VecDeque::new(),
        sleep_before_next_fetch: false,
    };

    unfold(state, move |mut state| async move {
        loop {
            if let Some(ev) = state.buffer.pop_front() {
                return Some((Ok(ev), state));
            }

            if state.sleep_before_next_fetch {
                sleep(opts.poll_interval).await;
            }

            match list_events_page(client, state.cursor.as_deref(), Some(opts.page_limit)).await {
                Ok(page) => {
                    state.buffer.extend(page.data);
                    // Retain the last-known cursor when the server
                    // signals "no more" with a null next_cursor —
                    // sending None would re-stream old events.
                    state.cursor = page.meta.next_cursor.or(state.cursor);
                    state.sleep_before_next_fetch = !page.meta.has_more;
                    // Loop: either the buffer now has events to yield,
                    // or we just got an empty page and the next turn
                    // of the loop will sleep before fetching again.
                }
                Err(e) => {
                    // Surface the error but keep state intact so the
                    // consumer can retry by calling `.next()` again.
                    // Sleep before the retry to avoid hammering a
                    // failing server.
                    state.sleep_before_next_fetch = true;
                    return Some((Err(e), state));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_poll_options_are_sane() {
        let opts = PollOptions::default();
        assert_eq!(opts.poll_interval, Duration::from_secs(5));
        assert_eq!(opts.page_limit, 100);
    }
}
