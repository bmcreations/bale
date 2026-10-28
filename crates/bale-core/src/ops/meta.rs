//! Record fields only people and adapters set: a tree's origin and its pin.

use anyhow::{bail, Context, Result};

use crate::lock::WorkspaceLock;
use crate::ops::up::resolve;
use crate::registry::{Origin, Record, Registry};
use crate::BaleHome;

/// Records which tool session made the tree. `id_or_path` resolves as for
/// `bale up`, so an unregistered checkout is registered first.
pub fn set_origin(home: &BaleHome, id_or_path: &str, origin: Origin) -> Result<()> {
    update(home, id_or_path, |r| r.origin = Some(origin))
}

/// Pins or unpins the tree; `sweep` never ups or colds a pinned one.
/// `id_or_path` resolves as for `bale up`.
pub fn set_pinned(home: &BaleHome, id_or_path: &str, pinned: bool) -> Result<()> {
    update(home, id_or_path, |r| r.pinned = pinned)
}

/// Rewrites the record under its `WorkspaceLock`, like every record write.
fn update(home: &BaleHome, id_or_path: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    let id = resolve(home, id_or_path)?;
    let Some(_lock) = WorkspaceLock::try_acquire(home, &id)? else {
        bail!("{id} is locked by another bale process; try again");
    };
    let registry = Registry::open(home.clone());
    let mut record = registry
        .get(&id)?
        .with_context(|| format!("record for {id} vanished"))?;
    change(&mut record);
    registry.put(&record)
}
