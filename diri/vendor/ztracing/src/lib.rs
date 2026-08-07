//! No-op compatibility surface for GPUI's optional profiling annotations.
//!
//! Diri does not enable Zed's `ztracing` cfg or Tracy integration. Keeping the
//! attribute as a no-op preserves that release behavior without pulling the
//! unrelated GPL-licensed logging/profiling implementation into the app.

pub use ztracing_macro::instrument;
