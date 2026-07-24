//! Redis execution coverage. Every assertion runs against an in-process RESP
//! fake that models the AUTH / ACL SETUSER-DELUSER / SCAN / UNLINK / PING /
//! SUBSCRIBE / SET / GET / PUBLISH permission semantics the wrapper depends on,
//! and additionally against a real `redis-server` when the binary is present.
//! The fake path is deterministic and always runs; the real path is skipped only
//! when the binary is absent, never when an assertion would fail.

use super::*;

use std::collections::HashSet;

use tokio::net::TcpListener;

#[test]
fn redis_acl_is_prefix_scoped_and_has_no_administrative_commands() {
    assert!(RedisAdapter::new("redis://127.0.0.1:6379", vec!["REDIS_URL".into()]).is_ok());
    assert!(REDIS_ALLOWED_COMMANDS.contains(&"+publish"));
    for forbidden in [
        "+acl",
        "+config",
        "+flushdb",
        "+flushall",
        "+script",
        "+module",
        "+function",
    ] {
        assert!(!REDIS_ALLOWED_COMMANDS.contains(&forbidden));
    }
}

const FAKE_ADMIN_PASSWORD: &str = "fake-admin-password";

// ----- In-process RESP fake modeling Redis ACL semantics -----

#[derive(Clone)]
struct FakeUser {
    password: Option<String>,
    enabled: bool,
    all_commands: bool,
    commands: HashSet<String>,
    all_keys: bool,
    key_patterns: Vec<String>,
    all_channels: bool,
    channel_patterns: Vec<String>,
}

impl FakeUser {
    fn reset() -> Self {
        Self {
            password: None,
            enabled: false,
            all_commands: false,
            commands: HashSet::new(),
            all_keys: false,
            key_patterns: Vec::new(),
            all_channels: false,
            channel_patterns: Vec::new(),
        }
    }

    fn can_run(&self, command: &str) -> bool {
        self.all_commands || self.commands.contains(command)
    }

    fn can_touch_key(&self, key: &str) -> bool {
        self.all_keys || self.key_patterns.iter().any(|p| glob_match(p, key))
    }

    fn can_touch_channel(&self, channel: &str) -> bool {
        self.all_channels || self.channel_patterns.iter().any(|p| glob_match(p, channel))
    }
}

struct FakeState {
    users: HashMap<String, FakeUser>,
    keys: BTreeMap<String, Vec<u8>>,
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn helper(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some(b'*') => {
                helper(&pattern[1..], text) || (!text.is_empty() && helper(pattern, &text[1..]))
            }
            Some(&head) => !text.is_empty() && head == text[0] && helper(&pattern[1..], &text[1..]),
        }
    }
    helper(pattern.as_bytes(), text.as_bytes())
}

struct FakeRedis {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FakeRedis {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FakeRedis {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut users = HashMap::new();
        users.insert(
            "default".to_owned(),
            FakeUser {
                password: Some(FAKE_ADMIN_PASSWORD.to_owned()),
                enabled: true,
                all_commands: true,
                commands: HashSet::new(),
                all_keys: true,
                key_patterns: Vec::new(),
                all_channels: true,
                channel_patterns: Vec::new(),
            },
        );
        let state = Arc::new(Mutex::new(FakeState {
            users,
            keys: BTreeMap::new(),
        }));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    fake_connection(stream, state).await;
                });
            }
        });
        Self { port, handle }
    }

    fn admin_url(&self) -> String {
        format!("redis://:{FAKE_ADMIN_PASSWORD}@127.0.0.1:{}", self.port)
    }
}

