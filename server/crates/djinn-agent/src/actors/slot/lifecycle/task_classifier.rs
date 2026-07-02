//! Agent facade for the canonical native-skill task classifier.

pub(crate) use djinn_slot::lifecycle::task_classifier::{
    NativeSkillTrigger, classify_native_skill_trigger, classify_native_skill_trigger_by_type,
};
