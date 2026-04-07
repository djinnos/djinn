//! Force cargo to re-run `djinn-db`'s compilation whenever a migration file
//! is added, removed, or modified.
//!
//! `refinery::embed_migrations!("migrations")` is a proc macro that reads the
//! `migrations/` directory at compile time, but cargo has no way to know that
//! without an explicit `rerun-if-changed` hint — proc-macro inputs are not
//! tracked automatically.  Without this build script, adding a new migration
//! file leaves the `.rlib` fingerprint unchanged and cargo happily re-uses
//! the stale build, shipping a binary whose embedded migration list is
//! out of date.
//!
//! See the fix/worktree-target-dir-isolation PR for the incident this
//! prevents: a new `V20260407000002__notes_project_folder_title_idx.sql`
//! was added but never embedded into the running daemon because cargo
//! decided nothing in `djinn-db` had changed.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
