//! HTTP request handlers for `/api/v1/*` endpoints.

mod dream;
mod health;
mod learn;
mod migrate;
mod preferences;
mod recall;
mod salience;

pub(crate) use dream::post_dream;
pub(crate) use health::get_health;
pub(crate) use learn::post_learn;
pub(crate) use migrate::post_migrate;
pub(crate) use preferences::get_preferences;
pub(crate) use recall::get_recall;
pub(crate) use salience::get_salience;

// Its only reader is `http::tests`, which is gated to `all(test, unix)`. The
// gate has to match: a `cfg(test)`-only re-export with no reader off Unix is an
// `unused_imports` warning.
#[cfg(all(test, unix))]
pub(crate) use preferences::PREFERENCES_SIZE_CAP;
