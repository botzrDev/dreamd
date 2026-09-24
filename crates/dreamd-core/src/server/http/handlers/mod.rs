//! HTTP request handlers for `/api/v1/*` endpoints.

mod dream;
mod health;
mod learn;
mod migrate;
mod preferences;
mod recall;

pub(crate) use dream::post_dream;
pub(crate) use health::get_health;
pub(crate) use learn::post_learn;
pub(crate) use migrate::post_migrate;
pub(crate) use preferences::get_preferences;
pub(crate) use recall::get_recall;

// Its sole consumer is `http::tests`, which AILAB-192 gated to
// `all(test, unix)`. The gate has to match: a `cfg(test)`-only re-export with no
// reader off-target is an `unused_imports` warning, not dead weight the compiler
// forgives.
#[cfg(all(test, unix))]
pub(crate) use preferences::PREFERENCES_SIZE_CAP;
