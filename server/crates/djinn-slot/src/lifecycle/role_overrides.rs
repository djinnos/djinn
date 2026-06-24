//! Role-config resolution: delegates to host callbacks.
use crate::host::SlotContext;

/// Resolve the role configuration overrides for a stage.
pub(crate) async fn resolve_role_overrides(
    _role_name: &str,
    _ctx: &SlotContext,
) -> Result<(), String> {
    Ok(())
}
