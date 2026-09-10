import { useCallback, useEffect, useRef, useState } from "react";
import {
  Excalidraw,
  MainMenu,
  MIME_TYPES,
  WelcomeScreen,
  loadFromBlob,
  serializeAsJSON,
} from "@excalidraw/excalidraw";
import { invoke } from "@tauri-apps/api/core";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import "@excalidraw/excalidraw/index.css";
import "./App.css";

window.EXCALIDRAW_ASSET_PATH = "/";

const FILE_FILTERS = [
  {
    name: "Excalidraw drawing",
    extensions: ["excalidraw", "json"],
  },
];

const primaryModifier = /Mac|iPhone|iPad/.test(navigator.userAgent)
  ? "Cmd"
  : "Ctrl";

const getFileName = (path) => path.split(/[\\/]/).pop() || "Untitled.excalidraw";

const getDocumentName = (path) =>
  getFileName(path).replace(/\.(excalidraw|json)$/i, "");

const ensureExcalidrawExtension = (path) =>
  /\.(excalidraw|json)$/i.test(path) ? path : `${path}.excalidraw`;

const loadSession = () => {
  try {
    return {
      appState: JSON.parse(localStorage.getItem("excalidrawState")) || {},
      elements: JSON.parse(localStorage.getItem("excalidrawElements")) || [],
      files: {},
    };
  } catch (error) {
    console.warn("Could not restore the previous Excalidraw session", error);
    return { appState: {}, elements: [], files: {} };
  }
};