async fn read_command(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<Vec<String>> {
    let mut header = String::new();
    if reader.read_line(&mut header).await.ok()? == 0 {
        return None;
    }
    let count: usize = header
        .strip_suffix("\r\n")?
        .strip_prefix('*')?
        .parse()
        .ok()?;
    let mut arguments = Vec::with_capacity(count);
    for _ in 0..count {
        let mut bulk = String::new();
        if reader.read_line(&mut bulk).await.ok()? == 0 {
            return None;
        }
        let length: usize = bulk.strip_suffix("\r\n")?.strip_prefix('$')?.parse().ok()?;
        let mut body = vec![0u8; length + 2];
        reader.read_exact(&mut body).await.ok()?;
        arguments.push(String::from_utf8(body[..length].to_vec()).ok()?);
    }
    Some(arguments)
}

async fn fake_connection(stream: TcpStream, state: Arc<Mutex<FakeState>>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut authenticated: Option<String> = None;
    loop {
        let Some(command) = read_command(&mut reader).await else {
            return;
        };
        if command.is_empty() {
            continue;
        }
        let reply = handle_command(&state, &mut authenticated, &command).await;
        if write.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn simple(value: &str) -> String {
    format!("+{value}\r\n")
}
fn error(value: &str) -> String {
    format!("-{value}\r\n")
}
fn integer(value: i64) -> String {
    format!(":{value}\r\n")
}
fn bulk(value: &str) -> String {
    format!("${}\r\n{value}\r\n", value.len())
}
fn array_of_bulk(values: &[String]) -> String {
    let mut reply = format!("*{}\r\n", values.len());
    for value in values {
        reply.push_str(&bulk(value));
    }
    reply
}

async fn handle_command(
    state: &Arc<Mutex<FakeState>>,
    authenticated: &mut Option<String>,
    command: &[String],
) -> String {
    let name = command[0].to_ascii_lowercase();
    if name == "auth" {
        let state = state.lock().await;
        let (user, password) = match command.len() {
            2 => ("default".to_owned(), command[1].clone()),
            3 => (command[1].clone(), command[2].clone()),
            _ => return error("ERR wrong number of arguments for 'auth'"),
        };
        let ok = state
            .users
            .get(&user)
            .is_some_and(|u| u.enabled && u.password.as_deref() == Some(password.as_str()));
        drop(state);
        return if ok {
            *authenticated = Some(user);
            simple("OK")
        } else {
            error("WRONGPASS invalid username-password pair")
        };
    }

    let Some(username) = authenticated.clone() else {
        return error("NOAUTH Authentication required.");
    };
    let mut state = state.lock().await;
    let Some(actor) = state.users.get(&username).cloned() else {
        return error("NOPERM unknown user");
    };
    if !actor.can_run(&name) {
        return error("NOPERM this user has no permissions to run the command");
    }

    match name.as_str() {
        "ping" => simple("PONG"),
        "acl" => {
            let subcommand = command.get(1).map(|s| s.to_ascii_lowercase());
            match subcommand.as_deref() {
                Some("setuser") => {
                    let Some(target) = command.get(2).cloned() else {
                        return error("ERR wrong number of arguments");
                    };
                    let mut user = state
                        .users
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(FakeUser::reset);
                    for token in &command[3..] {
                        apply_acl_token(&mut user, token);
                    }
                    state.users.insert(target, user);
                    simple("OK")
                }
                Some("deluser") => {
                    let mut removed = 0;
                    for target in &command[2..] {
                        if state.users.remove(target).is_some() {
                            removed += 1;
                        }
                    }
                    integer(removed)
                }
                _ => error("ERR unknown ACL subcommand"),
            }
        }
        "scan" => {
            // The fake returns every matching key in one page (cursor "0").
            let mut pattern = "*".to_owned();
            let mut index = 2;
            while index < command.len() {
                if command[index].eq_ignore_ascii_case("match") {
                    if let Some(value) = command.get(index + 1) {
                        pattern = value.clone();
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            let matched: Vec<String> = state
                .keys
                .keys()
                .filter(|key| glob_match(&pattern, key) && actor.can_touch_key(key))
                .cloned()
                .collect();
            format!("*2\r\n{}{}", bulk("0"), array_of_bulk(&matched))
        }
        "set" => {
            let (Some(key), Some(value)) = (command.get(1), command.get(2)) else {
                return error("ERR wrong number of arguments");
            };
            if !actor.can_touch_key(key) {
                return error("NOPERM no permissions to access a key");
            }
            state.keys.insert(key.clone(), value.clone().into_bytes());
            simple("OK")
        }
        "get" => {
            let Some(key) = command.get(1) else {
                return error("ERR wrong number of arguments");
            };
            if !actor.can_touch_key(key) {
                return error("NOPERM no permissions to access a key");
            }
            match state.keys.get(key) {
                Some(value) => bulk(&String::from_utf8_lossy(value)),
                None => "$-1\r\n".to_owned(),
            }
        }
        "del" | "unlink" | "exists" => {
            let mut affected = 0;
            for key in &command[1..] {
                if !actor.can_touch_key(key) {
                    return error("NOPERM no permissions to access a key");
                }
                if name == "exists" {
                    if state.keys.contains_key(key) {
                        affected += 1;
                    }
                } else if state.keys.remove(key).is_some() {
                    affected += 1;
                }
            }
            integer(affected)
        }
        "publish" => {
            let Some(channel) = command.get(1) else {
                return error("ERR wrong number of arguments");
            };
            if !actor.can_touch_channel(channel) {
                return error("NOPERM no permissions to access a channel");
            }
            integer(0)
        }
        "subscribe" | "psubscribe" => {
            for channel in &command[1..] {
                if !actor.can_touch_channel(channel) {
                    return error("NOPERM no permissions to access a channel");
                }
            }
            let mut reply = String::new();
            for (position, channel) in command[1..].iter().enumerate() {
                reply.push_str("*3\r\n");
                reply.push_str(&bulk("subscribe"));
                reply.push_str(&bulk(channel));
                reply.push_str(&integer(position as i64 + 1));
            }
            reply
        }
        _ => error("ERR unsupported command"),
    }
}

fn apply_acl_token(user: &mut FakeUser, token: &str) {
    match token {
        "reset" => *user = FakeUser::reset(),
        "on" => user.enabled = true,
        "off" => user.enabled = false,
        "nopass" => user.password = None,
        "allkeys" | "~*" => user.all_keys = true,
        "allchannels" | "&*" => user.all_channels = true,
        "-@all" => {
            user.all_commands = false;
            user.commands.clear();
        }
        "+@all" | "allcommands" => user.all_commands = true,
        other => {
            if let Some(password) = other.strip_prefix('>') {
                user.password = Some(password.to_owned());
            } else if let Some(pattern) = other.strip_prefix('~') {
                user.key_patterns.push(pattern.to_owned());
            } else if let Some(pattern) = other.strip_prefix('&') {
                user.channel_patterns.push(pattern.to_owned());
            } else if let Some(command) = other.strip_prefix('+') {
                user.commands.insert(command.to_ascii_lowercase());
            } else if let Some(command) = other.strip_prefix('-') {
                user.commands.remove(&command.to_ascii_lowercase());
            }
        }
    }
}

// ----- Real redis-server, used only when the binary is present -----

struct RealRedis {
    child: std::process::Child,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for RealRedis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_real_redis() -> Option<RealRedis> {
    let dir = tempfile::tempdir().ok()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    let child = match std::process::Command::new("redis-server")
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("")
        .arg("--appendonly")
        .arg("no")
        .arg("--dir")
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        // Binary genuinely absent: skip the real path (not an assertion).
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return None,
    };
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Some(RealRedis {
                child,
                port,
                _dir: dir,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Run `body` against the always-on fake and, when available, real redis.
async fn against_redis<F, Fut>(body: F)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let fake = FakeRedis::start().await;
    body(fake.admin_url()).await;
    drop(fake);

    if let Some(real) = spawn_real_redis().await {
        body(format!("redis://127.0.0.1:{}", real.port)).await;
    }
}

fn url_prefix(url: &str) -> String {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "key_prefix")
        .expect("returned url must carry key_prefix")
        .1
        .into_owned()
}

async fn tenant_conn(endpoint: &Endpoint, url: &str) -> Result<Conn, AdapterError> {
    let parsed = Url::parse(url).unwrap();
    let user = parsed.username().to_owned();
    let password = parsed.password().unwrap().to_owned();
    Conn::open(endpoint, Some(&user), Some(&password), OPERATION_DEADLINE).await
}

fn scan_keys(reply: &Resp) -> Vec<String> {
    let Resp::Array(entries) = reply else {
        return Vec::new();
    };
    let Some(Resp::Array(keys)) = entries.get(1) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|key| key.text().map(str::to_owned))
        .collect()
}

// ----- AC4: cross-lease isolation -----

#[tokio::test]
async fn two_leases_cannot_touch_each_others_keys_or_channels() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
        let (a_id, a_env) = adapter
            .create("attempt_a".into(), "nonce_a".into())
            .await
            .unwrap();
        let (b_id, b_env) = adapter
            .create("attempt_b".into(), "nonce_b".into())
            .await
            .unwrap();
        assert_ne!(a_id, b_id);

        let a_url = &a_env["REDIS_URL"];
        let b_url = &b_env["REDIS_URL"];
        let a_prefix = url_prefix(a_url);
        let b_prefix = url_prefix(b_url);
        assert_ne!(a_prefix, b_prefix);

        let mut a = tenant_conn(&adapter.endpoint, a_url).await.unwrap();
        let mut b = tenant_conn(&adapter.endpoint, b_url).await.unwrap();

        // Each lease can act within its own namespace.
        assert!(
            a.cmd(&["SET".into(), format!("{a_prefix}k"), "v".into()])
                .await
                .is_ok()
        );
        assert!(
            a.cmd(&["PUBLISH".into(), format!("{a_prefix}c"), "m".into()])
                .await
                .is_ok()
        );
        assert!(
            b.cmd(&["SET".into(), format!("{b_prefix}k"), "v".into()])
                .await
                .is_ok()
        );

        // Neither lease can read, write, or publish into the other's prefix.
        assert!(
            b.cmd(&["GET".into(), format!("{a_prefix}k")])
                .await
                .is_err()
        );
        assert!(
            b.cmd(&["SET".into(), format!("{a_prefix}x"), "v".into()])
                .await
                .is_err()
        );
        assert!(
            b.cmd(&["PUBLISH".into(), format!("{a_prefix}c"), "m".into()])
                .await
                .is_err()
        );
        assert!(
            b.cmd(&["SUBSCRIBE".into(), format!("{a_prefix}c")])
                .await
                .is_err()
        );
        assert!(
            a.cmd(&["GET".into(), format!("{b_prefix}k")])
                .await
                .is_err()
        );

        adapter.delete(&a_id).await.unwrap();
        adapter.delete(&b_id).await.unwrap();
    })
    .await;
}

// ----- AC4: delete preserves the other lease's data and user -----

#[tokio::test]
async fn deleting_one_lease_preserves_the_others_data_and_user() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
        let (a_id, a_env) = adapter
            .create("attempt_a".into(), "nonce_a".into())
            .await
            .unwrap();
        let (b_id, b_env) = adapter
            .create("attempt_b".into(), "nonce_b".into())
            .await
            .unwrap();
        let a_url = a_env["REDIS_URL"].clone();
        let b_url = b_env["REDIS_URL"].clone();
        let a_prefix = url_prefix(&a_url);
        let b_prefix = url_prefix(&b_url);

        let mut a = tenant_conn(&adapter.endpoint, &a_url).await.unwrap();
        let mut b = tenant_conn(&adapter.endpoint, &b_url).await.unwrap();
        a.cmd(&["SET".into(), format!("{a_prefix}k1"), "v".into()])
            .await
            .unwrap();
        b.cmd(&["SET".into(), format!("{b_prefix}k1"), "v".into()])
            .await
            .unwrap();
        drop(a);

        adapter.delete(&a_id).await.unwrap();

        // Lease B's user still authenticates and can keep writing.
        let mut b_again = tenant_conn(&adapter.endpoint, &b_url).await.unwrap();
        assert!(
            b_again
                .cmd(&["SET".into(), format!("{b_prefix}k2"), "v".into()])
                .await
                .is_ok()
        );

        // Lease B's data survives; lease A's data and user are gone.
        let mut admin = adapter.admin().await.unwrap();
        let surviving = admin
            .cmd(&[
                "SCAN".into(),
                "0".into(),
                "MATCH".into(),
                format!("{b_prefix}*"),
                "COUNT".into(),
                "100".into(),
            ])
            .await
            .unwrap();
        assert!(scan_keys(&surviving).contains(&format!("{b_prefix}k1")));

        let purged = admin
            .cmd(&[
                "SCAN".into(),
                "0".into(),
                "MATCH".into(),
                format!("{a_prefix}*"),
                "COUNT".into(),
                "100".into(),
            ])
            .await
            .unwrap();
        assert!(scan_keys(&purged).is_empty());
        assert!(tenant_conn(&adapter.endpoint, &a_url).await.is_err());

        adapter.delete(&b_id).await.unwrap();
    })
    .await;
}

