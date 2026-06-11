/// Recognise the "conversation must be resolved" 405 from a REST
/// `PUT /pulls/{n}/merge` error.
///
/// When a repo enforces branch protection's "A conversation must be resolved
/// before this pull request can be merged" rule and a PR has unresolved
/// review threads, GitHub rejects the direct merge with:
/// `405 Method Not Allowed: {"message":"Repository rule violations found\n\nA
/// conversation must be resolved before this pull request can be merged.\n\n"}`.
///
/// This is neither the merge-queue 405 ([`is_merge_queue_405`]) nor a
/// `mergeable == false` signal, so without this discriminator it falls into
/// the generic "merge failed, retry" arm and loops forever. We match the real
/// payload case-insensitively: a 405 that mentions a conversation needing
/// resolution (also accepting the "Repository rule violations" + "conversation"
/// combination GitHub wraps it in).
pub(in crate::actors::coordinator) fn is_conversation_resolution_block(
    err: &anyhow::Error,
) -> bool {
    let msg = format!("{err}").to_lowercase();
    if !msg.contains("405") {
        return false;
    }
    msg.contains("conversation must be resolved")
        || (msg.contains("repository rule violations") && msg.contains("conversation"))
}
