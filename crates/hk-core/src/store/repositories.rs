//! Tracked Git repositories and the skills found in them.

use super::*;
use crate::manager::DiscoveredSkill;
use crate::repositories::repo_key;

const REPOSITORY_COLUMNS: &str =
    "id, url, name, last_synced_at, last_commit_hash, last_commit_message, last_sync_error";

impl Store {
    pub fn insert_repository(&self, repo: &Repository) -> Result<(), HkError> {
        self.conn.execute(
            "INSERT INTO repositories (id, url, name) VALUES (?1, ?2, ?3)",
            params![repo.id, repo.url, repo.name],
        )?;
        Ok(())
    }

    pub(crate) fn get_repository(&self, id: &str) -> Result<Option<Repository>, HkError> {
        let sql = format!("SELECT {REPOSITORY_COLUMNS} FROM repositories WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], row_to_repository)
            .optional()?)
    }

    pub(crate) fn list_repositories(&self) -> Result<Vec<Repository>, HkError> {
        let sql = format!("SELECT {REPOSITORY_COLUMNS} FROM repositories ORDER BY name, id");
        let mut stmt = self.conn.prepare(&sql)?;
        let repos = stmt
            .query_map([], row_to_repository)?
            .collect::<Result<_, _>>()?;
        Ok(repos)
    }