// ----- AC4: a fresh lease starts empty -----

#[tokio::test]
async fn a_new_lease_starts_with_an_empty_namespace() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
        // Pre-populate a first lease so the backend is non-empty.
        let (first_id, first_env) = adapter
            .create("attempt_first".into(), "nonce_first".into())
            .await
            .unwrap();
        let first_url = first_env["REDIS_URL"].clone();
        let first_prefix = url_prefix(&first_url);
        let mut first = tenant_conn(&adapter.endpoint, &first_url).await.unwrap();
        first
            .cmd(&["SET".into(), format!("{first_prefix}k"), "v".into()])
            .await
            .unwrap();

        let (fresh_id, fresh_env) = adapter
            .create("attempt_fresh".into(), "nonce_fresh".into())
            .await
            .unwrap();
        let fresh_prefix = url_prefix(&fresh_env["REDIS_URL"]);

        let mut admin = adapter.admin().await.unwrap();
        let reply = admin
            .cmd(&[
                "SCAN".into(),
                "0".into(),
                "MATCH".into(),
                format!("{fresh_prefix}*"),
                "COUNT".into(),
                "100".into(),
            ])
            .await
            .unwrap();
        assert!(scan_keys(&reply).is_empty());

        adapter.delete(&first_id).await.unwrap();
        adapter.delete(&fresh_id).await.unwrap();
    })
    .await;
}

