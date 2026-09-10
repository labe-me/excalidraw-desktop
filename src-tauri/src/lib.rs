use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);
const MAX_RECENT_DOCUMENTS: usize = 10;

#[derive(Default)]
struct PendingOpenDocuments(Mutex<Vec<PathBuf>>);

#[derive(Default)]
struct WindowOpenDocuments(Mutex<HashMap<String, PathBuf>>);

enum WriteDocumentOutcome {
    Written,
    OwnedBy(String),
}

struct RecentDocuments {
    storage_path: PathBuf,
    entries: Mutex<Vec<PathBuf>>,
}

impl PendingOpenDocuments {
    fn add(&self, paths: impl IntoIterator<Item = PathBuf>) -> Result<bool, String> {
        let mut pending = self
            .0
            .lock()
            .map_err(|_| "Could not access pending documents".to_string())?;
        let previous_len = pending.len();

        for path in paths
            .into_iter()
            .filter(|path| is_excalidraw_document(path))
        {
            if !pending.contains(&path) {
                pending.push(path);
            }
        }

        Ok(pending.len() > previous_len)
    }

    fn take(&self) -> Result<Vec<PathBuf>, String> {
        let mut pending = self
            .0
            .lock()
            .map_err(|_| "Could not access pending documents".to_string())?;
        Ok(std::mem::take(&mut *pending))
    }
}

impl WindowOpenDocuments {
    fn claim(&self, label: &str, path: PathBuf) -> Result<Option<String>, String> {
        let mut documents = self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?;

        if let Some((owner, _)) = documents
            .iter()
            .find(|(owner, document)| owner.as_str() != label && *document == &path)
        {
            return Ok(Some(owner.clone()));
        }

        documents.insert(label.to_string(), path);
        Ok(None)
    }

    fn get(&self, label: &str) -> Result<Option<PathBuf>, String> {
        Ok(self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?
            .get(label)
            .cloned())
    }

    fn release(&self, label: &str, path: &Path) -> Result<(), String> {
        let mut documents = self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?;

        if documents
            .get(label)
            .is_some_and(|document| document == path)
        {
            documents.remove(label);
        }

        Ok(())
    }

    fn remove(&self, label: &str) -> Result<Option<PathBuf>, String> {
        Ok(self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?
            .remove(label))
    }

    fn write(
        &self,
        label: &str,
        path: &Path,
        contents: &str,
    ) -> Result<WriteDocumentOutcome, String> {
        let document_path = canonical_or_original(path);
        let mut documents = self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?;

        if let Some((owner, _)) = documents
            .iter()
            .find(|(owner, document)| owner.as_str() != label && *document == &document_path)
        {
            return Ok(WriteDocumentOutcome::OwnedBy(owner.clone()));
        }

        write_document_contents(path, contents)?;
        documents.insert(label.to_string(), canonical_or_original(path));
        Ok(WriteDocumentOutcome::Written)
    }
}

