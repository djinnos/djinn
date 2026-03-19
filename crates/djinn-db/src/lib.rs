pub mod crypto;
pub mod database;
pub mod error;
pub mod migrations;
pub mod repositories;

pub use database::{Database, default_db_path};
pub use error::{DbError as Error, DbResult as Result};
pub use repositories::{
    epic::{
        EpicCountQuery, EpicCreateInput, EpicListQuery, EpicListResult, EpicRepository,
        EpicTaskCounts, EpicUpdateInput,
    },
    events::EventsRepository,
    git_settings::GitSettingsRepository,
    models::ModelsRepository,
    note::{
        NoteAssociationEntry, NoteDedupCandidate, NoteRepository, UpdateNoteIndexParams, file_path_for,
        folder_for_type, is_singleton, permalink_for, slugify,
    },
    project::{ProjectConfig, ProjectRepository},
    session::{CreateSessionParams, SessionRepository},
    session_message::SessionMessageRepository,
    settings::SettingsRepository,
    task::TaskRepository,
    task::{
        ActivityQuery, BlockerRef, CountQuery, CreateTaskInProjectParams, CreateTaskParams,
        ListQuery, ListResult, ReadyQuery, UpdateTaskParams,
    },
    verification_cache::{CachedVerification, VerificationCacheRepository},
};
