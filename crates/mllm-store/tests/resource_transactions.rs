use mllm_store::Store;

#[test]
fn new_store_has_empty_epoch_zero() {
    let store = Store::open_in_memory().unwrap();
    let snapshot = store.resource_snapshot().unwrap();
    assert_eq!(snapshot.epoch, 0);
    assert!(snapshot.owners.is_empty());
}