    pub(crate) fn delete_repository(&self, id: &str) -> Result<(), HkError> {
        self.conn
            .execute("DELETE FROM repositories WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Record a successful sync: the commit the checkout is at and the skills it
    /// holds. Skills that left the repository are dropped; the rest keep
    /// their ignored flag. Skills that appear in a sync after the first are
    /// marked new, until a later sync brings other new ones.
    pub(crate) fn record_repository_sync(
        &self,
        id: &str,
        commit_hash: &str,
        commit_message: &str,
        skills: &[DiscoveredSkill],
    ) -> Result<(), HkError> {
        let tx = self.conn.unchecked_transaction()?;
        let synced_before: bool = tx.query_row(
            "SELECT last_synced_at IS NOT NULL FROM repositories WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        // item_id -> (ignored, is_new) as the last sync left them.
        let before: std::collections::HashMap<String, (bool, bool)> = tx
            .prepare("SELECT item_id, ignored, is_new FROM repository_items WHERE repo_id = ?1")?
            .query_map(params![id], |row| {
                Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
            })?
            .collect::<Result<_, _>>()?;
        let appeared = |skill: &DiscoveredSkill| !before.contains_key(&skill.skill_id);
        let any_appeared = synced_before && skills.iter().any(appeared);
        tx.execute(
            "DELETE FROM repository_items WHERE repo_id = ?1",
            params![id],
        )?;
        for skill in skills {
            let (ignored, was_new) = before.get(&skill.skill_id).copied().unwrap_or_default();
            let is_new = if any_appeared {
                appeared(skill)
            } else {
                was_new
            };
            tx.execute(
                "INSERT INTO repository_items
                   (repo_id, item_id, name, description, ignored, is_new)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    skill.skill_id,
                    skill.name,
                    skill.description,
                    ignored,
                    is_new
                ],
            )?;
        }
        tx.execute(
            "UPDATE repositories
             SET last_synced_at = ?2, last_commit_hash = ?3, last_commit_message = ?4,
                 last_sync_error = NULL
             WHERE id = ?1",
            params![id, Utc::now().to_rfc3339(), commit_hash, commit_message],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a failed sync. The items and commit of the last good sync stay.
    pub(crate) fn record_repository_sync_error(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), HkError> {
        self.conn.execute(
            "UPDATE repositories SET last_sync_error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn set_repository_item_ignored(
        &self,
        repo_id: &str,
        item_id: &str,
        ignored: bool,
    ) -> Result<(), HkError> {
        let updated = self.conn.execute(
            "UPDATE repository_items SET ignored = ?3 WHERE repo_id = ?1 AND item_id = ?2",
            params![repo_id, item_id, ignored],
        )?;
        if updated == 0 {
            return Err(HkError::NotFound(format!(
                "Item '{item_id}' not found in repository '{repo_id}'"
            )));
        }
        Ok(())
    }

    /// Every tracked repository with its items, each item carrying the
    /// installed skills that came from it.
    pub fn list_repositories_with_items(&self) -> Result<Vec<RepositoryWithItems>, HkError> {
        let skills = self.list_extensions(Some(ExtensionKind::Skill), None)?;
        let mut stmt = self.conn.prepare(
            "SELECT item_id, name, description, ignored, is_new FROM repository_items
             WHERE repo_id = ?1 ORDER BY name, item_id",
        )?;
        self.list_repositories()?
            .into_iter()
            .map(|repository| {
                let key = repo_key(&repository.url);
                let from_repo: Vec<&Extension> = skills
                    .iter()
                    .filter(|ext| installed_from(ext, &key))
                    .collect();
                let mut items = stmt
                    .query_map(params![repository.id], |row| {
                        Ok(RepositoryItem {
                            item_id: row.get(0)?,
                            name: row.get(1)?,
                            description: row.get(2)?,
                            ignored: row.get(3)?,
                            is_new: row.get(4)?,
                            installed: Vec::new(),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let item_ids: Vec<String> = items.iter().map(|i| i.item_id.clone()).collect();
                for item in &mut items {
                    item.installed = installed_copies(item, &item_ids, &from_repo);
                }
                let extension_ids = from_repo.iter().map(|ext| ext.id.clone()).collect();
                Ok(RepositoryWithItems {
                    repository,
                    key,
                    extension_ids,
                    items,
                })
            })
            .collect()
    }

    /// Track the repository of every skill that was installed from Git, so the
    /// list is not empty on the first run after an upgrade. Marketplace
    /// installs are left out: they come from repositories the user never
    /// picked as a whole.
    pub fn seed_repositories_from_installed_skills(&self) -> Result<usize, HkError> {
        let mut seen: std::collections::HashSet<String> = self
            .list_repositories()?
            .iter()
            .map(|repo| repo_key(&repo.url))
            .collect();
        // Plain SQL: this runs inside a migration, where the extensions table
        // may not have the columns later versions read.
        let urls: Vec<String> = self
            .conn
            .prepare(
                "SELECT DISTINCT install_url FROM extensions
                 WHERE kind = 'skill' AND install_type = 'git' AND install_url IS NOT NULL
                 ORDER BY install_url",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut added = 0;
        for url in urls {
            let Ok(repo) = crate::repositories::new_repository(&url) else {
                continue;
            };
            if seen.insert(repo_key(&repo.url)) {
                self.insert_repository(&repo)?;
                added += 1;
            }
        }
        Ok(added)
    }

    /// Track untracked repositories whose checkouts already exist under
    /// `cache_root` (`~/.harnesskit/repos/<id>`), preserving each directory's
    /// `<id>`, populating its local skills into `repository_items` (leaving
    /// `last_synced_at = NULL` so the UI refreshes from the remote on visit),
    /// and relinking unattributed installed skills (`install_type IS NULL`)
    /// back to the recovered repository.
    pub fn seed_repositories_from_cache(&self, cache_root: &Path) -> Result<usize, HkError> {
        let Ok(entries) = std::fs::read_dir(cache_root) else {
            return Ok(0);
        };
        let mut seen: std::collections::HashMap<String, String> = self
            .list_repositories()?
            .into_iter()
            .map(|repo| (repo_key(&repo.url), repo.id))
            .collect();
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();

        let mut added = 0;
        for dir in dirs {
            let Some(dir_name) = dir
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty() && !n.starts_with('.'))
                .map(str::to_string)
            else {
                continue;
            };
            let Some(url) = read_origin_url(&dir) else {
                continue;
            };
            let Ok(mut repo) = crate::repositories::new_repository(&url) else {
                continue;
            };
            if let Some(local_path) = repo.url.strip_prefix("file://")
                && !Path::new(local_path).exists()
            {
                continue;
            }
            let key = repo_key(&repo.url);
            let skills = crate::manager::scan_repo_skills(&dir);
            if let Some(existing_id) = seen.get(&key) {
                if existing_id == &dir_name {
                    for skill in &skills {
                        self.conn.execute(
                            "INSERT OR IGNORE INTO repository_items
                               (repo_id, item_id, name, description, ignored, is_new)
                             VALUES (?1, ?2, ?3, ?4, 0, 0)",
                            params![existing_id, skill.skill_id, skill.name, skill.description],
                        )?;
                    }
                    self.relink_unattributed_skills(&repo.url, &skills)?;
                }
                continue;
            }
            seen.insert(key, dir_name.clone());
            repo.id = dir_name;
            self.insert_repository(&repo)?;
            for skill in &skills {
                self.conn.execute(
                    "INSERT OR IGNORE INTO repository_items
                       (repo_id, item_id, name, description, ignored, is_new)
                     VALUES (?1, ?2, ?3, ?4, 0, 0)",
                    params![repo.id, skill.skill_id, skill.name, skill.description],
                )?;
            }
            self.relink_unattributed_skills(&repo.url, &skills)?;
            added += 1;
        }
        Ok(added)
    }

    /// Relink unattributed installed skills (`kind = 'skill' AND install_type IS NULL`)
    /// whose `name` matches `skill.name` or whose skill directory name matches
    /// `skill.skill_id`. Sets `install_type = 'git'`, `install_url`,
    /// `install_subpath`, and backfills `pack`.
    pub fn relink_unattributed_skills(
        &self,
        repo_url: &str,
        skills: &[DiscoveredSkill],
    ) -> Result<usize, HkError> {
        if skills.is_empty() {
            return Ok(0);
        }
        let rows: Vec<(String, String, Option<String>)> = self
            .conn
            .prepare(
                "SELECT id, name, source_path FROM extensions
                 WHERE kind = 'skill' AND install_type IS NULL",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let pack = crate::scanner::extract_pack_from_url(repo_url);
        let mut relinked = 0;
        for (id, ext_name, source_path) in rows {
            let folder_name = source_path
                .as_deref()
                .map(skill_entry_path)
                .and_then(crate::scanner::skill_entry_name);
            let matched = skills
                .iter()
                .find(|s| !s.skill_id.is_empty() && folder_name.as_deref() == Some(&s.skill_id))
                .or_else(|| skills.iter().find(|s| s.name == ext_name));
            let Some(skill) = matched else {
                continue;
            };
            let subpath = (!skill.skill_id.is_empty()).then_some(skill.skill_id.as_str());
            relinked += self.conn.execute(
                "UPDATE extensions
                 SET install_type = 'git',
                     install_url = ?2,
                     install_subpath = ?3,
                     pack = COALESCE(pack, ?4)
                 WHERE id = ?1 AND install_type IS NULL",
                params![id, repo_url, subpath, pack],
            )?;
        }
        Ok(relinked)
    }

    pub fn is_flag_set(&self, key: &str) -> Result<bool, HkError> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM app_state WHERE key = ?1 AND value = '1')",
            params![key],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    pub fn set_flag(&self, key: &str) -> Result<(), HkError> {
        self.conn.execute(
            "INSERT INTO app_state (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            params![key],
        )?;
        Ok(())
    }
}

fn row_to_repository(row: &rusqlite::Row) -> rusqlite::Result<Repository> {
    Ok(Repository {
        id: row.get(0)?,
        url: row.get(1)?,
        name: row.get(2)?,
        last_synced_at: row.get::<_, Option<String>>(3)?.map(parse_dt),
        last_commit_hash: row.get(4)?,
        last_commit_message: row.get(5)?,
        last_sync_error: row.get(6)?,
    })
}

/// Read the URL of `[remote "origin"]` (or the first `url = ` entry if no
/// `[remote "origin"]` header is present) from `<repo_dir>/.git/config`.
fn read_origin_url(repo_dir: &Path) -> Option<String> {
    let config = std::fs::read_to_string(repo_dir.join(".git/config")).ok()?;
    let mut in_origin = false;
    let mut fallback = None;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_origin = trimmed == "[remote \"origin\"]";
            continue;
        }
        if let Some(val) = trimmed.strip_prefix("url") {
            let val = val.trim_start();
            if let Some(url) = val.strip_prefix('=') {
                let url = url.trim();
                if !url.is_empty() {
                    if in_origin {
                        return Some(url.to_string());
                    }
                    if fallback.is_none() {
                        fallback = Some(url.to_string());
                    }
                }
            }
        }
    }
    fallback
}

fn installed_from(ext: &Extension, key: &str) -> bool {
    ext.install_meta
        .as_ref()
        .and_then(|meta| meta.url_resolved.as_deref().or(meta.url.as_deref()))
        .is_some_and(|url| repo_key(url) == key)
}

/// The installed skills that are this item. A Git install records the skill's
/// directory name as its subpath, which names the item exactly. Installs
/// whose subpath names no item of the repository are matched by name.
fn installed_copies(
    item: &RepositoryItem,
    item_ids: &[String],
    from_repo: &[&Extension],
) -> Vec<InstalledCopy> {
    from_repo
        .iter()
        .filter(|ext| {
            let subpath = ext.install_meta.as_ref().and_then(|m| m.subpath.as_deref());
            match subpath {
                Some(subpath) if item_ids.iter().any(|id| id == subpath) => subpath == item.item_id,
                _ => ext.name == item.name,
            }
        })
        .flat_map(|ext| {
            ext.agents.iter().map(|agent| InstalledCopy {
                extension_id: ext.id.clone(),
                agent: agent.clone(),
                scope: ext.scope.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn tracked(store: &Store, url: &str) -> Repository {
        let repo = crate::repositories::new_repository(url).unwrap();
        store.insert_repository(&repo).unwrap();
        repo
    }

    fn skill(id: &str) -> DiscoveredSkill {
        DiscoveredSkill {
            skill_id: id.into(),
            name: id.into(),
            description: String::new(),
            path: format!("skills/{id}"),
        }
    }

    fn installed_skill(name: &str, url: &str, subpath: &str) -> Extension {
        Extension {
            name: name.into(),
            install_meta: Some(InstallMeta {
                install_type: "git".into(),
                url: Some(url.into()),
                url_resolved: None,
                branch: None,
                subpath: Some(subpath.into()),
                revision: None,
                remote_revision: None,
                checked_at: None,
                check_error: None,
            }),
            ..sample_extension()
        }
    }

    #[test]
    fn a_resync_keeps_ignored_flags_drops_gone_skills_and_marks_new_ones() {
        let (store, _dir) = test_store();
        let repo = tracked(&store, "https://github.com/acme/skills");
        let sync = |commit: &str, ids: &[&str]| {
            let skills: Vec<DiscoveredSkill> = ids.iter().map(|id| skill(id)).collect();
            store
                .record_repository_sync(&repo.id, commit, "message", &skills)
                .unwrap();
            let listed = store.list_repositories_with_items().unwrap().remove(0);
            let flags = |item: &RepositoryItem| (item.item_id.clone(), item.ignored, item.is_new);
            listed.items.iter().map(flags).collect::<Vec<_>>()
        };
        let row = |id: &str, ignored, is_new| (id.to_string(), ignored, is_new);

        sync("c1", &["pdf", "docx"]);
        store
            .set_repository_item_ignored(&repo.id, "pdf", true)
            .unwrap();

        let second = sync("c2", &["pdf", "pptx"]);
        assert_eq!(second, [row("pdf", true, false), row("pptx", false, true)]);
        // A sync that brings nothing new leaves the marks alone.
        assert_eq!(sync("c3", &["pdf", "pptx"]), second);
        assert_eq!(
            sync("c4", &["pdf", "pptx", "xlsx"]),
            [
                row("pdf", true, false),
                row("pptx", false, false),
                row("xlsx", false, true)
            ]
        );
    }

    #[test]
    fn an_item_lists_the_copies_installed_from_its_repository_only() {
        let (store, _dir) = test_store();
        let repo = tracked(&store, "https://github.com/acme/skills");
        store
            .record_repository_sync(&repo.id, "c1", "first", &[skill("seo"), skill("geo-seo")])
            .unwrap();
        // The `.git` spelling of the tracked repository.
        let from_repo = installed_skill("seo", "https://github.com/acme/skills.git", "seo");
        // A skill of the same name installed from somewhere else.
        let elsewhere = installed_skill("seo", "https://github.com/other/skills", "seo");
        // Installed from the `geo-seo` directory, though it calls itself `seo`.
        let renamed = installed_skill("seo", "https://github.com/acme/skills", "geo-seo");
        for ext in [&from_repo, &elsewhere, &renamed] {
            store.insert_extension(ext).unwrap();
        }

        let listed = store.list_repositories_with_items().unwrap().remove(0);

        let installed = |item_id: &str| -> Vec<String> {
            let item = listed.items.iter().find(|i| i.item_id == item_id).unwrap();
            item.installed
                .iter()
                .map(|copy| copy.extension_id.clone())
                .collect()
        };
        assert_eq!(installed("seo"), std::slice::from_ref(&from_repo.id));
        assert_eq!(installed("geo-seo"), std::slice::from_ref(&renamed.id));
        assert_eq!(listed.extension_ids.len(), 2);
    }

    #[test]
    fn seeding_tracks_each_installed_git_repository_once() {
        let (store, _dir) = test_store();
        for (name, url) in [
            ("pdf", "https://github.com/acme/skills"),
            ("docx", "https://github.com/acme/skills.git"),
            ("eli5", "git@github.com:someone/eli5.git"),
        ] {
            store
                .insert_extension(&installed_skill(name, url, name))
                .unwrap();
        }
        let mut from_marketplace =
            installed_skill("xlsx", "https://github.com/openai/skills/xlsx", "xlsx");
        from_marketplace.install_meta.as_mut().unwrap().install_type = "marketplace".into();
        store.insert_extension(&from_marketplace).unwrap();
        store.insert_extension(&sample_extension()).unwrap();

        assert_eq!(store.seed_repositories_from_installed_skills().unwrap(), 2);
        assert_eq!(store.seed_repositories_from_installed_skills().unwrap(), 0);

        let names: Vec<String> = store
            .list_repositories()
            .unwrap()
            .into_iter()
            .map(|repo| repo.name)
            .collect();
        assert_eq!(names, ["acme/skills", "someone/eli5"]);
    }

    #[test]
    fn first_scan_auto_seeds_git_repositories_once_and_manual_detect_restores_unfollowed() {
        let (store, _dir) = test_store();
        assert!(!store.is_flag_set("repositories_seeded").unwrap());

        let mut git_skill = sample_extension();
        git_skill.id = "git-pdf".into();
        git_skill.name = "pdf".into();
        git_skill.source = Source {
            origin: SourceOrigin::Git,
            url: Some("https://github.com/acme/skills.git".into()),
            version: None,
            commit_hash: Some("deadbeef".into()),
            from_manifest: false,
        };

        store
            .sync_extensions(std::slice::from_ref(&git_skill))
            .unwrap();
        assert!(store.is_flag_set("repositories_seeded").unwrap());
        let repos = store.list_repositories().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "acme/skills");

        // Unfollow the seeded repository; subsequent scans must not re-add it.
        store.delete_repository(&repos[0].id).unwrap();
        store
            .sync_extensions(std::slice::from_ref(&git_skill))
            .unwrap();
        assert!(store.list_repositories().unwrap().is_empty());

        // Manual detect re-adds it and returns the newly added count.
        let cache = _dir.path().join("repos");
        let store_mutex = parking_lot::Mutex::new(store);
        assert_eq!(
            crate::repositories::detect_repositories(&store_mutex, &cache).unwrap(),
            1
        );
        assert_eq!(
            crate::repositories::detect_repositories(&store_mutex, &cache).unwrap(),
            0
        );
        assert_eq!(store_mutex.lock().list_repositories().unwrap().len(), 1);
    }

    fn write_cached_checkout(cache_root: &Path, id: &str, url: &str, skills: &[(&str, &str)]) {
        let repo_dir = cache_root.join(id);
        let git_dir = repo_dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            format!("[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = {url}\n"),
        )
        .unwrap();
        for (skill_id, skill_name) in skills {
            let dir = if skill_id.is_empty() {
                repo_dir.clone()
            } else {
                repo_dir.join("skills").join(skill_id)
            };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {skill_name}\ndescription: {skill_name} desc\n---\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn seeding_from_cache_recovers_orphaned_checkouts_and_relinks_unattributed_skills() {
        let (store, dir) = test_store();
        let cache = dir.path().join("repos");
        std::fs::create_dir_all(&cache).unwrap();

        // 1. Valid checkout with two skills ("pdf" and folder "geo-seo" with name "seo-helper").
        write_cached_checkout(
            &cache,
            "repo-uuid-1",
            "https://github.com/acme/skills.git",
            &[("pdf", "pdf"), ("geo-seo", "seo-helper")],
        );
        // 2. Duplicate checkout of the same repo under another spelling -> skipped.
        write_cached_checkout(
            &cache,
            "repo-uuid-dup",
            "https://github.com/Acme/Skills",
            &[("pdf", "pdf")],
        );
        // 3. Invalid remote (contains credentials) -> skipped.
        write_cached_checkout(
            &cache,
            "repo-uuid-bad",
            "https://user:secret@github.com/acme/private.git",
            &[("sec", "sec")],
        );
        // 4. Dead local file:// remote -> skipped.
        write_cached_checkout(
            &cache,
            "repo-uuid-dead-file",
            "file:///nonexistent/harnesskit/upstream",
            &[("ghost", "ghost")],
        );
        // 5. Stray lock file -> ignored.
        std::fs::write(cache.join("repo-uuid-1.lock"), "").unwrap();

        // Unattributed installed skills (as scanned after wiping metadata.db).
        let mut unattributed_by_name = sample_extension();
        unattributed_by_name.id = "ext-pdf".into();
        unattributed_by_name.name = "pdf".into();
        unattributed_by_name.source_path = Some("/home/u/.claude/skills/pdf/SKILL.md".into());
        store.insert_extension(&unattributed_by_name).unwrap();

        let mut unattributed_by_folder = sample_extension();
        unattributed_by_folder.id = "ext-geo".into();
        unattributed_by_folder.name = "custom-name".into();
        unattributed_by_folder.source_path = Some("/home/u/.claude/skills/geo-seo/SKILL.md".into());
        store.insert_extension(&unattributed_by_folder).unwrap();

        // Already-attributed skill with same name from another repo -> must NOT be overwritten.
        let already_attributed =
            installed_skill("pdf", "https://github.com/other/pdf-repo", "pdf");
        store.insert_extension(&already_attributed).unwrap();

        assert_eq!(store.seed_repositories_from_cache(&cache).unwrap(), 1);
        assert_eq!(store.seed_repositories_from_cache(&cache).unwrap(), 0);

        let listed = store.list_repositories_with_items().unwrap();
        assert_eq!(listed.len(), 1);
        let recovered = &listed[0];
        assert_eq!(recovered.repository.id, "repo-uuid-1");
        assert_eq!(recovered.repository.name, "acme/skills");
        assert!(recovered.repository.last_synced_at.is_none());

        let item_ids: Vec<&str> = recovered.items.iter().map(|i| i.item_id.as_str()).collect();
        assert_eq!(item_ids, ["pdf", "geo-seo"]);

        let pdf_item = recovered.items.iter().find(|i| i.item_id == "pdf").unwrap();
        assert_eq!(pdf_item.installed.len(), 1);
        assert_eq!(pdf_item.installed[0].extension_id, "ext-pdf");

        let geo_item = recovered
            .items
            .iter()
            .find(|i| i.item_id == "geo-seo")
            .unwrap();
        assert_eq!(geo_item.installed.len(), 1);
        assert_eq!(geo_item.installed[0].extension_id, "ext-geo");

        let ext_pdf = store.get_extension("ext-pdf").unwrap().unwrap();
        assert_eq!(ext_pdf.pack.as_deref(), Some("acme/skills"));
        let meta = ext_pdf.install_meta.unwrap();
        assert_eq!(meta.install_type, "git");
        assert_eq!(meta.subpath.as_deref(), Some("pdf"));

        let preserved = store.get_extension(&already_attributed.id).unwrap().unwrap();
        assert_eq!(
            preserved.install_meta.unwrap().url.as_deref(),
            Some("https://github.com/other/pdf-repo")
        );
    }

    #[test]
    fn first_scan_recovers_cached_repos_and_relinks_unattributed_skills_once() {
        let (store, dir) = test_store();
        let cache = dir.path().join("repos");
        write_cached_checkout(
            &cache,
            "cached-id-42",
            "https://github.com/acme/skills.git",
            &[("pdf", "pdf")],
        );

        let mut scanned_skill = sample_extension();
        scanned_skill.id = "scanned-pdf".into();
        scanned_skill.name = "pdf".into();
        scanned_skill.source_path = Some("/home/u/.claude/skills/pdf/SKILL.md".into());

        store
            .sync_extensions(std::slice::from_ref(&scanned_skill))
            .unwrap();
        assert!(store.is_flag_set("repositories_seeded").unwrap());

        let listed = store.list_repositories_with_items().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].repository.id, "cached-id-42");
        assert_eq!(listed[0].items.len(), 1);
        assert_eq!(listed[0].items[0].installed.len(), 1);
        assert_eq!(listed[0].items[0].installed[0].extension_id, "scanned-pdf");

        // Unfollow; subsequent scan must not re-seed it automatically.
        store.delete_repository("cached-id-42").unwrap();
        store
            .sync_extensions(std::slice::from_ref(&scanned_skill))
            .unwrap();
        assert!(store.list_repositories().unwrap().is_empty());
    }
}
