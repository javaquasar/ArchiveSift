//! Recoverable, host-owned organization of torrent cards and their local assets.
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Database;
use crate::fs::torrent::{safe_component, torrent_directory};
use crate::models::{TorrentCard, TorrentEdit};
use crate::AppResult;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedFile {
    pub from: String,
    pub to: String,
    pub size: u64,
    sha256: String,
    #[serde(default)]
    retain_source: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TorrentMovePlan {
    pub page_id: i64,
    pub revision: i64,
    pub edit: TorrentEdit,
    pub from_directory: Option<String>,
    pub to_directory: Option<String>,
    pub files: Vec<PlannedFile>,
    base_dir: PathBuf,
    page_url: String,
    image_paths: Vec<(i64, String)>,
    attachment_paths: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct MoveProgress {
    pub completed: usize,
    pub total: usize,
}

pub struct TorrentLibrary {
    database: Database,
    base_dir: PathBuf,
}

impl TorrentLibrary {
    pub fn new(database: Database, base_dir: impl Into<PathBuf>) -> Self {
        Self {
            database,
            base_dir: base_dir.into(),
        }
    }

    pub fn prepare(
        &self,
        page_id: i64,
        revision: i64,
        edit: TorrentEdit,
        organize: bool,
    ) -> AppResult<TorrentMovePlan> {
        self.database.initialize()?;
        let card = self.database.torrent_card(page_id)?;
        let lock = self.database.content_lock(&card.page_url);
        let _guard = lock.blocking_lock();
        let _lease = self
            .database
            .try_content_lease(&card.page_url)?
            .ok_or("this card is being updated by another application instance")?;
        self.recover_page_locked(&card.page_url)?;
        let card = self.database.torrent_card(page_id)?;
        if card.revision != revision {
            return Err("card changed; reload it before editing".into());
        }
        let edit = normalize_edit(edit)?;
        validate_primary_image(&card, &edit)?;
        let organize = organize || edit.primary_studio != card.primary_studio;
        let target = if organize {
            Some(
                torrent_directory(
                    &card.site_key,
                    &edit.primary_studio,
                    card.content_title(),
                    &card.page_url,
                    card.page_id,
                )
                .to_string_lossy()
                .into_owned(),
            )
        } else {
            card.directory.clone()
        };
        let mut plan = TorrentMovePlan {
            page_id,
            revision,
            edit,
            from_directory: card.directory.clone(),
            to_directory: target,
            files: vec![],
            base_dir: self.absolute_root()?,
            page_url: card.page_url.clone(),
            image_paths: vec![],
            attachment_paths: vec![],
        };
        if !organize {
            return Ok(plan);
        }
        let target = plan.to_directory.as_ref().unwrap();
        let mut mapping = HashMap::<String, String>::new();
        // Existing managed directories may also contain user-added sidecar files.
        if let Some(directory) = &card.directory {
            let absolute = checked_path(&plan.base_dir, directory)?;
            if absolute.is_dir() {
                for path in list_files(&absolute)? {
                    let old = path
                        .strip_prefix(&plan.base_dir)?
                        .to_string_lossy()
                        .into_owned();
                    let tail = path.strip_prefix(&absolute)?;
                    mapping.insert(
                        old,
                        Path::new(target).join(tail).to_string_lossy().into_owned(),
                    );
                }
            }
        }
        for image in &card.images {
            if let Some(from) = image.local_file_path.as_deref().filter(|s| !s.is_empty()) {
                let from = portable_path(from)?.to_string_lossy().into_owned();
                let to = mapping.get(&from).cloned().unwrap_or_else(|| {
                    Path::new(target)
                        .join("images")
                        .join(format!(
                            "{}_{}",
                            image.image_id,
                            safe_component(file_name(&from), "image", 100)
                        ))
                        .to_string_lossy()
                        .into_owned()
                });
                mapping.insert(from, to.clone());
                plan.image_paths.push((image.image_id, to));
            }
        }
        for attachment in &card.attachments {
            let from = portable_path(&attachment.local_file_path)?
                .to_string_lossy()
                .into_owned();
            let to = mapping.get(&from).cloned().unwrap_or_else(|| {
                Path::new(target)
                    .join(safe_component(file_name(&from), "attachment", 120))
                    .to_string_lossy()
                    .into_owned()
            });
            mapping.insert(from, to.clone());
            plan.attachment_paths
                .push((attachment.source_url.clone(), to));
        }
        let mut destinations = HashSet::new();
        for (from, to) in mapping {
            if portable_path(&from)? == portable_path(&to)? {
                continue;
            }
            // Case-insensitive collisions are rejected on every OS for portable archives.
            if !destinations.insert(to.replace('\\', "/").to_lowercase()) {
                return Err("two files would have the same destination".into());
            }
            let source = checked_path(&plan.base_dir, &from)?;
            let destination = checked_path(&plan.base_dir, &to)?;
            if destination.try_exists()? {
                return Err("destination already contains a file; nothing was moved".into());
            }
            let (size, sha256) = fingerprint(&source)?;
            let retain_source = self.database.archive_path_is_shared(page_id, &from)?;
            plan.files.push(PlannedFile {
                from,
                to,
                size,
                sha256,
                retain_source,
            });
        }
        plan.files.sort_by(|a, b| a.from.cmp(&b.from));
        if card.directory != plan.to_directory {
            let destination = checked_path(&plan.base_dir, target)?;
            if destination.try_exists()? {
                return Err("destination folder already exists; choose a different studio or resolve the conflict".into());
            }
        }
        Ok(plan)
    }

    pub fn apply(
        &self,
        plan: TorrentMovePlan,
        progress: impl Fn(MoveProgress),
    ) -> AppResult<TorrentCard> {
        let lock = self.database.content_lock(&plan.page_url);
        let _guard = lock.blocking_lock();
        let _lease = self
            .database
            .try_content_lease(&plan.page_url)?
            .ok_or("this card is being updated by another application instance")?;
        self.recover_page_locked(&plan.page_url)?;
        if self.absolute_root()? != plan.base_dir {
            return Err("download folder changed; prepare a new move".into());
        }
        let card = self.database.torrent_card(plan.page_id)?;
        if card.revision != plan.revision {
            return Err("card changed; prepare and confirm a new move".into());
        }
        validate_primary_image(&card, &plan.edit)?;
        if !plan.files.is_empty() {
            if let Some(from) = &plan.from_directory {
                let directory = checked_path(&plan.base_dir, from)?;
                if directory.is_dir() {
                    let actual = list_files(&directory)?.into_iter().collect::<HashSet<_>>();
                    let expected = plan
                        .files
                        .iter()
                        .map(|f| checked_path(&plan.base_dir, &f.from))
                        .collect::<AppResult<Vec<_>>>()?
                        .into_iter()
                        .filter(|p| p.starts_with(&directory))
                        .collect::<HashSet<_>>();
                    if actual != expected {
                        return Err("folder contents changed; prepare a new move".into());
                    }
                }
            }
        }
        for file in &plan.files {
            verify(&checked_path(&plan.base_dir, &file.from)?, file)?;
            if checked_path(&plan.base_dir, &file.to)?.try_exists()? {
                return Err("destination changed; nothing was moved".into());
            }
            if partial_path(&checked_path(&plan.base_dir, &file.to)?).try_exists()? {
                return Err(
                    "a temporary destination file already exists; nothing was moved".into(),
                );
            }
        }
        if plan.from_directory != plan.to_directory {
            if let Some(to) = &plan.to_directory {
                if checked_path(&plan.base_dir, to)?.try_exists()? {
                    return Err("destination folder already exists; nothing was moved".into());
                }
            }
        }
        self.database.begin_torrent_move(
            plan.page_id,
            plan.revision,
            &serde_json::to_string(&plan)?,
        )?;
        let result = self.transfer_and_commit(&plan, &progress);
        if let Err(error) = result {
            // Never report failure after the metadata transaction has committed.
            let committed = self
                .database
                .torrent_journals()?
                .iter()
                .any(|(id, _, done)| *id == plan.page_id && *done);
            if !committed {
                self.rollback(&plan)?;
                self.database.clear_torrent_journal(plan.page_id)?;
                return Err(error);
            }
        }
        // Cleanup is idempotent. Retain the journal if a file is temporarily busy.
        if self.cleanup(&plan).is_ok() {
            self.database.clear_torrent_journal(plan.page_id)?;
        }
        self.database.torrent_card(plan.page_id)
    }

    fn transfer_and_commit(
        &self,
        plan: &TorrentMovePlan,
        progress: &impl Fn(MoveProgress),
    ) -> AppResult<()> {
        progress(MoveProgress {
            completed: 0,
            total: plan.files.len(),
        });
        let renamed = self.rename_directory(plan)?;
        for (index, file) in plan.files.iter().enumerate() {
            let from = checked_path(&plan.base_dir, &file.from)?;
            let to = checked_path(&plan.base_dir, &file.to)?;
            if !renamed {
                if file.retain_source {
                    copy_verified(&from, &to, file)?;
                } else {
                    move_or_copy(&from, &to, file)?;
                }
            }
            verify(&to, file)?;
            progress(MoveProgress {
                completed: index + 1,
                total: plan.files.len(),
            });
        }
        self.database.finish_torrent_move(
            plan.page_id,
            plan.to_directory.as_deref(),
            &plan.edit,
            &plan.image_paths,
            &plan.attachment_paths,
        )
    }

    fn rename_directory(&self, plan: &TorrentMovePlan) -> AppResult<bool> {
        let (Some(from), Some(to)) = (&plan.from_directory, &plan.to_directory) else {
            return Ok(false);
        };
        if from == to || plan.files.is_empty() || plan.files.iter().any(|f| f.retain_source) {
            return Ok(false);
        }
        let from = checked_path(&plan.base_dir, from)?;
        let to = checked_path(&plan.base_dir, to)?;
        if !from.is_dir()
            || plan
                .files
                .iter()
                .any(|f| !checked_path(&plan.base_dir, &f.from).is_ok_and(|p| p.starts_with(&from)))
        {
            return Ok(false);
        }
        // Detect sidecars added after the preview; a new confirmation is required.
        if list_files(&from)?.len() != plan.files.len() {
            return Err("folder contents changed; prepare a new move".into());
        }
        fs::create_dir_all(to.parent().ok_or("destination has no parent")?)?;
        match fs::rename(&from, &to) {
            Ok(()) => Ok(true),
            Err(error) if cross_device(&error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn recover(&self) -> AppResult<()> {
        self.database.initialize()?;
        for (_, payload, _) in self.database.torrent_journals()? {
            let plan: TorrentMovePlan = serde_json::from_str(&payload)?;
            let lock = self.database.content_lock(&plan.page_url);
            let _guard = lock.blocking_lock();
            let Some(_lease) = self.database.try_content_lease(&plan.page_url)? else {
                continue;
            };
            self.recover_page_locked(&plan.page_url)?;
        }
        Ok(())
    }

    pub(crate) fn recover_page_locked(&self, url: &str) -> AppResult<()> {
        for (id, payload, committed) in self.database.torrent_journals()? {
            let plan: TorrentMovePlan = serde_json::from_str(&payload)?;
            if plan.page_url != url {
                continue;
            }
            if self.absolute_root()? != plan.base_dir {
                return Err("an unfinished move belongs to a different download folder; restore that storage setting to recover it".into());
            }
            if committed {
                self.cleanup(&plan)?;
            } else {
                self.rollback(&plan)?;
            }
            self.database.clear_torrent_journal(id)?;
        }
        Ok(())
    }

    fn rollback(&self, plan: &TorrentMovePlan) -> AppResult<()> {
        for file in plan.files.iter().rev() {
            let from = checked_path(&plan.base_dir, &file.from)?;
            let to = checked_path(&plan.base_dir, &file.to)?;
            remove_partial(&plan.base_dir, &file.to)?;
            if from.try_exists()? {
                verify(&from, file)?;
                if to.try_exists()? {
                    verify(&to, file)?;
                    fs::remove_file(&to)?;
                }
            } else {
                verify(&to, file)?;
                move_or_copy(&to, &from, file)?;
                verify(&from, file)?;
                if to.try_exists()? {
                    fs::remove_file(&to)?;
                }
            }
            prune_empty(to.parent(), &plan.base_dir);
        }
        Ok(())
    }

    fn cleanup(&self, plan: &TorrentMovePlan) -> AppResult<()> {
        for file in &plan.files {
            let from = checked_path(&plan.base_dir, &file.from)?;
            let to = checked_path(&plan.base_dir, &file.to)?;
            verify(&to, file)?;
            if from.try_exists()? && !file.retain_source {
                verify(&from, file)?;
                fs::remove_file(&from)?;
            }
            prune_empty(from.parent(), &plan.base_dir);
        }
        Ok(())
    }

    fn absolute_root(&self) -> AppResult<PathBuf> {
        // Previewing a metadata-only edit must also work before the first download.
        if self.base_dir.try_exists()? {
            return Ok(self.base_dir.canonicalize()?);
        }
        let path = if self.base_dir.is_absolute() {
            self.base_dir.clone()
        } else {
            std::env::current_dir()?.join(&self.base_dir)
        };
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("download folder must not contain parent traversal".into());
        }
        Ok(path)
    }
}

fn validate_primary_image(card: &TorrentCard, edit: &TorrentEdit) -> AppResult<()> {
    if edit
        .primary_image_id
        .is_some_and(|id| !card.images.iter().any(|image| image.image_id == id))
    {
        return Err("primary image must belong to this torrent card".into());
    }
    Ok(())
}

fn normalize_edit(mut edit: TorrentEdit) -> AppResult<TorrentEdit> {
    edit.primary_studio = edit.primary_studio.trim().into();
    for values in [&mut edit.studios, &mut edit.tags, &mut edit.categories] {
        let mut seen = HashSet::new();
        *values = values
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .filter(|v| seen.insert(v.to_ascii_lowercase()))
            .map(String::from)
            .collect();
        if values.len() > 200
            || values
                .iter()
                .any(|v| v.chars().count() > 256 || v.chars().any(char::is_control))
        {
            return Err("metadata values must be at most 256 characters (200 per field)".into());
        }
    }
    if edit.primary_studio.chars().count() > 256
        || edit.primary_studio.chars().any(char::is_control)
    {
        return Err("invalid primary studio".into());
    }
    if !edit.primary_studio.is_empty()
        && !edit
            .studios
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&edit.primary_studio))
    {
        edit.studios.push(edit.primary_studio.clone());
    }
    Ok(edit)
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or("file")
}

fn portable_path(value: &str) -> AppResult<PathBuf> {
    let value = value.replace('\\', "/");
    if value.is_empty() || value.contains(':') || value.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err("invalid relative archive path".into());
    }
    let path = PathBuf::from(value);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("archive path must be relative".into());
    }
    Ok(path.components().map(|c| c.as_os_str()).collect())
}

