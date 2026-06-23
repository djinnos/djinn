use std::collections::HashMap;
use std::sync::Arc;

use crate::LspManager;
use crate::client::OpenedFiles;
use crate::diagnostics::DiagnosticsMap;
use tokio::sync::Mutex;

use super::make_diag;

#[tokio::test]
async fn lsp_manager_diagnostics_empty_by_default() {
    let mgr = LspManager::new();
    let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
    assert!(mgr.diagnostics(temp.path()).await.is_empty());
}

#[tokio::test]
async fn lsp_manager_touch_file_no_server_for_txt() {
    let mgr = LspManager::new();
    let tmp = crate::test_helpers::test_tempdir("djinn-lsp-manager-");
    let file = tmp.path().join("test.txt");
    std::fs::write(&file, "hello").unwrap();
    mgr.touch_file(tmp.path(), &file, false).await;
    assert!(mgr.diagnostics(tmp.path()).await.is_empty());
}

#[tokio::test]
async fn diagnostics_xml_notes_indexing_client() {
    use std::sync::atomic::Ordering;

    let mgr = LspManager::new();
    let tmp = crate::test_helpers::test_tempdir("djinn-lsp-indexing-");
    let (key, client) = super::spawn_fake_client(&tmp.path().display().to_string()).await;
    client.ready.store(false, Ordering::SeqCst);
    mgr.inner.lock().await.clients.insert(key, client);

    assert!(mgr.indexing(tmp.path()).await);
    let xml = mgr.diagnostics_xml(tmp.path()).await;
    assert!(xml.contains("still indexing"), "got: {xml}");

    // Once the client reports quiescent, the note disappears.
    for client in mgr.inner.lock().await.clients.values() {
        client.ready.store(true, Ordering::SeqCst);
    }
    assert!(!mgr.indexing(tmp.path()).await);
    assert_eq!(mgr.diagnostics_xml(tmp.path()).await, "");

    mgr.shutdown_all().await;
}

#[tokio::test]
async fn opened_files_tracks_versions() {
    let opened: OpenedFiles = Arc::new(Mutex::new(HashMap::new()));
    let uri = "file:///test.rs".to_string();

    assert!(opened.lock().await.get(&uri).is_none());

    opened.lock().await.insert(uri.clone(), 0);
    assert_eq!(*opened.lock().await.get(&uri).unwrap(), 0);

    let version = *opened.lock().await.get(&uri).unwrap();
    opened.lock().await.insert(uri.clone(), version + 1);
    assert_eq!(*opened.lock().await.get(&uri).unwrap(), 1);

    let version = *opened.lock().await.get(&uri).unwrap();
    opened.lock().await.insert(uri.clone(), version + 1);
    assert_eq!(*opened.lock().await.get(&uri).unwrap(), 2);
}

#[tokio::test]
async fn diagnostics_cleared_before_retouch() {
    let diagnostics: DiagnosticsMap = Arc::new(Mutex::new(HashMap::new()));
    let uri = "file:///test.rs".to_string();

    diagnostics
        .lock()
        .await
        .insert(uri.clone(), vec![make_diag(&uri, 1, 1, 1, "old error")]);
    assert_eq!(diagnostics.lock().await.get(&uri).unwrap().len(), 1);

    crate::diagnostics::clear_uri(&diagnostics, &uri).await;
    assert!(diagnostics.lock().await.get(&uri).is_none());
}
