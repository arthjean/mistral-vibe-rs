use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{CredentialTargets, StoredCredential, WinVaultLayout};
use crate::auth::keyring::{KEYRING_SERVICE, KeyringBackend, KeyringFailure, KeyringStore};

/// The credential manager as a map from target to user name and secret.
#[derive(Default)]
struct MemoryTargets {
    credentials: Mutex<BTreeMap<String, StoredCredential>>,
}

impl MemoryTargets {
    fn with(entries: &[(&str, &str, &str)]) -> Self {
        let targets = Self::default();
        for (target, user_name, secret) in entries {
            targets
                .write(target, user_name, secret)
                .expect("seeded credential");
        }
        targets
    }

    fn snapshot(&self) -> BTreeMap<String, (String, String)> {
        self.credentials
            .lock()
            .expect("credentials")
            .iter()
            .map(|(target, stored)| {
                (
                    target.clone(),
                    (stored.user_name.clone(), stored.secret.clone()),
                )
            })
            .collect()
    }
}

impl CredentialTargets for MemoryTargets {
    fn read(&self, target: &str) -> Result<Option<StoredCredential>, KeyringFailure> {
        Ok(self
            .credentials
            .lock()
            .expect("credentials")
            .get(target)
            .cloned())
    }

    fn write(&self, target: &str, user_name: &str, secret: &str) -> Result<(), KeyringFailure> {
        self.credentials.lock().expect("credentials").insert(
            target.to_owned(),
            StoredCredential {
                user_name: user_name.to_owned(),
                secret: secret.to_owned(),
            },
        );
        Ok(())
    }

    fn remove(&self, target: &str) -> Result<(), KeyringFailure> {
        self.credentials
            .lock()
            .expect("credentials")
            .remove(target)
            .map(drop)
            .ok_or(KeyringFailure::NoEntry)
    }
}

fn entry(user_name: &str, secret: &str) -> (String, String) {
    (user_name.to_owned(), secret.to_owned())
}

