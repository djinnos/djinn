use super::*;

// ── Redirect violations ───────────────────────────────────────────

#[test]
fn rejects_output_redirect() {
    let violations = validate_read_only_command("echo hello > /tmp/out").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

#[test]
fn rejects_append_redirect() {
    let violations = validate_read_only_command("echo hello >> /tmp/out").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

#[test]
fn rejects_stderr_redirect() {
    let violations = validate_read_only_command("echo hello 2>/dev/null").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

#[test]
fn rejects_combined_redirect() {
    let violations = validate_read_only_command("echo hello &> /tmp/out").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

#[test]
fn rejects_tee_with_file_arg() {
    let violations = validate_read_only_command("echo hello | tee /tmp/out").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

// ── Heredoc violations ────────────────────────────────────────────

#[test]
fn rejects_heredoc_write() {
    let violations = validate_read_only_command("cat <<EOF > /tmp/f\nhello\nEOF").unwrap_err();
    assert!(violations.contains(&CommandViolation::Redirect));
}

// ── File mutation violations ──────────────────────────────────────

#[test]
fn rejects_rm() {
    let violations = validate_read_only_command("rm /tmp/file").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_rm_rf() {
    let violations = validate_read_only_command("rm -rf /tmp/dir").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_mv() {
    let violations = validate_read_only_command("mv /tmp/a /tmp/b").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_cp() {
    let violations = validate_read_only_command("cp /tmp/a /tmp/b").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_chmod() {
    let violations = validate_read_only_command("chmod 755 /tmp/file").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_mkdir() {
    let violations = validate_read_only_command("mkdir /tmp/dir").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_touch() {
    let violations = validate_read_only_command("touch /tmp/file").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_dd() {
    let violations =
        validate_read_only_command("dd if=/dev/zero of=/tmp/file bs=1M count=1").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_ln() {
    let violations = validate_read_only_command("ln -s /tmp/a /tmp/b").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

// ── Package install violations ────────────────────────────────────

#[test]
fn rejects_pip_install() {
    let violations = validate_read_only_command("pip install requests").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_pip3_install() {
    let violations = validate_read_only_command("pip3 install requests").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_npm_install() {
    let violations = validate_read_only_command("npm install lodash").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_npm_add() {
    let violations = validate_read_only_command("npm add lodash").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_cargo_install() {
    let violations = validate_read_only_command("cargo install ripgrep").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_cargo_add() {
    let violations = validate_read_only_command("cargo add serde").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_cargo_new() {
    let violations = validate_read_only_command("cargo new myproject").unwrap_err();
    assert!(violations.contains(&CommandViolation::PackageInstall));
}

#[test]
fn rejects_apt_install() {
    let violations = validate_read_only_command("apt install vim").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_yum_install() {
    let violations = validate_read_only_command("yum install vim").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

// ── Network mutation violations ───────────────────────────────────

#[test]
fn rejects_curl_post() {
    let violations = validate_read_only_command("curl -X POST https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_data() {
    let violations =
        validate_read_only_command("curl -d 'foo=bar' https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_data_raw() {
    let violations =
        validate_read_only_command("curl --data-raw '{\"key\":\"val\"}' https://example.com")
            .unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_form() {
    let violations =
        validate_read_only_command("curl -F file=@/tmp/f https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_upload() {
    let violations =
        validate_read_only_command("curl -T /tmp/file https://example.com/upload").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_wget_post() {
    let violations =
        validate_read_only_command("wget --post-data='foo=bar' https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_ssh_with_command() {
    let violations = validate_read_only_command("ssh user@host 'rm -rf /'").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

// ── VCS mutation violations ───────────────────────────────────────

#[test]
fn rejects_git_commit() {
    let violations = validate_read_only_command("git commit -m 'msg'").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_push() {
    let violations = validate_read_only_command("git push origin main").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_merge() {
    let violations = validate_read_only_command("git merge feature-branch").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_rebase() {
    let violations = validate_read_only_command("git rebase main").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_reset() {
    let violations = validate_read_only_command("git reset --hard HEAD~1").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_clean() {
    let violations = validate_read_only_command("git clean -fd").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_checkout() {
    let violations = validate_read_only_command("git checkout feature-branch").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_cherry_pick() {
    let violations = validate_read_only_command("git cherry-pick abc123").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_stash_push() {
    let violations = validate_read_only_command("git stash push -m 'wip'").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_stash_pop() {
    let violations = validate_read_only_command("git stash pop").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_stash_drop() {
    let violations = validate_read_only_command("git stash drop stash@{0}").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_branch_delete() {
    let violations = validate_read_only_command("git branch -d old-branch").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_tag_delete() {
    let violations = validate_read_only_command("git tag -d v1.0").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_remote_add() {
    let violations =
        validate_read_only_command("git remote add origin https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn rejects_git_config_set() {
    let violations = validate_read_only_command("git config --set user.name test").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

// ── Database mutation violations ──────────────────────────────────

#[test]
fn rejects_psql_insert() {
    let violations =
        validate_read_only_command(r#"psql -c "INSERT INTO t VALUES (1)""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_update() {
    let violations = validate_read_only_command(r#"psql -c "UPDATE t SET x=1""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_delete() {
    let violations = validate_read_only_command(r#"psql -c "DELETE FROM t""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_drop() {
    let violations = validate_read_only_command(r#"psql -c "DROP TABLE t""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_create() {
    let violations =
        validate_read_only_command(r#"psql -c "CREATE TABLE t (id int)""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_alter() {
    let violations =
        validate_read_only_command(r#"psql -c "ALTER TABLE t ADD col text""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_psql_truncate() {
    let violations = validate_read_only_command(r#"psql -c "TRUNCATE TABLE t""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_sqlite3_insert() {
    let violations =
        validate_read_only_command(r#"sqlite3 db.db "INSERT INTO t VALUES (1)""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_redis_set() {
    let violations = validate_read_only_command("redis-cli SET key value").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_redis_del() {
    let violations = validate_read_only_command("redis-cli DEL key").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_redis_flushdb() {
    let violations = validate_read_only_command("redis-cli FLUSHDB").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

// ── Command chain violations ──────────────────────────────────────

#[test]
fn rejects_chain_with_mutation() {
    let violations = validate_read_only_command("cat file.txt && rm file.txt").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_semicolon_chain_with_mutation() {
    let violations = validate_read_only_command("echo hello; rm /tmp/f").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_or_chain_with_mutation() {
    let violations = validate_read_only_command("cat file.txt || rm file.txt").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

// ── Sudo wrapping ─────────────────────────────────────────────────

#[test]
fn rejects_sudo_rm() {
    let violations = validate_read_only_command("sudo rm /tmp/file").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_sudo_git_push() {
    let violations = validate_read_only_command("sudo git push").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

// ── Sed in-place ──────────────────────────────────────────────────

#[test]
fn rejects_sed_in_place() {
    let violations = validate_read_only_command("sed -i 's/foo/bar/' file.txt").unwrap_err();
    assert!(violations.contains(&CommandViolation::FileMutation));
}

#[test]
fn rejects_sed_i_flag() {
    let violations = validate_read_only_command("sed -i.bak 's/foo/bar/' file.txt").unwrap_err();
    assert!(violations.contains(&CommandViolation::FileMutation));
}

// ── find -delete ──────────────────────────────────────────────────

#[test]
fn rejects_find_delete() {
    let violations = validate_read_only_command("find /tmp -name '*.log' -delete").unwrap_err();
    assert!(violations.contains(&CommandViolation::FileMutation));
}

// ── npm/pnpm/yarn run ─────────────────────────────────────────────

#[test]
fn rejects_npm_run() {
    let violations = validate_read_only_command("npm run build").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_pnpm_run() {
    let violations = validate_read_only_command("pnpm run test").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_cargo_run() {
    let violations = validate_read_only_command("cargo run").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

// ── Allowed commands ──────────────────────────────────────────────

#[test]
fn allows_cat() {
    assert!(validate_read_only_command("cat /tmp/file").is_ok());
}

#[test]
fn allows_head() {
    assert!(validate_read_only_command("head -n 10 /tmp/file").is_ok());
}

#[test]
fn allows_tail() {
    assert!(validate_read_only_command("tail -f /tmp/file").is_ok());
}

#[test]
fn allows_grep() {
    assert!(validate_read_only_command("grep -r 'pattern' /tmp/dir").is_ok());
}

#[test]
fn allows_rg() {
    assert!(validate_read_only_command("rg 'pattern' /tmp/dir").is_ok());
}

#[test]
fn allows_ls() {
    assert!(validate_read_only_command("ls -la /tmp").is_ok());
}

#[test]
fn allows_find_readonly() {
    assert!(validate_read_only_command("find /tmp -name '*.rs'").is_ok());
}

#[test]
fn allows_find_exec_readonly() {
    assert!(validate_read_only_command("find /tmp -name '*.rs' -exec cat {} \\\\;").is_ok());
}

#[test]
fn allows_wc() {
    assert!(validate_read_only_command("wc -l /tmp/file").is_ok());
}

#[test]
fn allows_diff() {
    assert!(validate_read_only_command("diff /tmp/a /tmp/b").is_ok());
}

#[test]
fn allows_jq() {
    assert!(validate_read_only_command("jq '.key' /tmp/file.json").is_ok());
}

#[test]
fn allows_echo() {
    assert!(validate_read_only_command("echo hello").is_ok());
}

#[test]
fn allows_git_log() {
    assert!(validate_read_only_command("git log --oneline -10").is_ok());
}

#[test]
fn allows_git_diff() {
    assert!(validate_read_only_command("git diff HEAD~1").is_ok());
}

#[test]
fn allows_git_status() {
    assert!(validate_read_only_command("git status").is_ok());
}

#[test]
fn allows_git_show() {
    assert!(validate_read_only_command("git show HEAD").is_ok());
}

#[test]
fn allows_git_branch_list() {
    assert!(validate_read_only_command("git branch -a").is_ok());
}

#[test]
fn allows_git_stash_list() {
    assert!(validate_read_only_command("git stash list").is_ok());
}

#[test]
fn allows_git_stash_show() {
    assert!(validate_read_only_command("git stash show -p").is_ok());
}

#[test]
fn allows_git_remote_list() {
    assert!(validate_read_only_command("git remote -v").is_ok());
}

#[test]
fn allows_git_config_get() {
    assert!(validate_read_only_command("git config --get user.name").is_ok());
}

#[test]
fn allows_git_config_list() {
    assert!(validate_read_only_command("git config --list").is_ok());
}

#[test]
fn allows_git_blame() {
    assert!(validate_read_only_command("git blame file.rs").is_ok());
}

#[test]
fn allows_git_rev_parse() {
    assert!(validate_read_only_command("git rev-parse HEAD").is_ok());
}

#[test]
fn allows_git_ls_files() {
    assert!(validate_read_only_command("git ls-files").is_ok());
}

#[test]
fn allows_git_tag_list() {
    assert!(validate_read_only_command("git tag -l").is_ok());
}

#[test]
fn allows_curl_get() {
    assert!(validate_read_only_command("curl https://example.com").is_ok());
}

#[test]
fn allows_curl_get_explicit() {
    assert!(validate_read_only_command("curl -X GET https://example.com").is_ok());
}

#[test]
fn allows_curl_head() {
    assert!(validate_read_only_command("curl -I https://example.com").is_ok());
}

#[test]
fn allows_wget() {
    assert!(validate_read_only_command("wget https://example.com/file").is_ok());
}

#[test]
fn allows_pip_list() {
    assert!(validate_read_only_command("pip list").is_ok());
}

#[test]
fn allows_pip_show() {
    assert!(validate_read_only_command("pip show requests").is_ok());
}

#[test]
fn allows_npm_list() {
    assert!(validate_read_only_command("npm list").is_ok());
}

#[test]
fn allows_npm_info() {
    assert!(validate_read_only_command("npm info lodash").is_ok());
}

#[test]
fn allows_cargo_check() {
    assert!(validate_read_only_command("cargo check").is_ok());
}

#[test]
fn allows_cargo_build() {
    assert!(validate_read_only_command("cargo build").is_ok());
}

#[test]
fn allows_cargo_test() {
    assert!(validate_read_only_command("cargo test").is_ok());
}

#[test]
fn allows_cargo_clippy() {
    assert!(validate_read_only_command("cargo clippy").is_ok());
}

#[test]
fn allows_cargo_doc() {
    assert!(validate_read_only_command("cargo doc").is_ok());
}

#[test]
fn allows_cargo_tree() {
    assert!(validate_read_only_command("cargo tree").is_ok());
}

#[test]
fn allows_psql_select() {
    assert!(validate_read_only_command(r#"psql -c "SELECT * FROM t""#).is_ok());
}

#[test]
fn allows_redis_get() {
    assert!(validate_read_only_command("redis-cli GET key").is_ok());
}

#[test]
fn allows_redis_keys() {
    assert!(validate_read_only_command("redis-cli KEYS '*'").is_ok());
}

#[test]
fn allows_sed_stdout() {
    assert!(validate_read_only_command("sed 's/foo/bar/' file.txt").is_ok());
}

#[test]
fn allows_awk() {
    assert!(validate_read_only_command("awk '{print $1}' file.txt").is_ok());
}

#[test]
fn allows_sort() {
    assert!(validate_read_only_command("sort file.txt").is_ok());
}

#[test]
fn allows_uniq() {
    assert!(validate_read_only_command("uniq file.txt").is_ok());
}

#[test]
fn allows_cut() {
    assert!(validate_read_only_command("cut -d',' -f1 file.csv").is_ok());
}

#[test]
fn allows_tree() {
    assert!(validate_read_only_command("tree /tmp").is_ok());
}

#[test]
fn allows_stat() {
    assert!(validate_read_only_command("stat /tmp/file").is_ok());
}

#[test]
fn allows_sha256sum() {
    assert!(validate_read_only_command("sha256sum /tmp/file").is_ok());
}

#[test]
fn allows_base64_decode() {
    assert!(validate_read_only_command("base64 -d /tmp/file").is_ok());
}

#[test]
fn allows_strings() {
    assert!(validate_read_only_command("strings /tmp/binary").is_ok());
}

#[test]
fn allows_xargs_with_cat() {
    assert!(validate_read_only_command("find . -name '*.rs' | xargs cat").is_ok());
}

#[test]
fn allows_env_prefix() {
    assert!(validate_read_only_command("env RUST_LOG=debug cargo check").is_ok());
}

#[test]
fn allows_var_assign_prefix() {
    assert!(validate_read_only_command("RUST_LOG=debug cargo check").is_ok());
}

#[test]
fn allows_pipe_chain_readonly() {
    assert!(validate_read_only_command("cat file | grep pattern | sort").is_ok());
}

#[test]
fn allows_empty_command() {
    assert!(validate_read_only_command("").is_ok());
    assert!(validate_read_only_command("  ").is_ok());
}

#[test]
fn allows_which() {
    assert!(validate_read_only_command("which cargo").is_ok());
}

#[test]
fn allows_type() {
    assert!(validate_read_only_command("type cargo").is_ok());
}

// ── Combined / complex scenarios ──────────────────────────────────

#[test]
fn rejects_chain_of_two_readonly_where_second_is_mutation() {
    let violations = validate_read_only_command("git status && git push").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn allows_pipe_to_readonly() {
    assert!(validate_read_only_command("git log --oneline | head -5").is_ok());
}

#[test]
fn rejects_git_stash_branch() {
    let violations = validate_read_only_command("git stash branch new-branch").unwrap_err();
    assert!(violations.contains(&CommandViolation::VcsMutation));
}

#[test]
fn allows_true_noop() {
    assert!(validate_read_only_command("true").is_ok());
}

#[test]
fn allows_colon_noop() {
    assert!(validate_read_only_command(":").is_ok());
}

#[test]
fn rejects_curl_put() {
    let violations = validate_read_only_command("curl -X PUT https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_patch() {
    let violations = validate_read_only_command("curl -X PATCH https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_curl_delete_method() {
    let violations = validate_read_only_command("curl -X DELETE https://example.com").unwrap_err();
    assert!(violations.contains(&CommandViolation::NetworkMutation));
}

#[test]
fn rejects_mysql_insert() {
    let violations =
        validate_read_only_command(r#"mysql -c "INSERT INTO t VALUES (1)""#).unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn allows_mysql_select() {
    assert!(validate_read_only_command(r#"mysql -c "SELECT * FROM t""#).is_ok());
}

#[test]
fn rejects_redis_hset() {
    let violations = validate_read_only_command("redis-cli HSET myhash field value").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn allows_redis_hget() {
    assert!(validate_read_only_command("redis-cli HGET myhash field").is_ok());
}

#[test]
fn rejects_redis_incr() {
    let violations = validate_read_only_command("redis-cli INCR counter").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

#[test]
fn rejects_redis_expire() {
    let violations = validate_read_only_command("redis-cli EXPIRE key 3600").unwrap_err();
    assert!(violations.contains(&CommandViolation::DatabaseMutation));
}

// ── xargs with mutation ───────────────────────────────────────────

#[test]
fn rejects_xargs_rm() {
    let violations = validate_read_only_command("find . -name '*.log' | xargs rm").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}

#[test]
fn rejects_xargs_chmod() {
    let violations =
        validate_read_only_command("find . -name '*.sh' | xargs chmod +x").unwrap_err();
    assert!(violations.contains(&CommandViolation::UnknownTool));
}
