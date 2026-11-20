//! The agenda: plans the user mentions in passing, such as an appointment,
//! a trip, or a deadline, left in their feed as candidates for reminders.
//! The model passes the time in the user's own words; it is read relative to
//! when the user said it, so 「明日」 said last night means today.

/// Tool that leaves a plan the user mentioned as a candidate in their feed.
pub const AGENDA_SUGGEST_NAME: &str = "agenda_suggest";

pub const MAX_TITLE_CHARS: usize = 60;
pub const MAX_WHEN_CHARS: usize = 80;
pub const MAX_QUOTE_CHARS: usize = 300;