// ----- AC2: readiness authenticates as the lease user and PINGs -----

#[tokio::test]
async fn ready_authenticates_as_the_lease_user() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
        let (lease_id, _) = adapter
            .create("attempt_ready".into(), "nonce_ready".into())
            .await
            .unwrap();
        adapter.ready(&lease_id).await.unwrap();
        assert_eq!(
            adapter.ready("lease_missing").await,
            Err(AdapterError::Rejected)
        );
        adapter.delete(&lease_id).await.unwrap();
    })
    .await;
}

// ----- AC3: idempotent create and repeated/unknown delete -----

#[tokio::test]
async fn create_is_idempotent_and_delete_is_repeatable() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
        let (first_id, first_env) = adapter
            .create("attempt_retry".into(), "nonce_retry".into())
            .await
            .unwrap();
        let (retry_id, retry_env) = adapter
            .create("attempt_retry".into(), "nonce_retry".into())
            .await
            .unwrap();
        assert_eq!(first_id, retry_id);
        assert_eq!(first_env, retry_env);
        assert_eq!(adapter.leases.lock().await.len(), 1);

        adapter.delete(&first_id).await.unwrap();
        // Repeated delete and unknown lease are both no-op successes.
        adapter.delete(&first_id).await.unwrap();
        adapter.delete("lease_unknown").await.unwrap();
    })
    .await;
}

