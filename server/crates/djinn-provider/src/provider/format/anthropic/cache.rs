pub(crate) const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str = "anthropic_cache_breakpoint";
#[cfg(test)]
pub(crate) const ANTHROPIC_STABLE_PREFIX_KIND: &str = "stable_prefix";

/// Anthropic's API accepts at most this many `cache_control` breakpoint markers
/// per request; exceeding it returns a 400. djinn distributes markers across
/// tool definitions, system blocks, and the trailing message breakpoint, none of
/// which individually knows the global count — so we enforce a hard cap over the
/// fully-assembled request body just before it ships.
pub(crate) const MAX_CACHE_CONTROL_MARKERS: usize = 4;