pub(crate) fn checked_path(root: &Path, relative: &str) -> AppResult<PathBuf> {
    let relative = portable_path(relative)?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !current.canonicalize()?.starts_with(root) {
                    return Err(
                        "archive path crosses a symbolic link or leaves the download folder".into(),
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}

fn list_files(directory: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err("card folders must not contain symbolic links".into());
        }
        if kind.is_dir() {
            files.extend(list_files(&entry.path())?);
        } else if kind.is_file() {
            files.push(entry.path());
        } else {
            return Err("unsupported file type in card folder".into());
        }
    }
    Ok(files)
}

fn fingerprint(path: &Path) -> AppResult<(u64, String)> {
    let mut file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err("expected a regular archive file".into());
    }
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((size, format!("{:x}", hash.finalize())))
}

fn verify(path: &Path, file: &PlannedFile) -> AppResult<()> {
    if fingerprint(path)? != (file.size, file.sha256.clone()) {
        return Err("file contents changed; recovery requires the original file".into());
    }
    Ok(())
}

fn cross_device(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(17)
    }
    #[cfg(not(windows))]
    {
        error.raw_os_error() == Some(18)
    }
}

fn move_or_copy(from: &Path, to: &Path, file: &PlannedFile) -> AppResult<()> {
    fs::create_dir_all(to.parent().ok_or("destination has no parent")?)?;
    if to.try_exists()? {
        return Err("refusing to overwrite a destination file".into());
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if cross_device(&error) => copy_verified(from, to, file),
        Err(error) => Err(error.into()),
    }
}

