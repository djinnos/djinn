use super::classify_destructive_shell_command as classify;
use super::{ShellDestructiveClass, ShellDestructiveDecision};

// ── Helpers ──────────────────────────────────────────────────────────────

fn assert_allow(cmd: &str) {
    assert_eq!(
        classify(cmd),
        ShellDestructiveDecision::Allow,
        "expected Allow for: {cmd}"
    );
}

fn assert_hard_deny(cmd: &str) {
    assert!(
        matches!(classify(cmd), ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for: {cmd}, got {:?}",
        classify(cmd),
    );
}

fn assert_soft_gate_file(cmd: &str) {
    let decision = classify(cmd);
    assert!(
        matches!(
            decision,
            ShellDestructiveDecision::SoftGate {
                class: ShellDestructiveClass::WorktreeLocalFileMutation,
                ..
            }
        ),
        "expected SoftGate(WorktreeLocalFileMutation) for: {cmd}, got {decision:?}",
    );
}

fn assert_hard_deny_reason_contains(cmd: &str, needle: &str) {
    let decision = classify(cmd);
    match &decision {
        ShellDestructiveDecision::HardDeny { reason } => {
            assert!(
                reason.contains(needle),
                "expected HardDeny reason to contain '{needle}' for: {cmd}, got '{reason}'",
            );
        }
        _ => panic!("expected HardDeny for: {cmd}, got {decision:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  AC 2: Hard-deny categories
// ═══════════════════════════════════════════════════════════════════════════

// ── Git hard reset/clean/stash ───────────────────────────────────────────

#[test]
fn hard_deny_git_reset_hard() {
    assert_hard_deny("git reset --hard HEAD~1");
}

#[test]
fn hard_deny_git_reset_hard_no_ref() {
    assert_hard_deny("git reset --hard");
}

#[test]
fn allow_git_reset_soft() {
    assert_allow("git reset --soft HEAD~1");
}

#[test]
fn allow_git_reset_mixed() {
    assert_allow("git reset HEAD~1");
}

#[test]
fn hard_deny_git_clean_fd() {
    assert_hard_deny("git clean -fd");
}

#[test]
fn hard_deny_git_clean_fdx() {
    assert_hard_deny("git clean -fdx");
}

#[test]
fn hard_deny_git_stash_bare() {
    // bare `git stash` = stash push
    assert_hard_deny("git stash");
}

#[test]
fn hard_deny_git_stash_push() {
    assert_hard_deny("git stash push -m 'wip'");
}

#[test]
fn hard_deny_git_stash_pop() {
    assert_hard_deny("git stash pop");
}

#[test]
fn hard_deny_git_stash_apply() {
    assert_hard_deny("git stash apply");
}

#[test]
fn hard_deny_git_stash_drop() {
    assert_hard_deny("git stash drop stash@{0}");
}

#[test]
fn hard_deny_git_stash_clear() {
    assert_hard_deny("git stash clear");
}

#[test]
fn hard_deny_git_stash_branch() {
    assert_hard_deny("git stash branch new-branch");
}

#[test]
fn allow_git_stash_list() {
    assert_allow("git stash list");
}

#[test]
fn allow_git_stash_show() {
    assert_allow("git stash show -p");
}

#[test]
fn allow_git_stash_diff() {
    assert_allow("git stash diff");
}

// ── Force-push / remote mutation ─────────────────────────────────────────

#[test]
fn hard_deny_git_push_force() {
    assert_hard_deny("git push --force origin main");
}

#[test]
fn hard_deny_git_push_f() {
    assert_hard_deny("git push -f origin main");
}

#[test]
fn hard_deny_git_push_force_with_lease() {
    assert_hard_deny("git push --force-with-lease");
}

#[test]
fn allow_git_push() {
    assert_allow("git push origin main");
}

#[test]
fn allow_git_push_no_args() {
    assert_allow("git push");
}

#[test]
fn hard_deny_git_remote_add() {
    assert_hard_deny("git remote add origin https://example.com");
}

#[test]
fn hard_deny_git_remote_remove() {
    assert_hard_deny("git remote remove origin");
}

#[test]
fn hard_deny_git_remote_rm() {
    assert_hard_deny("git remote rm origin");
}

#[test]
fn hard_deny_git_remote_rename() {
    assert_hard_deny("git remote rename origin upstream");
}

#[test]
fn hard_deny_git_remote_set_url() {
    assert_hard_deny("git remote set-url origin https://new.url");
}

#[test]
fn hard_deny_git_remote_set_branches() {
    assert_hard_deny("git remote set-branches origin main");
}

#[test]
fn allow_git_remote_v() {
    assert_allow("git remote -v");
}

#[test]
fn allow_git_remote_show() {
    assert_allow("git remote show origin");
}

// ── Package installs/publishes ───────────────────────────────────────────

#[test]
fn hard_deny_cargo_install() {
    assert_hard_deny("cargo install ripgrep");
}

#[test]
fn hard_deny_cargo_uninstall() {
    assert_hard_deny("cargo uninstall ripgrep");
}

#[test]
fn hard_deny_cargo_publish() {
    assert_hard_deny("cargo publish");
}

#[test]
fn hard_deny_cargo_add() {
    assert_hard_deny("cargo add serde");
}

#[test]
fn hard_deny_cargo_remove() {
    assert_hard_deny("cargo remove serde");
}

#[test]
fn hard_deny_cargo_update() {
    assert_hard_deny("cargo update");
}

#[test]
fn hard_deny_cargo_new() {
    assert_hard_deny("cargo new myproject");
}

#[test]
fn allow_cargo_build() {
    assert_allow("cargo build");
}

#[test]
fn allow_cargo_test() {
    assert_allow("cargo test");
}

#[test]
fn allow_cargo_check() {
    assert_allow("cargo check");
}

#[test]
fn allow_cargo_clippy() {
    assert_allow("cargo clippy");
}

#[test]
fn hard_deny_pip_install() {
    assert_hard_deny("pip install requests");
}

#[test]
fn hard_deny_pip3_install() {
    assert_hard_deny("pip3 install requests");
}

#[test]
fn hard_deny_pip_uninstall() {
    assert_hard_deny("pip uninstall requests");
}

#[test]
fn allow_pip_list() {
    assert_allow("pip list");
}

#[test]
fn hard_deny_npm_install() {
    assert_hard_deny("npm install lodash");
}

#[test]
fn hard_deny_npm_add() {
    assert_hard_deny("npm add lodash");
}

#[test]
fn hard_deny_npm_publish() {
    assert_hard_deny("npm publish");
}

#[test]
fn hard_deny_npm_deploy() {
    assert_hard_deny("npm deploy");
}

#[test]
fn allow_npm_list() {
    assert_allow("npm list");
}

#[test]
fn hard_deny_apt_install() {
    assert_hard_deny("apt install vim");
}

#[test]
fn hard_deny_apt_get_install() {
    assert_hard_deny("apt-get install vim");
}

#[test]
fn hard_deny_yum_install() {
    assert_hard_deny("yum install vim");
}

#[test]
fn hard_deny_dnf_remove() {
    assert_hard_deny("dnf remove vim");
}

#[test]
fn hard_deny_brew_install() {
    assert_hard_deny("brew install git");
}

// ── Network mutation ─────────────────────────────────────────────────────

#[test]
fn hard_deny_curl_post() {
    assert_hard_deny("curl -X POST https://example.com");
}

#[test]
fn hard_deny_curl_put() {
    assert_hard_deny("curl -X PUT https://example.com");
}

#[test]
fn hard_deny_curl_delete_method() {
    assert_hard_deny("curl -X DELETE https://example.com");
}

#[test]
fn hard_deny_curl_data() {
    assert_hard_deny("curl -d 'foo=bar' https://example.com");
}

#[test]
fn hard_deny_curl_data_raw() {
    assert_hard_deny(r#"curl --data-raw '{"key":"val"}' https://example.com"#);
}

#[test]
fn hard_deny_curl_form() {
    assert_hard_deny("curl -F file=@/tmp/f https://example.com");
}

#[test]
fn hard_deny_curl_upload() {
    assert_hard_deny("curl -T /tmp/file https://example.com/upload");
}

#[test]
fn allow_curl_get() {
    assert_allow("curl https://example.com");
}

#[test]
fn allow_curl_get_explicit() {
    assert_allow("curl -X GET https://example.com");
}

#[test]
fn hard_deny_wget_post() {
    assert_hard_deny("wget --post-data='foo=bar' https://example.com");
}

#[test]
fn hard_deny_wget_post_file() {
    assert_hard_deny("wget --post-file=/tmp/data https://example.com");
}

#[test]
fn hard_deny_wget_method_post() {
    assert_hard_deny("wget --method=POST https://example.com");
}

#[test]
fn allow_wget_get() {
    assert_allow("wget https://example.com/file");
}

#[test]
fn hard_deny_ssh() {
    assert_hard_deny("ssh user@host 'ls'");
}

#[test]
fn hard_deny_scp() {
    assert_hard_deny("scp file user@host:/tmp/");
}

#[test]
fn hard_deny_rsync() {
    assert_hard_deny("rsync -av src/ user@host:/dst/");
}

#[test]
fn hard_deny_nc() {
    assert_hard_deny("nc host 80");
}

// ── DB DDL/DML ───────────────────────────────────────────────────────────

#[test]
fn hard_deny_psql_drop_table() {
    assert_hard_deny_reason_contains(r#"psql -c "DROP TABLE users""#, "database mutation");
}

#[test]
fn hard_deny_psql_delete_from() {
    assert_hard_deny_reason_contains(
        r#"psql -c "DELETE FROM users WHERE id=1""#,
        "database mutation",
    );
}

#[test]
fn hard_deny_psql_truncate() {
    assert_hard_deny_reason_contains(r#"psql -c "TRUNCATE TABLE users""#, "database mutation");
}

#[test]
fn hard_deny_psql_alter() {
    assert_hard_deny_reason_contains(
        r#"psql -c "ALTER TABLE users ADD col text""#,
        "database mutation",
    );
}

#[test]
fn hard_deny_psql_insert() {
    assert_hard_deny_reason_contains(
        r#"psql -c "INSERT INTO users VALUES (1)""#,
        "database mutation",
    );
}

#[test]
fn hard_deny_psql_update() {
    assert_hard_deny_reason_contains(
        r#"psql -c "UPDATE users SET name='x'""#,
        "database mutation",
    );
}

#[test]
fn hard_deny_psql_create_table() {
    assert_hard_deny_reason_contains(r#"psql -c "CREATE TABLE t (id int)""#, "database mutation");
}

#[test]
fn allow_psql_select() {
    assert_allow(r#"psql -c "SELECT * FROM users""#);
}

#[test]
fn hard_deny_mysql_delete() {
    assert_hard_deny_reason_contains(r#"mysql -e "DELETE FROM users""#, "database mutation");
}

#[test]
fn hard_deny_sqlite3_insert() {
    assert_hard_deny_reason_contains(
        r#"sqlite3 db.db "INSERT INTO t VALUES (1)""#,
        "database mutation",
    );
}

#[test]
fn hard_deny_sqlite3_drop() {
    assert_hard_deny_reason_contains(r#"sqlite3 db.db "DROP TABLE t""#, "database mutation");
}

#[test]
fn hard_deny_redis_set() {
    assert_hard_deny("redis-cli SET key value");
}

#[test]
fn hard_deny_redis_del() {
    assert_hard_deny("redis-cli DEL key");
}

#[test]
fn hard_deny_redis_flushdb() {
    assert_hard_deny("redis-cli FLUSHDB");
}

#[test]
fn allow_redis_get() {
    assert_allow("redis-cli GET key");
}

// ── dd / install ─────────────────────────────────────────────────────────

#[test]
fn hard_deny_dd() {
    assert_hard_deny("dd if=/dev/zero of=/tmp/file bs=1M count=1");
}

#[test]
fn hard_deny_install_cmd() {
    assert_hard_deny("install -m 755 script.sh /usr/local/bin/script");
}

// ═══════════════════════════════════════════════════════════════════════════
//  AC 3: Soft-gated local worktree mutations and path-scope exclusions
// ═══════════════════════════════════════════════════════════════════════════

// ── Soft-gated file mutations (safe relative paths) ──────────────────────

#[test]
fn soft_gate_rm_relative() {
    assert_soft_gate_file("rm src/temp.txt");
}

#[test]
fn soft_gate_rm_rf_relative() {
    assert_soft_gate_file("rm -rf build/output");
}

#[test]
fn soft_gate_mv_relative() {
    assert_soft_gate_file("mv src/old.rs src/new.rs");
}

#[test]
fn soft_gate_mkdir_relative() {
    assert_soft_gate_file("mkdir build/output");
}

#[test]
fn soft_gate_touch_relative() {
    assert_soft_gate_file("touch src/newfile.rs");
}

#[test]
fn soft_gate_truncate_relative() {
    assert_soft_gate_file("truncate -s 0 build/log.txt");
}

#[test]
fn soft_gate_chmod_relative() {
    assert_soft_gate_file("chmod 755 scripts/run.sh");
}

#[test]
fn soft_gate_ln_relative() {
    assert_soft_gate_file("ln -s target link");
}

#[test]
fn soft_gate_sed_i_relative() {
    assert_soft_gate_file("sed -i 's/foo/bar/' src/config.txt");
}

#[test]
fn soft_gate_sed_i_bak_relative() {
    assert_soft_gate_file("sed -i.bak 's/foo/bar/' src/config.txt");
}

#[test]
fn soft_gate_cp_relative() {
    assert_soft_gate_file("cp src/template.rs src/generated.rs");
}

#[test]
fn soft_gate_redirect_to_relative() {
    let decision = classify("echo hello > output.txt");
    assert!(
        matches!(
            decision,
            ShellDestructiveDecision::SoftGate {
                class: ShellDestructiveClass::WorktreeLocalFileMutation,
                ..
            }
        ),
        "expected SoftGate for redirect to relative path, got {decision:?}",
    );
}

// ── Path-scope exclusions: .git ──────────────────────────────────────────

#[test]
fn hard_deny_rm_git_dir() {
    assert_hard_deny("rm -rf .git");
}

#[test]
fn hard_deny_rm_git_objects() {
    assert_hard_deny("rm .git/objects/pack/file.pack");
}

#[test]
fn hard_deny_mv_git_config() {
    assert_hard_deny("mv .git/config .git/config.bak");
}

#[test]
fn hard_deny_mkdir_git_subdir() {
    assert_hard_deny("mkdir .git/hooks");
}

#[test]
fn hard_deny_touch_in_git() {
    assert_hard_deny("touch .git/index");
}

#[test]
fn hard_deny_redirect_to_git() {
    let decision = classify("echo data > .git/info/exclude");
    assert!(
        matches!(decision, ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for redirect into .git, got {decision:?}",
    );
}

// ── Path-scope exclusions: parent directory (..) ─────────────────────────

#[test]
fn hard_deny_rm_parent() {
    assert_hard_deny("rm ../other-project/file.txt");
}

#[test]
fn hard_deny_mv_parent() {
    assert_hard_deny("mv ../sibling/file.txt ./file.txt");
}

#[test]
fn hard_deny_rm_parent_mid_path() {
    assert_hard_deny("rm some/path/../../other/file");
}

// ── Path-scope exclusions: absolute / out-of-worktree paths ──────────────

#[test]
fn hard_deny_rm_absolute() {
    assert_hard_deny("rm /etc/passwd");
}

#[test]
fn hard_deny_mv_absolute() {
    assert_hard_deny("mv /tmp/a /tmp/b");
}

#[test]
fn hard_deny_mkdir_absolute() {
    assert_hard_deny("mkdir /opt/mydir");
}

#[test]
fn hard_deny_redirect_to_absolute() {
    let decision = classify("echo hello > /tmp/output");
    assert!(
        matches!(decision, ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for redirect to absolute path, got {decision:?}",
    );
}

#[test]
fn allow_redirect_to_dev_null() {
    assert_allow("echo hello > /dev/null");
}

// ── Path-scope exclusions: owner read-source caches ───────────────────────

#[test]
fn hard_deny_rm_internal_read_sources() {
    assert_hard_deny("rm .task-runtime/read-sources/project/file.rs");
}

#[test]
fn hard_deny_mv_internal_read_sources() {
    assert_hard_deny("mv .task-runtime/read-sources/a.txt b.txt");
}

#[test]
fn hard_deny_redirect_to_internal_read_sources() {
    let decision = classify("echo data > .task-runtime/read-sources/patch");
    assert!(
        matches!(decision, ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for redirect into owner read-source cache, got {decision:?}",
    );
}

#[test]
fn hard_deny_rm_legacy_task_local_read_sources() {
    assert_hard_deny("rm .djinn-read-sources/project/file.rs");
}

#[test]
fn hard_deny_redirect_to_legacy_task_local_read_sources() {
    let decision = classify("echo data > .djinn-read-sources/project/patch");
    assert!(
        matches!(decision, ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for redirect into legacy read-source mount, got {decision:?}",
    );
}

// ── Path-scope exclusions: durable data paths ────────────────────────────

#[test]
fn hard_deny_rm_cargo_toml() {
    assert_hard_deny("rm Cargo.toml");
}

#[test]
fn hard_deny_rm_package_json() {
    assert_hard_deny("rm package.json");
}

#[test]
fn hard_deny_rm_cargo_lock() {
    assert_hard_deny("rm Cargo.lock");
}

#[test]
fn hard_deny_rm_dotenv() {
    assert_hard_deny("rm .env");
}

#[test]
fn hard_deny_rm_dockerfile() {
    assert_hard_deny("rm Dockerfile");
}

#[test]
fn hard_deny_rm_makefile() {
    assert_hard_deny("rm Makefile");
}

#[test]
fn hard_deny_rm_gitignore() {
    assert_hard_deny("rm .gitignore");
}

#[test]
fn hard_deny_mv_cargo_toml() {
    assert_hard_deny("mv Cargo.toml Cargo.toml.bak");
}

#[test]
fn hard_deny_redirect_to_cargo_toml() {
    let decision = classify("echo '[package]' > Cargo.toml");
    assert!(
        matches!(decision, ShellDestructiveDecision::HardDeny { .. }),
        "expected HardDeny for redirect to Cargo.toml, got {decision:?}",
    );
}

#[test]
fn hard_deny_rm_nested_cargo_toml() {
    assert_hard_deny("rm crates/mycrate/Cargo.toml");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Allow-by-default: ordinary build/read/test commands
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn allow_cargo_build_alias() {
    assert_allow("cargo b");
}

#[test]
fn allow_cargo_test_alias() {
    assert_allow("cargo t");
}

#[test]
fn allow_cargo_check_alias() {
    assert_allow("cargo c");
}

#[test]
fn allow_echo() {
    assert_allow("echo hello world");
}

#[test]
fn allow_cat() {
    assert_allow("cat src/main.rs");
}

#[test]
fn allow_grep() {
    assert_allow("grep -r 'pattern' src/");
}

#[test]
fn allow_git_log() {
    assert_allow("git log --oneline -10");
}

#[test]
fn allow_git_diff() {
    assert_allow("git diff HEAD~1");
}

#[test]
fn allow_git_status() {
    assert_allow("git status");
}

#[test]
fn allow_git_commit() {
    assert_allow("git commit -m 'msg'");
}

#[test]
fn allow_git_merge() {
    assert_allow("git merge feature-branch");
}

#[test]
fn allow_git_rebase() {
    assert_allow("git rebase main");
}

#[test]
fn allow_git_checkout() {
    assert_allow("git checkout feature-branch");
}

#[test]
fn allow_git_branch_delete() {
    // Local branch deletion is allowed (not the same as git reset --hard).
    assert_allow("git branch -d old-branch");
}

#[test]
fn allow_pip_show() {
    assert_allow("pip show requests");
}

#[test]
fn allow_npm_info() {
    assert_allow("npm info lodash");
}

#[test]
fn allow_sed_stdout() {
    assert_allow("sed 's/foo/bar/' file.txt");
}

#[test]
fn allow_find() {
    assert_allow("find . -name '*.rs'");
}

#[test]
fn allow_empty_command() {
    assert_allow("");
    assert_allow("   ");
}

#[test]
fn allow_true_false() {
    assert_allow("true");
    assert_allow("false");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Chained / pipeline commands
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn hard_deny_chain_with_git_clean() {
    assert_hard_deny("echo hello && git clean -fd");
}

#[test]
fn soft_gate_chain_with_rm() {
    assert_soft_gate_file("echo listing && rm build/output");
}

#[test]
fn allow_chain_all_safe() {
    assert_allow("echo hello && cargo test && git status");
}

#[test]
fn soft_gate_pipeline_xargs_rm() {
    // find ... | xargs rm — rm is the destructive command
    assert_soft_gate_file("find build -name '*.o' | xargs rm");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Sudo wrapping
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn hard_deny_sudo_git_reset_hard() {
    assert_hard_deny("sudo git reset --hard");
}

#[test]
fn soft_gate_sudo_rm_relative() {
    assert_soft_gate_file("sudo rm build/temp");
}

#[test]
fn allow_sudo_cargo_test() {
    assert_allow("sudo cargo test");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Display impl
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn display_allow() {
    assert_eq!(ShellDestructiveDecision::Allow.to_string(), "allowed");
}

#[test]
fn display_hard_deny() {
    let d = ShellDestructiveDecision::HardDeny {
        reason: "test reason".into(),
    };
    assert_eq!(d.to_string(), "hard-deny: test reason");
}

#[test]
fn display_soft_gate() {
    let d = ShellDestructiveDecision::SoftGate {
        class: ShellDestructiveClass::WorktreeLocalFileMutation,
        reason: "rm file.txt".into(),
    };
    assert_eq!(
        d.to_string(),
        "soft-gate(destructive.worktree_local_file_mutation): rm file.txt"
    );
}

#[test]
fn destructive_class_as_str_round_trip() {
    assert_eq!(
        ShellDestructiveClass::WorktreeLocalFileMutation.as_str(),
        "destructive.worktree_local_file_mutation"
    );
    assert_eq!(
        ShellDestructiveClass::VcsSoftGate.as_str(),
        "destructive.vcs_soft_gate"
    );
    assert_eq!(
        ShellDestructiveClass::DbSoftGate.as_str(),
        "destructive.db_soft_gate"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Existing validate_read_only_command is NOT weakened
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn read_only_validator_still_rejects_rm() {
    // The fail-closed validator still rejects rm as UnknownTool.
    let violations =
        crate::command_validator::validate_read_only_command("rm src/file.txt").unwrap_err();
    assert!(violations.contains(&crate::command_validator::CommandViolation::UnknownTool));
}

#[test]
fn read_only_validator_still_allows_cat() {
    assert!(crate::command_validator::validate_read_only_command("cat src/main.rs").is_ok());
}

#[test]
fn read_only_validator_still_rejects_git_commit() {
    let violations =
        crate::command_validator::validate_read_only_command("git commit -m 'msg'").unwrap_err();
    assert!(violations.contains(&crate::command_validator::CommandViolation::VcsMutation));
}