#[test]
fn a_key_the_reference_stored_is_found_under_the_plain_target() {
    let layout = WinVaultLayout::new(MemoryTargets::with(&[(
        "ai.mistral.vibe",
        "MISTRAL_API_KEY",
        "reference-key",
    )]));
    assert_eq!(
        layout.get(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Ok(Some("reference-key".to_owned()))
    );
    assert_eq!(layout.get(KEYRING_SERVICE, "OTHER_KEY"), Ok(None));
}

#[test]
fn a_write_takes_the_plain_target_and_never_the_former_one() {
    let layout = WinVaultLayout::new(MemoryTargets::default());
    layout
        .set(KEYRING_SERVICE, "MISTRAL_API_KEY", "port-key")
        .expect("write");
    assert_eq!(
        layout.targets.snapshot(),
        BTreeMap::from([(
            "ai.mistral.vibe".to_owned(),
            entry("MISTRAL_API_KEY", "port-key")
        )])
    );
}

#[test]
fn a_second_account_moves_the_holder_to_its_compound_target() {
    let layout = WinVaultLayout::new(MemoryTargets::with(&[(
        "ai.mistral.vibe",
        "mcp-oauth:docs:tokens",
        "{\"access_token\":\"t\"}",
    )]));
    layout
        .set(KEYRING_SERVICE, "MISTRAL_API_KEY", "port-key")
        .expect("write");
    assert_eq!(
        layout.targets.snapshot(),
        BTreeMap::from([
            (
                "ai.mistral.vibe".to_owned(),
                entry("MISTRAL_API_KEY", "port-key")
            ),
            (
                "mcp-oauth:docs:tokens@ai.mistral.vibe".to_owned(),
                entry("mcp-oauth:docs:tokens", "{\"access_token\":\"t\"}")
            ),
        ])
    );
    assert_eq!(
        layout.get(KEYRING_SERVICE, "mcp-oauth:docs:tokens"),
        Ok(Some("{\"access_token\":\"t\"}".to_owned())),
        "the moved account is read back through its compound target"
    );
    assert_eq!(
        layout.get(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Ok(Some("port-key".to_owned()))
    );
}

#[test]
fn a_key_a_prior_build_stored_is_found_and_moved_to_the_reference_layout() {
    let layout = WinVaultLayout::new(MemoryTargets::with(&[(
        "MISTRAL_API_KEY.ai.mistral.vibe",
        "MISTRAL_API_KEY",
        "prior-key",
    )]));
    assert_eq!(
        layout.get(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Ok(Some("prior-key".to_owned()))
    );
    assert_eq!(
        layout.targets.snapshot(),
        BTreeMap::from([(
            "ai.mistral.vibe".to_owned(),
            entry("MISTRAL_API_KEY", "prior-key")
        )])
    );
}

#[test]
fn a_delete_removes_only_what_the_account_owns() {
    let layout = WinVaultLayout::new(MemoryTargets::with(&[
        ("ai.mistral.vibe", "mcp-oauth:docs:tokens", "tokens"),
        ("MISTRAL_API_KEY@ai.mistral.vibe", "MISTRAL_API_KEY", "key"),
        (
            "MISTRAL_API_KEY.ai.mistral.vibe",
            "MISTRAL_API_KEY",
            "prior",
        ),
    ]));
    layout
        .delete(KEYRING_SERVICE, "MISTRAL_API_KEY")
        .expect("delete");
    assert_eq!(
        layout.targets.snapshot(),
        BTreeMap::from([(
            "ai.mistral.vibe".to_owned(),
            entry("mcp-oauth:docs:tokens", "tokens")
        )])
    );
    assert_eq!(
        layout.delete(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Err(KeyringFailure::NoEntry)
    );
}

#[test]
fn the_store_reads_a_prior_build_key_through_the_layout() {
    // The whole read path a launch takes: the current service answers through
    // the prior-build target and the key lands in the reference's layout.
    struct Shared(std::sync::Arc<WinVaultLayout<MemoryTargets>>);
    impl KeyringBackend for Shared {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
            self.0.get(service, account)
        }
        fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), KeyringFailure> {
            self.0.set(service, account, secret)
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure> {
            self.0.delete(service, account)
        }
    }
    let layout = std::sync::Arc::new(WinVaultLayout::new(MemoryTargets::with(&[(
        "MISTRAL_API_KEY.ai.mistral.vibe",
        "MISTRAL_API_KEY",
        "prior-key",
    )])));
    let store = KeyringStore::new(Box::new(Shared(layout.clone())));
    assert_eq!(
        store.get_api_key("MISTRAL_API_KEY"),
        Some("prior-key".to_owned())
    );
    assert_eq!(
        layout.targets.snapshot(),
        BTreeMap::from([(
            "ai.mistral.vibe".to_owned(),
            entry("MISTRAL_API_KEY", "prior-key")
        )])
    );
}

/// The live half: this port and the pinned reference's `keyring` share one
/// Windows credential manager, each reading what the other wrote, under a
/// service name of the test's own so no real credential is touched.
#[cfg(windows)]
#[test]
fn the_reference_and_the_port_read_each_other_s_secrets() {
    use std::process::Command;

    use super::NativeTargets;

    struct Cleanup(Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for target in &self.0 {
                let _ = NativeTargets.remove(target);
            }
        }
    }

    let root = crate::parity::reference_root();
    if let Some(reason) = crate::parity::off_pin_reason(&root, "keyring") {
        eprintln!("{reason}");
        return;
    }
    let Some(python) = crate::parity::pinned_interpreter(&root) else {
        eprintln!("skipping the live keyring probe: the reference has no interpreter");
        return;
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let service = format!("ai.mistral.vibe.parity-{}-{nanos}", std::process::id());
    let api_key = "MISTRAL_API_KEY";
    let tokens = "mcp-oauth:docs:tokens";
    let _cleanup = Cleanup(vec![
        service.clone(),
        format!("{api_key}@{service}"),
        format!("{tokens}@{service}"),
        format!("{api_key}.{service}"),
    ]);
    let layout = WinVaultLayout::new(NativeTargets);
    match layout.get(&service, api_key) {
        Err(KeyringFailure::NoBackend) => {
            eprintln!("skipping the live keyring probe: no credential manager on this host");
            return;
        }
        other => assert_eq!(other, Ok(None)),
    }
    let reference = |script: &str| {
        let output = Command::new(&python)
            .args(["-I", "-c", script, &service, api_key, tokens])
            .output()
            .expect("the reference interpreter runs");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .expect("utf-8 output")
            .trim()
            .to_owned()
    };

    layout
        .set(&service, api_key, "port-key")
        .expect("port write");
    assert_eq!(
        reference("import sys, keyring; print(keyring.get_password(sys.argv[1], sys.argv[2]))"),
        "port-key"
    );

    reference("import sys, keyring; keyring.set_password(sys.argv[1], sys.argv[3], 'tokens')");
    assert_eq!(layout.get(&service, tokens), Ok(Some("tokens".to_owned())));
    assert_eq!(
        layout.get(&service, api_key),
        Ok(Some("port-key".to_owned())),
        "the reference moved the port's key to its compound target"
    );

    layout
        .set(&service, api_key, "rotated-key")
        .expect("port rewrite");
    assert_eq!(
        reference(
            "import sys, keyring; print(keyring.get_password(sys.argv[1], sys.argv[2]), \
             keyring.get_password(sys.argv[1], sys.argv[3]))"
        ),
        "rotated-key tokens"
    );
    assert_eq!(
        NativeTargets.read(&format!("{api_key}.{service}")),
        Ok(None),
        "nothing is written under the target earlier builds used"
    );
}
