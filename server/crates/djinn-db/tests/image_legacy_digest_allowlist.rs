//! The signed, immutable legacy-digest allowlist, loaded the way the cutover
//! loads it — against real PostgreSQL (task `eeky`, epic `xowm`).
//!
//! # What is actually being proven
//!
//! Not "the allowlist has a signature". [`SignedLegacyAllowlist`] has no
//! signature field, no `signed` flag and no `verified` boolean to read, so that
//! assertion is unwritable here by construction. What is proven is that the
//! four tamperings below each **fail to produce a value at all**:
//!
//! 1. an entry in mutable-tag form (`image:latest`) is rejected at load;
//! 2. one flipped bit of the signature is rejected;
//! 3. one altered character of an allowlisted digest is rejected;
//! 4. a document signed by a key other than the deployment key is rejected.
//!
//! Each is asserted from a *positive control* in the same test — the untampered
//! document loads — so a load path that rejected everything unconditionally
//! could not pass either.
//!
//! # Why real Postgres
//!
//! [`ImageRepository::signed_legacy_digest_allowlist`] does not just verify a
//! document: it cross-checks the verified digest set against the catalog rows
//! that would actually dispatch without a handshake
//! (`legacy_pre_protocol_digests`, whose predicate is `status = 'ready' AND
//! launcher_authority_protocol IS NULL AND registry_digest IS NOT NULL`). That
//! predicate is a property of the `images` relation and migration 166's
//! `images_declared_protocol_requires_digest_check`. A fake repository holds
//! any predicate you like, and would prove none of it.
//!
//! `Database::ephemeral()` clones the migrated test template; it is a real
//! PostgreSQL database, not an in-memory stand-in.

use djinn_db::launcher_compatibility::{InventoryFault, InventoryProvenance};
use djinn_db::repositories::image::LegacyAllowlistDefect;
use djinn_db::{Database, ImageRepository, LegacyDigestInventory, PreProtocolDigest};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use ring::signature::KeyPair as _;

// ── fixtures ────────────────────────────────────────────────────────────────

/// A canonical immutable manifest digest: `sha256:` + 64 lowercase hex.
fn digest(seed: char) -> String {
    format!("sha256:{}", seed.to_string().repeat(64))
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The deployment's signing key. A second call yields a *different* key, which
/// is what mutation 4 uses as the attacker's key.
fn signing_key() -> ring::signature::Ed25519KeyPair {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
}

/// The inventory document naming `entries`, serialized to the exact bytes that
/// get signed. Signing covers these bytes, so any later edit — including a
/// single character inside one digest — invalidates the signature.
fn document(entries: &[String]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "issuer": "platform-ops",
        "issued_at": "2026-07-31T00:00:00Z",
        "digests": entries,
    }))
    .unwrap()
}

/// Load a document exactly as the deployment does: verify it against the
/// deployment public key and signature, then resolve it against the catalog.
async fn load(
    repo: &ImageRepository,
    doc: &[u8],
    public_key: &str,
    signature: &str,
) -> Result<djinn_db::repositories::image::SignedLegacyAllowlist, LegacyAllowlistDefect> {
    let inventory =
        LegacyDigestInventory::from_signed_document(doc, Some(public_key), Some(signature));
    repo.signed_legacy_digest_allowlist(&inventory).await
}

/// Seed a `ready`, digest-pinned, no-handshake catalog row — the exact
/// population the allowlist exists to vouch for.
async fn seed_no_handshake_image(db: &Database, id: &str, digest: &str) {
    let repo = ImageRepository::new(db.clone());
    repo.create(id, id, None, "{}").await.unwrap();
    repo.mark_ready(id, &format!("reg/{id}:tag"), Some(digest), None)
        .await
        .unwrap();
}

// ── the four mutations ──────────────────────────────────────────────────────

