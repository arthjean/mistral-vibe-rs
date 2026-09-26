use super::*;

#[test]
fn method_inventory_is_sorted_and_unique() {
    assert!(
        SERVER_METHODS.is_sorted_by(|left, right| left < right),
        "SERVER_METHODS must stay sorted and duplicate-free for binary_search"
    );
    assert!(is_server_method("turn/start"));
    assert!(!is_server_method("turn/unknown"));
    for method in ["initialize", "initialized", "shutdown", "exit"] {
        assert!(
            !is_server_method(method),
            "{method} is a lifecycle frame and must stay out of the inventory"
        );
    }
}