fn copy_verified(from: &Path, to: &Path, file: &PlannedFile) -> AppResult<()> {
    fs::create_dir_all(to.parent().ok_or("destination has no parent")?)?;
    let partial = partial_path(to);
    let mut source = fs::File::open(from)?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let result = (|| -> AppResult<()> {
        std::io::copy(&mut source, &mut destination)?;
        destination.flush()?;
        destination.sync_all()?;
        verify(&partial, file)
    })();
    drop(destination);
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;
    if to.try_exists()? {
        return Err("refusing to overwrite a destination file".into());
    }
    fs::rename(partial, to)?;
    Ok(())
}

fn partial_path(to: &Path) -> PathBuf {
    let hash = format!("{:x}", Sha256::digest(to.to_string_lossy().as_bytes()));
    to.with_file_name(format!(".archivesift-{}.partial", &hash[..24]))
}

fn remove_partial(root: &Path, to: &str) -> AppResult<()> {
    let partial = partial_path(&checked_path(root, to)?);
    let relative = partial.strip_prefix(root)?.to_string_lossy();
    let checked = checked_path(root, &relative)?;
    if checked.try_exists()? {
        fs::remove_file(checked)?;
    }
    Ok(())
}

fn prune_empty(mut directory: Option<&Path>, root: &Path) {
    while let Some(path) = directory.filter(|p| *p != root && p.starts_with(root)) {
        if fs::remove_dir(path).is_err() {
            break;
        }
        directory = path.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetadataAttribute, ScrapedItem};

    fn fixture() -> (tempfile::TempDir, Database, TorrentLibrary, TorrentMovePlan) {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("files");
        fs::create_dir(&base).unwrap();
        let db = Database::new(root.path().join("library.sqlite3"));
        db.initialize().unwrap();
        let page = ScrapedItem {
            site_key: "fixture".into(),
            page_url: "https://example.test/topic?t=7".into(),
            title: "Fixture".into(),
            raw_html: String::new(),
            images: vec![],
            attributes: vec![MetadataAttribute {
                kind: "magnet".into(),
                label: "Magnet".into(),
                value: "magnet:?xt=urn:btih:0123456789012345678901234567890123456789".into(),
                source_url: None,
                is_female: None,
            }],
        };
        let id = db.upsert_scraped_item(&page).unwrap().page_id;
        let card = db.torrent_card(id).unwrap();
        let path = Path::new(card.directory.as_ref().unwrap()).join("release.magnet.txt");
        fs::create_dir_all(base.join(&path).parent().unwrap()).unwrap();
        fs::write(base.join(&path), b"fixture bytes").unwrap();
        db.save_page_attachment(id, "fixture-reference", "Magnet", &path.to_string_lossy())
            .unwrap();
        let service = TorrentLibrary::new(db.clone(), &base);
        let plan = service
            .prepare(
                id,
                card.revision,
                TorrentEdit {
                    primary_studio: "Other studio".into(),
                    ..TorrentEdit::from(&card)
                },
                false,
            )
            .unwrap();
        (root, db, service, plan)
    }
    fn begin(db: &Database, plan: &TorrentMovePlan) {
        db.begin_torrent_move(
            plan.page_id,
            plan.revision,
            &serde_json::to_string(plan).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn legacy_move_journals_without_a_primary_image_are_recoverable() {
        let (_root, db, service, plan) = fixture();
        let mut payload = serde_json::to_value(&plan).unwrap();
        payload["edit"]
            .as_object_mut()
            .unwrap()
            .remove("primary_image_id");
        db.begin_torrent_move(
            plan.page_id,
            plan.revision,
            &serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        service.recover().unwrap();
        assert!(db.torrent_journals().unwrap().is_empty());
        assert_eq!(
            db.torrent_card(plan.page_id).unwrap().primary_image_id,
            None
        );
        for file in plan.files {
            assert!(service.base_dir.join(file.from).is_file());
        }
    }

    #[test]
    fn restart_rolls_back_an_interrupted_directory_rename() {
        let (_root, db, service, plan) = fixture();
        begin(&db, &plan);
        assert!(service.rename_directory(&plan).unwrap());
        assert!(!plan
            .base_dir
            .join(plan.from_directory.as_ref().unwrap())
            .exists());
        TorrentLibrary::new(db.clone(), &service.base_dir)
            .recover()
            .unwrap();
        assert!(plan.base_dir.join(&plan.files[0].from).is_file());
        assert!(!plan.base_dir.join(&plan.files[0].to).exists());
        assert_eq!(db.torrent_card(plan.page_id).unwrap().primary_studio, "");
        assert!(db.torrent_journals().unwrap().is_empty());
    }

    #[test]
    fn recovery_skips_an_operation_still_owned_by_another_instance() {
        let (_root, db, service, plan) = fixture();
        begin(&db, &plan);
        service.rename_directory(&plan).unwrap();
        let lease = db.try_content_lease(&plan.page_url).unwrap().unwrap();
        assert!(db.try_content_lease(&plan.page_url).unwrap().is_none());
        service.recover().unwrap();
        assert_eq!(db.torrent_journals().unwrap().len(), 1);
        assert!(plan.base_dir.join(&plan.files[0].to).is_file());
        drop(lease);
        service.recover().unwrap();
        assert!(db.torrent_journals().unwrap().is_empty());
        assert!(plan.base_dir.join(&plan.files[0].from).is_file());
    }

    #[test]
    fn restart_finishes_cleanup_after_a_committed_cross_volume_copy() {
        let (_root, db, service, plan) = fixture();
        begin(&db, &plan);
        for file in &plan.files {
            copy_verified(
                &plan.base_dir.join(&file.from),
                &plan.base_dir.join(&file.to),
                file,
            )
            .unwrap();
        }
        db.finish_torrent_move(
            plan.page_id,
            plan.to_directory.as_deref(),
            &plan.edit,
            &plan.image_paths,
            &plan.attachment_paths,
        )
        .unwrap();
        service.recover().unwrap();
        service.recover().unwrap();
        assert!(!plan.base_dir.join(&plan.files[0].from).exists());
        assert!(plan.base_dir.join(&plan.files[0].to).is_file());
        assert_eq!(
            db.torrent_card(plan.page_id).unwrap().primary_studio,
            "Other studio"
        );
        assert!(db.torrent_journals().unwrap().is_empty());
    }

    #[test]
    fn restart_discards_an_incomplete_copy_and_preserves_its_original() {
        let (_root, db, service, plan) = fixture();
        begin(&db, &plan);
        let to = plan.base_dir.join(&plan.files[0].to);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        let partial = partial_path(&to);
        fs::write(&partial, b"partial").unwrap();
        service.recover().unwrap();
        assert!(!partial.exists());
        assert!(!to.exists());
        verify(&plan.base_dir.join(&plan.files[0].from), &plan.files[0]).unwrap();
        assert!(db.torrent_journals().unwrap().is_empty());
    }

    #[test]
    fn recovery_preserves_the_journal_if_an_original_was_changed_or_storage_switched() {
        let (_root, db, service, plan) = fixture();
        begin(&db, &plan);
        fs::write(plan.base_dir.join(&plan.files[0].from), b"external edit").unwrap();
        assert!(service.recover().is_err());
        assert_eq!(db.torrent_journals().unwrap().len(), 1);
        let other = tempfile::tempdir().unwrap();
        assert!(TorrentLibrary::new(db.clone(), other.path())
            .recover()
            .is_err());
        fs::write(plan.base_dir.join(&plan.files[0].from), b"fixture bytes").unwrap();
        service.recover().unwrap();
        assert!(db.torrent_journals().unwrap().is_empty());
    }

    #[test]
    fn adding_a_sidecar_after_confirmation_preview_requires_a_new_plan() {
        let (_root, db, service, plan) = fixture();
        fs::write(
            plan.base_dir
                .join(plan.from_directory.as_ref().unwrap())
                .join("new.txt"),
            b"new",
        )
        .unwrap();
        assert!(service.apply(plan.clone(), |_| {}).is_err());
        assert!(db.torrent_journals().unwrap().is_empty());
        assert!(plan.base_dir.join(&plan.files[0].from).is_file());
    }

    #[test]
    fn concurrent_confirmations_are_serialized_for_the_same_card() {
        let (_root, db, service, plan) = fixture();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other_db = db.clone();
        let other_root = service.base_dir.clone();
        let other_plan = plan.clone();
        let other_barrier = barrier.clone();
        let other = std::thread::spawn(move || {
            other_barrier.wait();
            TorrentLibrary::new(other_db, other_root)
                .apply(other_plan, |_| {})
                .is_ok()
        });
        barrier.wait();
        let first = service.apply(plan.clone(), |_| {}).is_ok();
        let second = other.join().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            db.torrent_card(plan.page_id).unwrap().primary_studio,
            "Other studio"
        );
        assert!(plan.base_dir.join(&plan.files[0].to).is_file());
    }

    #[test]
    fn relative_paths_accept_both_separators_and_reject_absolute_paths() {
        assert_eq!(
            portable_path("folder\\studio/file").unwrap(),
            PathBuf::from("folder").join("studio").join("file")
        );
        for path in [
            "../file",
            "a/../../file",
            "C:\\file",
            "/file",
            "\\\\server\\file",
            "a/./b",
            "",
        ] {
            assert!(portable_path(path).is_err(), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_cannot_move_files_outside_the_archive() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        assert!(checked_path(root.path(), "link/file").is_err());
    }
}