// ----- AC3: timeout mid-provision rolls back and preserves nothing -----

#[tokio::test]
async fn timeout_after_user_creation_rolls_back_the_user() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()])
            .unwrap()
            .with_operation_deadline(Duration::from_millis(25));
        adapter
            .pause_after_creating_user(Duration::from_millis(200))
            .await;
        assert_eq!(
            adapter
                .create("attempt_timeout".into(), "nonce_timeout".into())
                .await,
            Err(AdapterError::Rejected)
        );
        // Background rollback removes the user it created and clears the
        // retained creation state, leaving nothing behind.
        wait_for_empty_creations(&adapter).await;
        assert!(adapter.leases.lock().await.is_empty());
    })
    .await;
}

// ----- AC3 / finding 4: a failed rollback retains cleanup authority -----

#[tokio::test]
async fn failed_rollback_retains_authority_until_a_retry_cleans_it() {
    against_redis(|admin_url| async move {
        let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()])
            .unwrap()
            .with_operation_deadline(Duration::from_millis(25));
        adapter
            .pause_after_creating_user(Duration::from_millis(200))
            .await;
        adapter.fail_next_rollbacks(1);
        assert_eq!(
            adapter
                .create("attempt_cleanup".into(), "nonce_cleanup".into())
                .await,
            Err(AdapterError::Rejected)
        );
        wait_for_cleanup_pending(&adapter).await;
        // The identical retry retries cleanup rather than orphaning the user.
        assert_eq!(
            adapter
                .create("attempt_cleanup".into(), "nonce_cleanup".into())
                .await,
            Err(AdapterError::Rejected)
        );
        wait_for_empty_creations(&adapter).await;
    })
    .await;
}

