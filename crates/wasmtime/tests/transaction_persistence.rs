use tempfile::tempdir;

use wasmtime::_internal::transaction_persistence::{
    corrupt_first_log_crc, create_file_backed_region_image, publish_committed_global_object_root,
    publish_committed_struct_object, publish_committed_tmemory_update,
    reopen_and_recover_file_backed_region,
};

#[test]
fn file_backed_region_recovers_committed_tmemory_update() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tmemory-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_tmemory_update(&path, 1, 0x1000_0000_0000_0001, 1, &[1, 2, 3, 4]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.winners.len(), 1);
    assert_eq!(recovered.winners[0].logical_id, 0x1000_0000_0000_0001);
    assert_eq!(recovered.winners[0].version, 1);
}

#[test]
fn file_backed_region_rejects_corrupted_log_crc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corrupt-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_tmemory_update(&path, 1, 0x1000_0000_0000_0002, 1, &[9, 8, 7, 6]).unwrap();
    corrupt_first_log_crc(&path).unwrap();

    let err = reopen_and_recover_file_backed_region(&path).unwrap_err();
    assert!(err.to_string().contains("corrupt log entry"));
}

#[test]
fn file_backed_region_recovers_committed_struct_object() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[9, 8, 7]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 41);
    assert_eq!(recovered.object_winners[0].version, 1);
}

#[test]
fn file_backed_region_recovers_object_rooted_by_global() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rooted-object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[1, 2, 3]).unwrap();
    publish_committed_global_object_root(&path, 2, 41).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 41);
    assert_eq!(recovered.root_object_ids, vec![41]);
}