impl RecentDocuments {
    fn load(storage_path: PathBuf) -> Self {
        let (entries, should_reset_corrupt_file) = match fs::read(&storage_path) {
            Ok(contents) => match serde_json::from_slice::<Vec<PathBuf>>(&contents) {
                Ok(entries) => (entries, false),
                Err(error) => {
                    eprintln!(
                        "Could not parse recent documents from {}: {error}",
                        storage_path.display()
                    );
                    (Vec::new(), true)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(error) => {
                eprintln!(
                    "Could not read recent documents from {}: {error}",
                    storage_path.display()
                );
                (Vec::new(), false)
            }
        };

        let mut valid_entries: Vec<PathBuf> = Vec::new();
        for path in &entries {
            if is_supported_document(path) && path.is_file() && !valid_entries.contains(path) {
                valid_entries.push(path.clone());
                if valid_entries.len() == MAX_RECENT_DOCUMENTS {
                    break;
                }
            }
        }

        let should_persist_pruned_entries = should_reset_corrupt_file || valid_entries != entries;
        let recent = Self {
            storage_path,
            entries: Mutex::new(valid_entries),
        };

        if should_persist_pruned_entries {
            if let Ok(entries) = recent.snapshot() {
                if let Err(error) = recent.persist(&entries) {
                    eprintln!("{error}");
                }
            }
        }

        recent
    }

    fn snapshot(&self) -> Result<Vec<PathBuf>, String> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| "Could not access recent documents".to_string())?
            .clone())
    }

    fn record(&self, path: PathBuf) -> Result<(Vec<PathBuf>, bool), String> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Could not access recent documents".to_string())?;

        if !is_supported_document(&path) {
            return Ok((entries.clone(), false));
        }

        let already_first = entries.first() == Some(&path);
        entries.retain(|entry| entry != &path);
        entries.insert(0, path);
        entries.truncate(MAX_RECENT_DOCUMENTS);

        self.persist(&entries)?;
        Ok((entries.clone(), !already_first))
    }

    fn remove(&self, path: &Path) -> Result<(Vec<PathBuf>, bool), String> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Could not access recent documents".to_string())?;
        let previous_len = entries.len();
        entries.retain(|entry| entry != path);
        let changed = entries.len() != previous_len;

        if changed {
            self.persist(&entries)?;
        }

        Ok((entries.clone(), changed))
    }

    fn prune_missing(&self) -> Result<(Vec<PathBuf>, bool), String> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Could not access recent documents".to_string())?;
        let previous_len = entries.len();
        entries.retain(|path| path.is_file());
        let changed = entries.len() != previous_len;

        if changed {
            self.persist(&entries)?;
        }

        Ok((entries.clone(), changed))
    }

    fn clear(&self) -> Result<(Vec<PathBuf>, bool), String> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Could not access recent documents".to_string())?;
        let changed = !entries.is_empty();
        entries.clear();

        if changed {
            self.persist(&entries)?;
        }

        Ok((entries.clone(), changed))
    }

    fn persist(&self, entries: &[PathBuf]) -> Result<(), String> {
        if let Some(parent) = self.storage_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "Could not create the recent documents directory {}: {error}",
                    parent.display()
                )
            })?;
        }

        let contents = serde_json::to_vec(entries)
            .map_err(|error| format!("Could not serialize recent documents: {error}"))?;
        fs::write(&self.storage_path, contents).map_err(|error| {
            format!(
                "Could not save recent documents to {}: {error}",
                self.storage_path.display()
            )
        })
    }
}

fn is_excalidraw_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("excalidraw"))
}

fn is_supported_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("excalidraw") || extension.eq_ignore_ascii_case("json")
        })
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn documents_from_args(args: impl IntoIterator<Item = OsString>) -> Vec<PathBuf> {
    args.into_iter()
        .map(PathBuf::from)
        .filter(|path| is_excalidraw_document(path))
        .collect()
}

fn next_window_label(app: &tauri::AppHandle, prefix: &str) -> String {
    loop {
        let id = NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{prefix}-{id}");
        if app.get_webview_window(&candidate).is_none() {
            return candidate;
        }
    }
}

fn focus_document_window(app: &tauri::AppHandle, label: &str) -> Result<bool, String> {
    let Some(window) = app.get_webview_window(label) else {
        return Ok(false);
    };

    window
        .show()
        .and_then(|_| window.unminimize())
        .and_then(|_| window.set_focus())
        .map_err(|error| format!("Could not focus the open drawing: {error}"))?;
    Ok(true)
}

fn claim_document_path(
    app: &tauri::AppHandle,
    documents: &WindowOpenDocuments,
    label: &str,
    path: &Path,
) -> Result<Option<PathBuf>, String> {
    let document_path = path
        .canonicalize()
        .map_err(|error| format!("Failed to access {}: {error}", path.display()))?;

    loop {
        match documents.claim(label, document_path.clone())? {
            Some(owner) if focus_document_window(app, &owner)? => return Ok(None),
            Some(owner) => {
                documents.remove(&owner)?;
            }
            None => return Ok(Some(document_path)),
        }
    }
}

fn publish_recent_documents(
    app: &tauri::AppHandle,
    recent: &RecentDocuments,
    update: Result<(Vec<PathBuf>, bool), String>,
) -> Result<Vec<PathBuf>, String> {
    match update {
        Ok((entries, changed)) => {
            if changed {
                let _ = app.emit("recent-documents-changed", entries.clone());
            }
            Ok(entries)
        }
        Err(error) => {
            eprintln!("{error}");
            let entries = recent.snapshot()?;
            let _ = app.emit("recent-documents-changed", entries.clone());
            Ok(entries)
        }
    }
}

fn record_recent_document(app: &tauri::AppHandle, recent: &RecentDocuments, path: &Path) {
    let _ = publish_recent_documents(app, recent, recent.record(path.to_path_buf()));
}

fn remove_recent_document(app: &tauri::AppHandle, recent: &RecentDocuments, path: &Path) {
    let _ = publish_recent_documents(app, recent, recent.remove(path));
}

