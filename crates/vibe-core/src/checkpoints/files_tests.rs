//! What the port promises about absence, and what applying a plan does.

use std::cell::RefCell;
use std::collections::BTreeMap;

use super::files::{CheckpointFiles, FileAccessError, FileStore};
use super::lines::FileState;

/// A disk held in memory, with per-path failures a test can arm.
#[derive(Debug, Default)]
pub(super) struct FakeFiles {
    content: RefCell<BTreeMap<String, Vec<u8>>>,
    unreadable: RefCell<Vec<String>>,
    unwritable: RefCell<Vec<String>>,
}

impl FakeFiles {
    pub(super) fn new(entries: &[(&str, &str)]) -> Self {
        let files = Self::default();
        for (path, content) in entries {
            files.put(path, content);
        }
        files
    }

    pub(super) fn put(&self, path: &str, content: &str) {
        self.content
            .borrow_mut()
            .insert(path.to_owned(), content.as_bytes().to_vec());
    }

    pub(super) fn text_at(&self, path: &str) -> Option<String> {
        self.content
            .borrow()
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    pub(super) fn refuse_reads(&self, path: &str) {
        self.unreadable.borrow_mut().push(path.to_owned());
    }

    pub(super) fn refuse_writes(&self, path: &str) {
        self.unwritable.borrow_mut().push(path.to_owned());
    }
}

impl CheckpointFiles for &FakeFiles {
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, FileAccessError> {
        if self.unreadable.borrow().iter().any(|armed| armed == path) {
            return Err(FileAccessError::new("reading", path, "permission refused"));
        }
        Ok(self.content.borrow().get(path).cloned())
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), FileAccessError> {
        if self.unwritable.borrow().iter().any(|armed| armed == path) {
            return Err(FileAccessError::new("writing", path, "read-only"));
        }
        self.content
            .borrow_mut()
            .insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), FileAccessError> {
        if self.unwritable.borrow().iter().any(|armed| armed == path) {
            return Err(FileAccessError::new("deleting", path, "read-only"));
        }
        self.content.borrow_mut().remove(path);
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        self.content.borrow().contains_key(path)
    }
}

fn plan(entries: &[(&str, Option<&str>)]) -> Vec<(String, FileState)> {
    entries
        .iter()
        .map(|(path, content)| {
            (
                (*path).to_owned(),
                content.map_or_else(FileState::absent, FileState::from_text),
            )
        })
        .collect()
}

// -- US-131: the port ---------------------------------------------------------

#[test]
fn a_path_with_nothing_at_it_reads_as_absent_rather_than_failing() {
    let files = FakeFiles::new(&[("here", "content\n")]);
    let store = FileStore::new(&files);

    assert_eq!(
        store.read("gone").expect("absence is an answer"),
        FileState::absent()
    );
    assert_eq!(
        store.read("here").unwrap(),
        FileState::from_text("content\n")
    );
}

#[test]
fn a_path_that_exists_but_cannot_be_read_fails_rather_than_reporting_a_deletion() {
    let files = FakeFiles::new(&[("locked", "content\n")]);
    files.refuse_reads("locked");
    let store = FileStore::new(&files);

    let error = store
        .read("locked")
        .expect_err("an unreadable file is not an absent one");

    assert_eq!(error.path, "locked");
    assert!(error.to_string().contains("permission refused"), "{error}");
}

#[test]
fn applying_a_plan_deletes_absent_targets_writes_present_ones_and_skips_the_rest() {
    let files = FakeFiles::new(&[
        ("deleted", "goes away\n"),
        ("rewritten", "old\n"),
        ("already right", "unchanged\n"),
    ]);
    let store = FileStore::new(&files);

    let outcome = store.apply(&plan(&[
        ("deleted", None),
        ("rewritten", Some("new\n")),
        ("already right", Some("unchanged\n")),
        ("never existed", None),
    ]));

    assert_eq!(outcome.restored, vec!["deleted", "rewritten"]);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(files.text_at("deleted"), None);
    assert_eq!(files.text_at("rewritten").as_deref(), Some("new\n"));
}

#[test]
fn one_failing_path_is_reported_and_the_rest_of_the_plan_still_lands() {
    let files = FakeFiles::new(&[("blocked", "old\n"), ("open", "old\n")]);
    files.refuse_writes("blocked");
    let store = FileStore::new(&files);

    let outcome = store.apply(&plan(&[
        ("blocked", Some("new\n")),
        ("open", Some("new\n")),
    ]));

    assert_eq!(outcome.restored, vec!["open"]);
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].path, "blocked");
    assert_eq!(files.text_at("blocked").as_deref(), Some("old\n"));
    assert_eq!(files.text_at("open").as_deref(), Some("new\n"));
}

#[test]
fn only_the_paths_whose_content_differs_are_reported_as_diverging() {
    let files = FakeFiles::new(&[("same", "held\n"), ("moved", "held\n")]);
    let store = FileStore::new(&files);

    let diverging = store.diverging_paths(&plan(&[
        ("same", Some("held\n")),
        ("moved", Some("other\n")),
        ("gone", None),
    ]));

    assert_eq!(diverging, vec!["moved"]);
}
