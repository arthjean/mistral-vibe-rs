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
            "{method} is a lifecycle method and must stay out of the inventory"
        );
    }
}

#[test]
fn local_extensions_stay_outside_the_reference_inventory() {
    assert!(
        LOCAL_EXTENSION_METHODS.is_sorted_by(|left, right| left < right),
        "LOCAL_EXTENSION_METHODS must stay sorted and duplicate-free for binary_search"
    );
    for method in LOCAL_EXTENSION_METHODS {
        assert!(
            !is_server_method(method),
            "{method} is a local extension and must stay out of SERVER_METHODS"
        );
        assert!(is_local_extension_method(method));
        assert!(is_dispatchable_method(method));
    }
    assert!(!is_local_extension_method("turn/start"));
    assert!(is_dispatchable_method("turn/start"));
    assert!(!is_dispatchable_method("turn/unknown"));
}