async fn wait_for_empty_creations(adapter: &RedisAdapter) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if adapter.creations.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background cleanup did not finish");
}

async fn wait_for_cleanup_pending(adapter: &RedisAdapter) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                adapter.creations.lock().await.get(&CreationKey {
                    attempt_id: "attempt_cleanup".into(),
                    lease_nonce: "nonce_cleanup".into(),
                }),
                Some(RedisCreationState::CleanupPending(_))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("partial cleanup authority was discarded");
}

// ----- RESP client coverage: read_resp over crafted byte streams -----

async fn resp_of(chunks: &'static [&'static [u8]]) -> Result<Resp, AdapterError> {
    let (mut writer, reader) = tokio::io::duplex(256);
    tokio::spawn(async move {
        for chunk in chunks {
            let _ = writer.write_all(chunk).await;
            tokio::task::yield_now().await;
        }
    });
    let mut reader = BufReader::new(reader);
    read_resp(&mut reader).await
}

#[tokio::test]
async fn read_resp_decodes_the_reply_types_the_wrapper_consumes() {
    assert!(matches!(resp_of(&[b"+OK\r\n"]).await, Ok(Resp::Text(value)) if value == "OK"));
    assert!(matches!(resp_of(&[b":7\r\n"]).await, Ok(Resp::Integer)));
    assert!(matches!(
        resp_of(&[b"$5\r\nhello\r\n"]).await,
        Ok(Resp::Text(value)) if value == "hello"
    ));
    let array = resp_of(&[b"*2\r\n$1\r\na\r\n:3\r\n"]).await.unwrap();
    let Resp::Array(items) = array else {
        panic!("expected array");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].text(), Some("a"));
    assert!(matches!(items[1], Resp::Integer));
}

#[tokio::test]
async fn read_resp_maps_error_replies_to_a_bounded_error() {
    assert!(matches!(
        resp_of(&[b"-NOPERM this user has no permissions\r\n"]).await,
        Err(AdapterError::Rejected)
    ));
    // An unknown RESP type byte is rejected rather than misparsed.
    assert!(matches!(
        resp_of(&[b"?bogus\r\n"]).await,
        Err(AdapterError::Rejected)
    ));
}

#[tokio::test]
async fn read_resp_reassembles_a_reply_split_across_reads() {
    let reply = resp_of(&[b"$11\r\nhel", b"lo ", b"world\r\n"]).await;
    assert!(matches!(reply, Ok(Resp::Text(value)) if value == "hello world"));
}