/// Positive control plus the four tamperings, in one test so the control and
/// the mutations share a catalog. If the load path were broken open the control
/// would still pass and every mutation would fail; if it were broken closed the
/// control would fail. Both directions are therefore covered.
#[tokio::test]
async fn the_signed_allowlist_rejects_tags_tampering_and_a_foreign_key() {
    let db = Database::ephemeral().await.unwrap();
    let repo = ImageRepository::new(db.clone());
    seed_no_handshake_image(&db, "legacy-a", &digest('a')).await;

    let key = signing_key();
    let public_key = b64(key.public_key().as_ref());
    let doc = document(&[digest('a')]);
    let signature = b64(key.sign(&doc).as_ref());

    // Positive control: the untampered, correctly signed document loads and
    // covers the seeded row.
    let allowlist = load(&repo, &doc, &public_key, &signature)
        .await
        .expect("a correctly signed document over an inventoried catalog must load");
    assert_eq!(
        allowlist.digests,
        [PreProtocolDigest::parse(&digest('a')).unwrap()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        allowlist
            .inventoried
            .iter()
            .map(|image| image.image_id.as_str())
            .collect::<Vec<_>>(),
        vec!["legacy-a"]
    );
    assert_eq!(
        allowlist.provenance,
        InventoryProvenance {
            issuer: "platform-ops".into(),
            issued_at: "2026-07-31T00:00:00Z".into(),
        }
    );

    // MUTATION 1 — a mutable tag in the allowlist. Signed correctly; refused
    // for what it names, not for how it was signed.
    let tag_doc = document(&["djinn-image-legacy-a:latest".to_owned()]);
    let tag_signature = b64(key.sign(&tag_doc).as_ref());
    assert!(
        matches!(
            load(&repo, &tag_doc, &public_key, &tag_signature).await,
            Err(LegacyAllowlistDefect::Unusable(
                InventoryFault::DigestMalformed(_)
            ))
        ),
        "a mutable-tag entry must be rejected at load even under a valid signature"
    );

    // MUTATION 2 — one flipped bit of the signature.
    let mut raw_signature = key.sign(&doc).as_ref().to_vec();
    raw_signature[0] ^= 0b0000_0001;
    assert_eq!(
        load(&repo, &doc, &public_key, &b64(&raw_signature)).await,
        Err(LegacyAllowlistDefect::Unusable(
            InventoryFault::SignatureInvalid
        )),
        "one flipped signature bit must be rejected"
    );

    // MUTATION 3 — one altered character inside an allowlisted digest, with the
    // ORIGINAL signature. This is the substitution a registry-level attacker
    // wants: the document still parses, still lists a canonical digest, and
    // still carries a signature that verified a moment ago.
    let mut altered = digest('a');
    altered.replace_range(7..8, "b");
    assert_ne!(altered, digest('a'));
    assert!(
        PreProtocolDigest::parse(&altered).is_ok(),
        "the altered digest must still be canonical, or this mutation would be \
         caught by shape validation instead of by the signature"
    );
    assert_eq!(
        load(&repo, &document(&[altered]), &public_key, &signature).await,
        Err(LegacyAllowlistDefect::Unusable(
            InventoryFault::SignatureInvalid
        )),
        "altering one character of an allowlisted digest must be rejected"
    );

    // MUTATION 4 — signed by a key that is not the deployment key. The document
    // is byte-identical to the control and its signature is internally
    // consistent; only the issuing key differs.
    let attacker = signing_key();
    let attacker_signature = b64(attacker.sign(&doc).as_ref());
    assert_ne!(public_key, b64(attacker.public_key().as_ref()));
    assert_eq!(
        load(&repo, &doc, &public_key, &attacker_signature).await,
        Err(LegacyAllowlistDefect::Unusable(
            InventoryFault::SignatureInvalid
        )),
        "a document signed by a key other than the deployment key must be rejected"
    );

    // …and the attacker's own key does not rescue it either: the deployment
    // pins the key, so a self-consistent (document, key, signature) triple from
    // somewhere else is still not this deployment's allowlist. Verified by
    // showing the triple IS self-consistent and would load under the attacker's
    // key, which is precisely why pinning is what refuses it.
    assert!(
        load(
            &repo,
            &doc,
            &b64(attacker.public_key().as_ref()),
            &attacker_signature
        )
        .await
        .is_ok(),
        "the attacker's triple is self-consistent; only the pinned deployment \
         key distinguishes it, which is the property under test"
    );
}

/// An unsigned (unconfigured) inventory is a legitimate *dispatch* state and
/// must never authorize a cutover.
///
/// This is the one place the cutover is deliberately stricter than admission:
/// `launcher_compatibility` treats `Unconfigured` as unarmed so a missing
/// config file cannot strand an already-built catalog, and this method refuses
/// it so an operator cannot flip authority having proven nothing.
#[tokio::test]
async fn an_unsigned_inventory_never_authorizes_a_cutover() {
    let db = Database::ephemeral().await.unwrap();
    let repo = ImageRepository::new(db.clone());
    seed_no_handshake_image(&db, "legacy-a", &digest('a')).await;

    assert_eq!(
        repo.signed_legacy_digest_allowlist(&LegacyDigestInventory::Unconfigured)
            .await,
        Err(LegacyAllowlistDefect::Unsigned)
    );

    // The very same catalog under a *signed* inventory loads, so the refusal
    // above is caused by the missing signature and not by the catalog.
    let key = signing_key();
    let doc = document(&[digest('a')]);
    assert!(
        load(
            &repo,
            &doc,
            &b64(key.public_key().as_ref()),
            &b64(key.sign(&doc).as_ref())
        )
        .await
        .is_ok()
    );
}

/// A no-handshake row the signed document does not name blocks the load, and
/// the defect names the exact row so an operator can act on it.
///
/// This is the arm that makes the allowlist an *inventory* rather than a
/// wishlist: signing a document that omits a live dispatch-eligible image would
/// otherwise produce a green cutover and a refused dispatch afterwards.
#[tokio::test]
async fn an_uninventoried_no_handshake_row_blocks_the_load() {
    let db = Database::ephemeral().await.unwrap();
    let repo = ImageRepository::new(db.clone());
    seed_no_handshake_image(&db, "legacy-a", &digest('a')).await;
    seed_no_handshake_image(&db, "legacy-b", &digest('b')).await;

    let key = signing_key();
    let public_key = b64(key.public_key().as_ref());
    let partial = document(&[digest('a')]);
    let partial_signature = b64(key.sign(&partial).as_ref());

    let Err(LegacyAllowlistDefect::Uninventoried(missing)) =
        load(&repo, &partial, &public_key, &partial_signature).await
    else {
        panic!("an uninventoried dispatch-eligible row must block the load");
    };
    assert_eq!(
        missing
            .iter()
            .map(|image| image.image_id.as_str())
            .collect::<Vec<_>>(),
        vec!["legacy-b"],
        "the defect must name the exact rows, not merely report a count"
    );

    // Adding the missing digest to the signed document resolves it. Same
    // catalog, same key: the only change is the document's contents.
    let full = document(&[digest('a'), digest('b')]);
    let full_signature = b64(key.sign(&full).as_ref());
    let allowlist = load(&repo, &full, &public_key, &full_signature)
        .await
        .unwrap();
    assert_eq!(allowlist.inventoried.len(), 2);
}

/// A declaring image is not part of the legacy population at all, and a
/// digest-less row cannot be vouched for by any allowlist.
///
/// Both facts are properties of the `images` relation, which is why this runs
/// against real Postgres: the first is `launcher_authority_protocol IS NULL` in
/// the production predicate, and the second is migration 166's
/// `images_declared_protocol_requires_digest_check` refusing the write.
#[tokio::test]
async fn declaring_and_digestless_rows_are_outside_the_legacy_population() {
    let db = Database::ephemeral().await.unwrap();
    let repo = ImageRepository::new(db.clone());
    seed_no_handshake_image(&db, "legacy-a", &digest('a')).await;

    // A resize-v2 declaring row with a digest: dispatch-eligible, but it speaks
    // for itself and needs no allowlist entry.
    repo.create("modern", "modern", None, "{}").await.unwrap();
    repo.mark_ready(
        "modern",
        "reg/modern:tag",
        Some(&digest('c')),
        Some(LauncherAuthorityProtocol::ResizeV2),
    )
    .await
    .unwrap();

    // A digest-less no-handshake row: `legacy_pre_protocol_digests` excludes it
    // because there is no immutable identity to vouch for.
    repo.create("digestless", "digestless", None, "{}")
        .await
        .unwrap();
    repo.mark_ready("digestless", "reg/digestless:tag", None, None)
        .await
        .unwrap();

    let key = signing_key();
    let doc = document(&[digest('a')]);
    let allowlist = load(
        &repo,
        &doc,
        &b64(key.public_key().as_ref()),
        &b64(key.sign(&doc).as_ref()),
    )
    .await
    .expect("a declaring row and a digestless row are not uninventoried legacy rows");
    assert_eq!(
        allowlist
            .inventoried
            .iter()
            .map(|image| image.image_id.as_str())
            .collect::<Vec<_>>(),
        vec!["legacy-a"]
    );

    // And the declaring row could not have been written without its digest, so
    // "declares a protocol but cannot be pinned" is not a reachable state.
    assert!(
        repo.mark_ready(
            "digestless",
            "reg/digestless:tag",
            None,
            Some(LauncherAuthorityProtocol::ResizeV2)
        )
        .await
        .is_err()
    );
}
