use crate::lsp::LspManager;

use super::spawn_fake_client;

#[tokio::test]
async fn shutdown_for_worktree_removes_matching_clients() {
    let mgr = LspManager::new();
    let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
    let wt1_buf = temp.path().join("worktree1");
    let wt2_buf = temp.path().join("worktree2");
    let wt1 = wt1_buf.to_string_lossy().to_string();
    let wt2 = wt2_buf.to_string_lossy().to_string();

    let (k1, c1) = spawn_fake_client(&format!("{wt1}/proj")).await;
    let (k2, c2) = spawn_fake_client(&format!("{wt1}/proj2")).await;
    let (k3, c3) = spawn_fake_client(&format!("{wt2}/proj")).await;

    {
        let mut inner = mgr.inner.lock().await;
        inner.clients.insert(k1, c1);
        inner.clients.insert(k2, c2);
        inner.clients.insert(k3, c3);
    }

    assert_eq!(mgr.inner.lock().await.clients.len(), 3);

    mgr.shutdown_for_worktree(wt1_buf.as_path()).await;

    let remaining: Vec<String> = mgr.inner.lock().await.clients.keys().cloned().collect();
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].contains(&wt2), "only wt2 client should remain");
}

#[tokio::test]
async fn shutdown_for_worktree_noop_on_no_match() {
    let mgr = LspManager::new();
    let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
    let other = temp.path().join("other");
    let nonexistent = temp.path().join("nonexistent");
    let (k, c) = spawn_fake_client(&other.join("proj").to_string_lossy()).await;
    mgr.inner.lock().await.clients.insert(k, c);

    mgr.shutdown_for_worktree(nonexistent.as_path()).await;

    assert_eq!(mgr.inner.lock().await.clients.len(), 1);
}

#[tokio::test]
async fn shutdown_all_kills_all_clients() {
    let mgr = LspManager::new();
    let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
    let wt = temp.path().join("wt");
    let (k1, c1) = spawn_fake_client(&wt.join("proj").to_string_lossy()).await;
    let (k2, c2) = spawn_fake_client(&wt.join("proj2").to_string_lossy()).await;
    {
        let mut inner = mgr.inner.lock().await;
        inner.clients.insert(k1, c1);
        inner.clients.insert(k2, c2);
    }
    assert_eq!(mgr.inner.lock().await.clients.len(), 2);
    mgr.shutdown_all().await;
    assert_eq!(mgr.inner.lock().await.clients.len(), 0);
}

#[tokio::test]
async fn session_end_leaves_no_clients_for_worktree() {
    let mgr = LspManager::new();
    let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
    let worktree = temp.path().join("task-abc").join("worktree");
    let other = temp.path().join("task-xyz").join("worktree");
    let worktree_str = worktree.to_string_lossy().to_string();
    let other_str = other.to_string_lossy().to_string();

    let (k1, c1) = spawn_fake_client(&format!("{worktree_str}/src")).await;
    let (k2, c2) = spawn_fake_client(&format!("{worktree_str}/tests")).await;
    let (k3, c3) = spawn_fake_client(&format!("{other_str}/src")).await;

    {
        let mut inner = mgr.inner.lock().await;
        inner.clients.insert(k1, c1);
        inner.clients.insert(k2, c2);
        inner.clients.insert(k3, c3);
    }

    mgr.shutdown_for_worktree(worktree.as_path()).await;

    let remaining: Vec<String> = mgr.inner.lock().await.clients.keys().cloned().collect();
    assert!(
        remaining.iter().all(|k| !k.contains(&worktree_str)),
        "no clients should remain for the ended session's worktree"
    );
    assert_eq!(
        remaining.len(),
        1,
        "clients for other worktrees must be untouched"
    );
}
