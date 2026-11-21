//! Events: dated happenings announced in articles the user reads, such as a
//! festival, a sale, or a release. They are public knowledge kept in
//! chronotope with the article as their source; ano keeps which of them each
//! user saved.

/// Tool that saves one event from an article the user shared or reads.
pub const EVENT_SAVE_NAME: &str = "event_save";
/// Tool that finds the events the user saved, by period in their own words.
pub const EVENT_SEARCH_NAME: &str = "event_search";

pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_WHEN_CHARS: usize = 80;
pub const MAX_PLACE_CHARS: usize = 80;
/// The passage of the article the event comes from, kept as its evidence.
pub const MAX_PASSAGE_CHARS: usize = 4000;
pub const MAX_URL_CHARS: usize = 2048;
