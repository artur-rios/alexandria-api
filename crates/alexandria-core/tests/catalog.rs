#[path = "common/mod.rs"]
mod common;

#[path = "catalog/browse.rs"]
mod browse;

#[path = "catalog/edit_metadata.rs"]
mod edit_metadata;

#[path = "catalog/index.rs"]
mod index;

#[path = "catalog/purge.rs"]
mod purge;

#[path = "catalog/purge_on_disk.rs"]
mod purge_on_disk;

#[path = "catalog/refresh.rs"]
mod refresh;

#[path = "catalog/rename.rs"]
mod rename;

#[path = "catalog/restore.rs"]
mod restore;

#[path = "catalog/soft_delete.rs"]
mod soft_delete;