//! Jackstay's clipboard model: what one copy holds, and the history of copies. No I/O.

mod content;
mod history;

pub use content::{
    Content, ContentError, Format, Kept, MAX_NAME_LEN, NotKept, Target, TargetData, TargetName,
};
pub use history::{Entry, EntryId, History, Limits, Recorded, RestoreError, Source};
