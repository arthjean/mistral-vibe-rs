use super::*;

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}], "entryId": null, "annotations": {}})
}

fn enqueue(
    queue: &mut TurnQueue,
    text: &str,
    key: Option<&str>,
    id: &str,
) -> Result<(QueuedTurn, bool), QueueRefusal> {
    queue.enqueue(
        json!({"entries": [text], "idempotencyKey": key}),
        vec![user(text)],
        key,
        id.to_owned(),
        1,
    )
}

#[test]
fn a_repeated_key_answers_the_item_it_made_and_a_different_request_conflicts() {
    let mut queue = TurnQueue::default();
    let (first, created) = enqueue(&mut queue, "one", Some("k"), "a").expect("first enqueue");
    assert!(created);
    let (again, created) = enqueue(&mut queue, "one", Some("k"), "b").expect("retried enqueue");
    assert!(!created);
    assert_eq!(again.id, first.id);
    assert_eq!(
        enqueue(&mut queue, "two", Some("k"), "c"),
        Err(QueueRefusal::IdempotencyConflict("k".to_owned()))
    );
    // The key outlives the item: removing it does not forget the receipt.
    assert!(queue.remove("a"));
    assert!(
        !enqueue(&mut queue, "one", Some("k"), "d")
            .expect("receipt")
            .1
    );
}

#[test]
fn a_full_queue_refuses_and_a_replaced_item_keeps_its_place() {
    let mut queue = TurnQueue::default();
    for index in 0..MAX_ITEMS {
        enqueue(&mut queue, "item", None, &index.to_string()).expect("room left");
    }
    assert_eq!(
        enqueue(&mut queue, "item", None, "extra"),
        Err(QueueRefusal::Full)
    );
    let (replaced, _) = queue
        .replace("1", json!({}), vec![user("replaced")], None)
        .expect("known item");
    assert_eq!(replaced.id, "1");
    assert_eq!(
        queue.public()["items"][1]["entries"][0]["content"][0]["text"],
        "replaced"
    );
    assert_eq!(
        queue.replace("missing", json!({}), Vec::new(), None),
        Err(QueueRefusal::ItemNotFound("missing".to_owned()))
    );
}

#[test]
fn a_paused_queue_holds_its_items_until_it_resumes_or_empties() {
    let mut queue = TurnQueue::default();
    assert!(!queue.pause(), "an empty queue has nothing to pause");
    enqueue(&mut queue, "one", None, "a").expect("enqueue");
    assert!(queue.pause());
    assert!(queue.peek_next().is_none());
    assert!(queue.resume());
    assert!(!queue.resume());
    assert!(queue.pause());
    assert!(queue.remove("a"));
    assert_eq!(queue.public()["paused"], false);
}