function App() {
  const initialSession = useRef(loadSession());
  const [initialData, setInitialData] = useState(initialSession.current);
  const [documentKey, setDocumentKey] = useState(0);
  const [documentName, setDocumentName] = useState(null);
  const excalidrawAPIRef = useRef(null);
  const activeFilePathRef = useRef(null);
  const sceneRef = useRef(initialSession.current);
  const operationInProgressRef = useRef(false);
  const ignoreNextChangeRef = useRef(true);
  const hasUnsavedChangesRef = useRef(
    initialSession.current.elements.length > 0,
  );

  const showError = useCallback((action, error) => {
    console.error(`Could not ${action} the drawing`, error);
    excalidrawAPIRef.current?.setToast({
      message: `Could not ${action} the drawing: ${error?.message || error}`,
      duration: 5000,
      closable: true,
    });
  }, []);

  const persistSession = useCallback((scene = sceneRef.current) => {
    const appState = { ...scene.appState };
    delete appState.collaborators;

    localStorage.setItem("excalidrawState", JSON.stringify(appState));
    localStorage.setItem("excalidrawElements", JSON.stringify(scene.elements));
  }, []);

  useEffect(() => {
    const handleBeforeUnload = () => persistSession();
    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
  }, [persistSession]);

  const openDocument = useCallback(async () => {
    if (operationInProgressRef.current) {
      return;
    }

    operationInProgressRef.current = true;
    try {
      if (
        hasUnsavedChangesRef.current &&
        sceneRef.current.elements.length > 0
      ) {
        const shouldDiscard = await confirm(
          "Opening another drawing will discard your unsaved changes.",
          {
            title: "Open drawing",
            kind: "warning",
            buttons: { ok: "Discard and Open", cancel: "Cancel" },
          },
        );

        if (!shouldDiscard) {
          return;
        }
      }

      const path = await open({
        multiple: false,
        directory: false,
        title: "Open Excalidraw drawing",
        filters: FILE_FILTERS,
      });

      if (!path) {
        return;
      }

      const contents = await invoke("read_document", { path });
      const currentScene = sceneRef.current;
      const loadedScene = await loadFromBlob(
        new Blob([contents], { type: MIME_TYPES.excalidraw }),
        currentScene.appState,
        currentScene.elements,
      );
      const name = getDocumentName(path);
      const nextScene = {
        ...loadedScene,
        appState: { ...loadedScene.appState, name },
        files: loadedScene.files || {},
      };

      sceneRef.current = nextScene;
      excalidrawAPIRef.current = null;
      activeFilePathRef.current = path;
      hasUnsavedChangesRef.current = false;
      ignoreNextChangeRef.current = true;
      setDocumentName(name);
      setInitialData(nextScene);
      setDocumentKey((key) => key + 1);
      document.title = `${getFileName(path)} — Excalidraw Desktop`;
      persistSession(nextScene);
    } catch (error) {
      showError("open", error);
    } finally {
      operationInProgressRef.current = false;
    }
  }, [persistSession, showError]);

  const saveDocument = useCallback(
    async (saveAs = false) => {
      if (operationInProgressRef.current || !excalidrawAPIRef.current) {
        return;
      }

      operationInProgressRef.current = true;
      try {
        const api = excalidrawAPIRef.current;
        let path = activeFilePathRef.current;

        if (saveAs || !path) {
          const suggestedPath = path || `${api.getName()}.excalidraw`;
          path = await save({
            title: saveAs
              ? "Save Excalidraw drawing as"
              : "Save Excalidraw drawing",
            defaultPath: suggestedPath,
            filters: FILE_FILTERS,
          });

          if (!path) {
            return;
          }
          path = ensureExcalidrawExtension(path);
        }

        const name = getDocumentName(path);
        const appState = { ...api.getAppState(), name };
        const contents = serializeAsJSON(
          api.getSceneElementsIncludingDeleted(),
          appState,
          api.getFiles(),
          "local",
        );

        await invoke("write_document", { path, contents });

        activeFilePathRef.current = path;
        setDocumentName(name);
        ignoreNextChangeRef.current = true;
        api.updateScene({ appState: { name } });
        hasUnsavedChangesRef.current = false;
        document.title = `${getFileName(path)} — Excalidraw Desktop`;
        persistSession({
          elements: api.getSceneElementsIncludingDeleted(),
          appState,
          files: api.getFiles(),
        });
        api.setToast({
          message: `Saved to ${getFileName(path)}`,
          duration: 3000,
        });
      } catch (error) {
        showError("save", error);
      } finally {
        operationInProgressRef.current = false;
      }
    },
    [persistSession, showError],
  );

  useEffect(() => {
    const handleKeyDown = (event) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey) {
        return;
      }

      const key = event.key.toLowerCase();
      if (key !== "o" && key !== "s") {
        return;
      }

      event.preventDefault();
      event.stopImmediatePropagation();

      if (key === "o") {
        void openDocument();
      } else {
        void saveDocument(event.shiftKey);
      }
    };

    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [openDocument, saveDocument]);

  const handleChange = useCallback((elements, appState, files) => {
    sceneRef.current = { elements, appState, files };
    if (ignoreNextChangeRef.current) {
      ignoreNextChangeRef.current = false;
    } else {
      hasUnsavedChangesRef.current = true;
    }
  }, []);

  return (
    <main className="container">
      <Excalidraw
        key={documentKey}
        excalidrawAPI={(api) => {
          excalidrawAPIRef.current = api;
        }}
        initialData={initialData}
        name={documentName || undefined}
        onChange={handleChange}
        UIOptions={{
          canvasActions: {
            loadScene: false,
            saveToActiveFile: false,
          },
        }}
      >
        <MainMenu>
          <MainMenu.Item
            onSelect={openDocument}
            shortcut={`${primaryModifier}+O`}
          >
            Open…
          </MainMenu.Item>
          <MainMenu.Item
            onSelect={() => saveDocument(false)}
            shortcut={`${primaryModifier}+S`}
          >
            Save
          </MainMenu.Item>
          <MainMenu.Item
            onSelect={() => saveDocument(true)}
            shortcut={`${primaryModifier}+Shift+S`}
          >
            Save As…
          </MainMenu.Item>
          <MainMenu.DefaultItems.Export />
          <MainMenu.DefaultItems.SaveAsImage />
          <MainMenu.DefaultItems.SearchMenu />
          <MainMenu.DefaultItems.Help />
          <MainMenu.DefaultItems.ClearCanvas />
          <MainMenu.Separator />
          <MainMenu.DefaultItems.ToggleTheme />
          <MainMenu.DefaultItems.ChangeCanvasBackground />
        </MainMenu>

        <WelcomeScreen>
          <WelcomeScreen.Center>
            <WelcomeScreen.Center.Logo />
            <WelcomeScreen.Center.Heading>
              All your data is saved locally on this device.
            </WelcomeScreen.Center.Heading>
            <WelcomeScreen.Center.Menu>
              <WelcomeScreen.Center.MenuItem
                onSelect={openDocument}
                shortcut={`${primaryModifier}+O`}
              >
                Open drawing
              </WelcomeScreen.Center.MenuItem>
              <WelcomeScreen.Center.MenuItemHelp />
            </WelcomeScreen.Center.Menu>
          </WelcomeScreen.Center>
        </WelcomeScreen>
      </Excalidraw>
    </main>
  );
}

export default App;