fn read_document_contents(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))
}

fn write_document_contents(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

#[tauri::command]
fn read_document(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
    recent: tauri::State<'_, RecentDocuments>,
    path: PathBuf,
) -> Result<String, String> {
    let Some(document_path) = claim_document_path(&app, &documents, window.label(), &path)? else {
        return Err(format!(
            "{} is already open in another window",
            path.display()
        ));
    };

    match read_document_contents(&path) {
        Ok(contents) => {
            record_recent_document(&app, &recent, &path);
            Ok(contents)
        }
        Err(error) => {
            documents.release(window.label(), &document_path)?;
            if !path.is_file() {
                remove_recent_document(&app, &recent, &path);
            }
            Err(error)
        }
    }
}

#[tauri::command]
fn write_document(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
    recent: tauri::State<'_, RecentDocuments>,
    path: PathBuf,
    contents: String,
) -> Result<(), String> {
    loop {
        match documents.write(window.label(), &path, &contents)? {
            WriteDocumentOutcome::Written => {
                record_recent_document(&app, &recent, &path);
                return Ok(());
            }
            WriteDocumentOutcome::OwnedBy(owner) if focus_document_window(&app, &owner)? => {
                return Err(format!(
                    "{} is already open in another window",
                    path.display()
                ));
            }
            WriteDocumentOutcome::OwnedBy(owner) => {
                documents.remove(&owner)?;
            }
        }
    }
}

#[tauri::command]
fn claim_document_window(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
    recent: tauri::State<'_, RecentDocuments>,
    path: PathBuf,
) -> Result<bool, String> {
    let claimed = claim_document_path(&app, &documents, window.label(), &path)?.is_some();
    if !claimed {
        record_recent_document(&app, &recent, &path);
    }
    Ok(claimed)
}

#[tauri::command]
fn release_document_window(
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
    path: PathBuf,
) -> Result<(), String> {
    documents.release(window.label(), &canonical_or_original(&path))
}

#[tauri::command]
fn get_recent_documents(
    app: tauri::AppHandle,
    recent: tauri::State<'_, RecentDocuments>,
) -> Result<Vec<PathBuf>, String> {
    publish_recent_documents(&app, &recent, recent.prune_missing())
}

#[tauri::command]
fn clear_recent_documents(
    app: tauri::AppHandle,
    recent: tauri::State<'_, RecentDocuments>,
) -> Result<(), String> {
    publish_recent_documents(&app, &recent, recent.clear())?;
    Ok(())
}

#[tauri::command]
fn take_pending_open_documents(
    pending: tauri::State<'_, PendingOpenDocuments>,
) -> Result<Vec<PathBuf>, String> {
    pending.take()
}

#[tauri::command]
fn get_window_open_document(
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
) -> Result<Option<PathBuf>, String> {
    documents.get(window.label())
}

#[tauri::command]
fn open_document_window(
    app: tauri::AppHandle,
    documents: tauri::State<'_, WindowOpenDocuments>,
    recent: tauri::State<'_, RecentDocuments>,
    path: PathBuf,
) -> Result<(), String> {
    if !path.is_file() {
        remove_recent_document(&app, &recent, &path);
        return Err(format!("The file {} no longer exists", path.display()));
    }

    let label = next_window_label(&app, "document");
    let Some(_) = claim_document_path(&app, &documents, &label, &path)? else {
        record_recent_document(&app, &recent, &path);
        return Ok(());
    };

    let title = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name} — Excalidraw Desktop"))
        .unwrap_or_else(|| "Excalidraw Desktop".to_string());

    if let Err(error) =
        WebviewWindowBuilder::new(&app, label.clone(), WebviewUrl::App("index.html".into()))
            .title(title)
            .inner_size(1000.0, 700.0)
            .build()
    {
        let _ = documents.remove(&label);
        return Err(format!(
            "Failed to open {} in a new window: {error}",
            path.display()
        ));
    }

    Ok(())
}

