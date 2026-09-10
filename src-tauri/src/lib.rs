use std::{fs, path::PathBuf};

#[tauri::command]
fn read_document(path: PathBuf) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|error| format!("Failed to read {}: {error}", path.display()))
}

#[tauri::command]
fn write_document(path: PathBuf, contents: String) -> Result<(), String> {
    fs::write(&path, contents)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![read_document, write_document])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{read_document, write_document};
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
}
