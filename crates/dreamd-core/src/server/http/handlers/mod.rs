//! HTTP request handlers for `/api/v1/*` endpoints.

mod dream;
mod learn;
mod preferences;
mod recall;

pub(crate) use dream::post_dream;
pub(crate) use learn::post_learn;
pub(crate) use preferences::get_preferences;
pub(crate) use recall::get_recall;

#[cfg(test)]
pub(crate) use preferences::PREFERENCES_SIZE_CAP;
