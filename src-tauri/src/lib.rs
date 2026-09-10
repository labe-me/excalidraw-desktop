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

#[cfg(target_os = "macos")]
use tauri::Emitter;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct PendingOpenDocuments(Mutex<Vec<PathBuf>>);

#[derive(Default)]
struct WindowOpenDocuments(Mutex<HashMap<String, PathBuf>>);

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
    fn assign(&self, label: String, path: PathBuf) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?
            .insert(label, path);
        Ok(())
    }

    fn take(&self, label: &str) -> Result<Option<PathBuf>, String> {
        Ok(self
            .0
            .lock()
            .map_err(|_| "Could not access window documents".to_string())?
            .remove(label))
    }
}

fn is_excalidraw_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("excalidraw"))
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

#[tauri::command]
fn read_document(path: PathBuf) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|error| format!("Failed to read {}: {error}", path.display()))
}

#[tauri::command]
fn write_document(path: PathBuf, contents: String) -> Result<(), String> {
    fs::write(&path, contents)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

#[tauri::command]
fn take_pending_open_documents(
    pending: tauri::State<'_, PendingOpenDocuments>,
) -> Result<Vec<PathBuf>, String> {
    pending.take()
}

#[tauri::command]
fn take_window_open_document(
    window: tauri::WebviewWindow,
    documents: tauri::State<'_, WindowOpenDocuments>,
) -> Result<Option<PathBuf>, String> {
    documents.take(window.label())
}

#[tauri::command]
fn open_document_window(
    app: tauri::AppHandle,
    documents: tauri::State<'_, WindowOpenDocuments>,
    path: PathBuf,
) -> Result<(), String> {
    let label = next_window_label(&app, "document");

    documents.assign(label.clone(), path.clone())?;

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
        let _ = documents.take(&label);
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
        .invoke_handler(tauri::generate_handler![
            read_document,
            write_document,
            take_pending_open_documents,
            take_window_open_document,
            open_document_window,
            open_new_window
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, event| {
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
        documents_from_args, read_document, write_document, PendingOpenDocuments,
        WindowOpenDocuments,
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

        write_document(path.clone(), "old contents".into()).expect("initial write should succeed");
        write_document(path.clone(), "updated drawing".into()).expect("overwrite should succeed");

        assert_eq!(
            read_document(path.clone()).expect("saved drawing should be readable"),
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
    fn window_documents_are_taken_by_window_label() {
        let documents = WindowOpenDocuments::default();
        let drawing = PathBuf::from("drawing.excalidraw");

        documents
            .assign("document-1".into(), drawing.clone())
            .expect("a document should be assigned to its window");

        assert_eq!(
            documents
                .take("document-1")
                .expect("the assigned document should be available"),
            Some(drawing)
        );
        assert_eq!(
            documents
                .take("document-1")
                .expect("taking a document twice should be safe"),
            None
        );
    }
}