#[tauri::command]
fn open_new_window(app: tauri::AppHandle) -> Result<(), String> {
    let label = next_window_label(&app, "new");

    WebviewWindowBuilder::new(&app, label, WebviewUrl::App("index.html".into()))
        .title("Untitled — Excalidraw Desktop")
        .inner_size(1000.0, 700.0)
        .build()
        .map(|_| ())
        .map_err(|error| format!("Failed to open a new window: {error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let pending = PendingOpenDocuments::default();
    pending
        .add(documents_from_args(std::env::args_os().skip(1)))
        .expect("pending document state should be available during startup");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(pending)
        .manage(WindowOpenDocuments::default())
        .setup(|app| {
            let storage_path = app.path().app_data_dir()?.join("recent-documents.json");
            app.manage(RecentDocuments::load(storage_path));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            read_document,
            write_document,
            get_recent_documents,
            clear_recent_documents,
            claim_document_window,
            release_document_window,
            take_pending_open_documents,
            get_window_open_document,
            open_document_window,
            open_new_window
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, event| {
        if let tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Destroyed,
            ..
        } = &event
        {
            let documents = _app_handle.state::<WindowOpenDocuments>();
            if let Err(error) = documents.remove(label) {
                eprintln!("{error}");
            }
        }

        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Opened { urls } = event {
            let paths = urls
                .into_iter()
                .filter_map(|url| url.to_file_path().ok())
                .collect::<Vec<_>>();
            let pending = _app_handle.state::<PendingOpenDocuments>();

            match pending.add(paths) {
                Ok(true) => {
                    let _ = _app_handle.emit("open-documents-requested", ());
                }
                Ok(false) => {}
                Err(error) => eprintln!("{error}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        documents_from_args, read_document_contents, write_document_contents, PendingOpenDocuments,
        RecentDocuments, WindowOpenDocuments, WriteDocumentOutcome, MAX_RECENT_DOCUMENTS,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn write_document_overwrites_an_existing_file() {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "excalidraw-desktop-{}-{unique_suffix}.excalidraw",
            std::process::id()
        ));

        write_document_contents(&path, "old contents").expect("initial write should succeed");
        write_document_contents(&path, "updated drawing").expect("overwrite should succeed");

        assert_eq!(
            read_document_contents(&path).expect("saved drawing should be readable"),
            "updated drawing"
        );

        std::fs::remove_file(path).expect("temporary test file should be removable");
    }

    #[test]
    fn startup_arguments_only_include_excalidraw_documents() {
        let documents = documents_from_args([
            OsString::from("--verbose"),
            OsString::from("drawing.excalidraw"),
            OsString::from("notes.json"),
            OsString::from("SKETCH.EXCALIDRAW"),
        ]);

        assert_eq!(
            documents,
            vec![
                PathBuf::from("drawing.excalidraw"),
                PathBuf::from("SKETCH.EXCALIDRAW")
            ]
        );
    }

    #[test]
    fn pending_documents_are_deduplicated_and_drained() {
        let pending = PendingOpenDocuments::default();
        let drawing = PathBuf::from("drawing.excalidraw");

        assert!(pending
            .add([drawing.clone(), drawing.clone()])
            .expect("documents should be queued"));
        assert_eq!(
            pending.take().expect("documents should be drained"),
            vec![drawing]
        );
        assert!(pending
            .take()
            .expect("the empty queue should still be available")
            .is_empty());
    }

    #[test]
    fn window_documents_retain_ownership_until_released() {
        let documents = WindowOpenDocuments::default();
        let drawing = PathBuf::from("drawing.excalidraw");

        assert_eq!(
            documents
                .claim("document-1", drawing.clone())
                .expect("a document should be claimed"),
            None
        );

        assert_eq!(
            documents
                .get("document-1")
                .expect("the assigned document should be available"),
            Some(drawing.clone())
        );
        assert_eq!(
            documents
                .claim("document-2", drawing.clone())
                .expect("a duplicate claim should be checked"),
            Some("document-1".to_string())
        );
        documents
            .release("document-1", &drawing)
            .expect("the owner should release the document");
        assert_eq!(
            documents
                .claim("document-2", drawing)
                .expect("the released document should be claimable"),
            None
        );
    }

    #[test]
    fn a_document_owned_by_another_window_cannot_be_overwritten() {
        let (directory, _) = recent_documents_fixture("write-ownership");
        let drawing = directory.join("drawing.excalidraw");
        std::fs::write(&drawing, "original").expect("fixture drawing should be written");
        let canonical_drawing = drawing
            .canonicalize()
            .expect("fixture drawing should have a canonical path");
        let documents = WindowOpenDocuments::default();
        documents
            .claim("document-1", canonical_drawing)
            .expect("the first window should claim the drawing");

        match documents
            .write("document-2", &drawing, "overwritten")
            .expect("the duplicate write should be checked")
        {
            WriteDocumentOutcome::OwnedBy(owner) => assert_eq!(owner, "document-1"),
            WriteDocumentOutcome::Written => panic!("the duplicate write must be rejected"),
        }
        assert_eq!(
            std::fs::read_to_string(&drawing).expect("fixture drawing should be readable"),
            "original"
        );

        std::fs::remove_dir_all(directory).expect("fixture directory should be removed");
    }

    fn recent_documents_fixture(name: &str) -> (PathBuf, PathBuf) {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after the Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "excalidraw-desktop-recents-{name}-{}-{unique_suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("fixture directory should be created");
        let storage_path = directory.join("recent-documents.json");
        (directory, storage_path)
    }

    #[test]
    fn recent_documents_are_ordered_deduplicated_and_limited() {
        let (directory, storage_path) = recent_documents_fixture("ordering");
        let recent = RecentDocuments::load(storage_path);
        let mut drawings = Vec::new();

        for index in 0..=MAX_RECENT_DOCUMENTS {
            let drawing = directory.join(format!("drawing-{index}.excalidraw"));
            std::fs::write(&drawing, "{}").expect("fixture drawing should be written");
            recent
                .record(drawing.clone())
                .expect("recent drawing should be recorded");
            drawings.push(drawing);
        }

        let entries = recent
            .snapshot()
            .expect("recent drawings should be readable");
        assert_eq!(entries.len(), MAX_RECENT_DOCUMENTS);
        assert_eq!(entries.first(), drawings.last());
        assert!(!entries.contains(&drawings[0]));

        recent
            .record(drawings[5].clone())
            .expect("existing recent drawing should be promoted");
        let entries = recent
            .snapshot()
            .expect("recent drawings should be readable");
        assert_eq!(entries.first(), Some(&drawings[5]));
        assert_eq!(
            entries.iter().filter(|path| *path == &drawings[5]).count(),
            1
        );

        std::fs::remove_dir_all(directory).expect("fixture directory should be removed");
    }

    #[test]
    fn recent_documents_persist_across_registry_instances() {
        let (directory, storage_path) = recent_documents_fixture("persistence");
        let drawing = directory.join("drawing.json");
        std::fs::write(&drawing, "{}").expect("fixture drawing should be written");

        let recent = RecentDocuments::load(storage_path.clone());
        recent
            .record(drawing.clone())
            .expect("recent drawing should be recorded");
        drop(recent);

        let restored = RecentDocuments::load(storage_path);
        assert_eq!(
            restored.snapshot().expect("recent drawings should load"),
            vec![drawing]
        );

        std::fs::remove_dir_all(directory).expect("fixture directory should be removed");
    }

    #[test]
    fn corrupt_recent_documents_are_reset_to_an_empty_list() {
        let (directory, storage_path) = recent_documents_fixture("corrupt");
        std::fs::write(&storage_path, "not valid JSON")
            .expect("corrupt recent documents should be written");

        let recent = RecentDocuments::load(storage_path.clone());
        assert!(recent
            .snapshot()
            .expect("recent drawings should be readable")
            .is_empty());
        assert_eq!(
            serde_json::from_slice::<Vec<PathBuf>>(
                &std::fs::read(storage_path).expect("reset recent documents should be readable")
            )
            .expect("reset recent documents should contain valid JSON"),
            Vec::<PathBuf>::new()
        );

        std::fs::remove_dir_all(directory).expect("fixture directory should be removed");
    }

    #[test]
    fn recent_documents_prune_missing_files_and_can_be_cleared() {
        let (directory, storage_path) = recent_documents_fixture("prune-clear");
        let existing = directory.join("existing.excalidraw");
        let missing = directory.join("missing.excalidraw");
        std::fs::write(&existing, "{}").expect("fixture drawing should be written");
        std::fs::write(&missing, "{}").expect("fixture drawing should be written");

        let recent = RecentDocuments::load(storage_path.clone());
        recent
            .record(existing.clone())
            .expect("existing drawing should be recorded");
        recent
            .record(missing.clone())
            .expect("soon-to-be-missing drawing should be recorded");
        std::fs::remove_file(missing).expect("fixture drawing should be removed");

        let (entries, changed) = recent
            .prune_missing()
            .expect("missing drawings should be pruned");
        assert!(changed);
        assert_eq!(entries, vec![existing]);

        let (entries, changed) = recent.clear().expect("recent drawings should be cleared");
        assert!(changed);
        assert!(entries.is_empty());
        drop(recent);
        assert!(RecentDocuments::load(storage_path)
            .snapshot()
            .expect("cleared recent drawings should reload")
            .is_empty());

        std::fs::remove_dir_all(directory).expect("fixture directory should be removed");
    }
}
