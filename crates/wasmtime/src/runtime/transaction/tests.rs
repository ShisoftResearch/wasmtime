use super::*;

fn with_transaction_memory_metadata(wasm: &[u8]) -> Vec<u8> {
    with_transaction_object_metadata(wasm, &[1, 1, 0, 0])
}

fn with_transaction_global_metadata(wasm: &[u8]) -> Vec<u8> {
    with_transaction_object_metadata(wasm, &[1, 0, 1, 0])
}

fn with_transaction_object_metadata(wasm: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut wasm = wasm.to_vec();
    wasm.extend_from_slice(&[
        0x00, 0x20, 0x1b, b's', b'h', b'i', b's', b'o', b'f', b't', b'.', b't', b'r', b'a', b'n',
        b's', b'a', b'c', b't', b'i', b'o', b'n', b'.', b'o', b'b', b'j', b'e', b'c', b't', b's',
    ]);
    wasm.extend_from_slice(payload);
    wasm
}

fn transaction_test_module(engine: &crate::Engine, wat: &str) -> crate::Module {
    crate::Module::new(engine, wat::parse_str(wat).unwrap()).unwrap()
}

fn count_retire_committed_linear_undo_attempts(
    events: &[persist::RecordingBackendEvent],
    chunk_start_block: u32,
) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::RetireCommittedLinearUndoChunk(chunk)
                    if *chunk == chunk_start_block
            )
        })
        .count()
}

fn count_retire_committed_linear_undo_failures(
    events: &[persist::RecordingBackendEvent],
    chunk_start_block: u32,
) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::RetireCommittedLinearUndoChunkFailed(chunk)
                    if *chunk == chunk_start_block
            )
        })
        .count()
}

#[test]
fn transaction_region_runtime_can_be_shared_between_stores_for_test() {
    let engine = crate::Engine::default();
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();

    let mut first = crate::Store::new(&engine, ());
    let mut second = crate::Store::new(&engine, ());

    first.set_transaction_region_runtime_for_test(runtime.clone());
    second.set_transaction_region_runtime_for_test(runtime);

    assert!(first.transaction_region_runtime_is_same_for_test(&second));
}

#[test]
fn stores_have_distinct_transaction_region_runtimes_by_default() {
    let engine = crate::Engine::default();
    let first = crate::Store::new(&engine, ());
    let second = crate::Store::new(&engine, ());

    assert!(!first.transaction_region_runtime_is_same_for_test(&second));
}

#[test]
fn shared_region_runtime_allocates_unique_transaction_ids_across_threads() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let barrier = Arc::new(Barrier::new(2));

    let first_runtime = runtime.clone();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_runtime.allocate_transaction_id_for_test().unwrap()
    });

    let second_runtime = runtime.clone();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_runtime.allocate_transaction_id_for_test().unwrap()
    });

    let first = first.join().unwrap();
    let second = second.join().unwrap();

    assert_ne!(first, second);
}

#[test]
fn shared_region_runtime_assigns_distinct_log_segments_to_threads() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let barrier = Arc::new(Barrier::new(2));

    let first_runtime = runtime.clone();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id()
    });

    let second_runtime = runtime.clone();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id()
    });

    let first = first.join().unwrap();
    let second = second.join().unwrap();

    assert_ne!(first, second);
}

#[test]
fn shared_region_runtime_reuses_released_log_segments() {
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();

    let first_runtime = runtime.clone();
    let first = thread::spawn(move || {
        let segment = first_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id();
        first_runtime
            .release_current_thread_log_segment_for_test()
            .unwrap();
        segment
    });
    let first = first.join().unwrap();

    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 1);

    let second_runtime = runtime.clone();
    let second = thread::spawn(move || {
        let segment = second_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id();
        second_runtime
            .release_current_thread_log_segment_for_test()
            .unwrap();
        segment
    });
    let second = second.join().unwrap();

    assert_eq!(second, first);
    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 1);
}

#[test]
fn shared_region_runtime_uses_thread_log_segments_for_tmemory_publication() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    clear_current_thread_transaction_for_test();

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let barrier = Arc::new(Barrier::new(2));

    let spawn_commit = |runtime: TransactionRegionRuntime,
                        barrier: Arc<Barrier>,
                        addr: u64,
                        new_bytes: [u8; 4]|
     -> thread::JoinHandle<(u32, u32, Vec<crate::vm::TxLogEntry>)> {
        thread::spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            let tmemory_path = dir.path().join("tmemory.bin");
            let mut tmemory = crate::runtime::vm::TMemory::new(
                TransactionConfig::with_file_backed_tmemory_path(tmemory_path).unwrap(),
                1,
                Some(1),
            )
            .unwrap();
            tmemory
                .commit_staged_tmemory_granule(0, &vec![0x11; TMEMORY_GRANULE_SIZE])
                .unwrap();

            let mut state = TransactionState::default();
            let transaction = state.begin_with_region_runtime(&runtime).unwrap();
            state
                .stage_tmemory_write_for_test(0, 0, addr, &new_bytes, &tmemory)
                .unwrap();

            barrier.wait();

            let stream_id = runtime
                .current_thread_log_segment_for_test()
                .unwrap()
                .stream_id();
            let txid = u32::try_from(transaction.as_raw()).unwrap();
            assert_ne!(stream_id, txid);
            assert!(state.commit_tmemory_for_test(&mut tmemory).unwrap());
            let entries = state.durable_log_entries_for_test(stream_id);
            state.clear_active().unwrap();

            (stream_id, txid, entries)
        })
    };

    let first = spawn_commit(runtime.clone(), barrier.clone(), 0, [1, 2, 3, 4]);
    let second = spawn_commit(
        runtime.clone(),
        barrier,
        u64::try_from(TMEMORY_GRANULE_SIZE).unwrap(),
        [5, 6, 7, 8],
    );

    let (first_stream_id, first_txid, first_entries) = first.join().unwrap();
    let (second_stream_id, second_txid, second_entries) = second.join().unwrap();

    assert_ne!(first_stream_id, second_stream_id);
    assert_ne!(first_txid, second_txid);
    assert_eq!(first_entries.len(), 1);
    assert_eq!(first_entries[0].tx_meta & 1, 1);
    assert_eq!(first_entries[0].tx_meta >> 1, first_txid);
    assert_eq!(
        first_entries[0].role().unwrap(),
        crate::vm::TxLogEntryRole::TMemoryUndo
    );
    assert_eq!(second_entries.len(), 1);
    assert_eq!(second_entries[0].tx_meta & 1, 1);
    assert_eq!(second_entries[0].tx_meta >> 1, second_txid);
    assert_eq!(
        second_entries[0].role().unwrap(),
        crate::vm::TxLogEntryRole::TMemoryUndo
    );

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_file_backed_tmemory_recovers_commits_from_two_stores() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let spawn_commit = |runtime: TransactionRegionRuntime,
                        barrier: Arc<Barrier>,
                        addr: i32,
                        value: i32|
     -> thread::JoinHandle<()> {
        thread::spawn(move || {
            clear_current_thread_transaction_for_test();

            let engine = crate::Engine::default();
            let module = transaction_test_module(
                &engine,
                r#"
                    (module
                      (tmemory 1)
                      (tfunc (export "write") (param i32 i32)
                        (i32.tstore (local.get 0) (local.get 1))))
                "#,
            );
            let mut store = crate::Store::new(&engine, ());
            store.set_transaction_region_runtime_for_test(runtime);
            let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
            let write = instance
                .get_typed_func::<(i32, i32), ()>(&mut store, "write")
                .unwrap();

            barrier.wait();
            write.call(&mut store, (addr, value)).unwrap();
            clear_current_thread_transaction_for_test();
        })
    };

    let first = spawn_commit(runtime.clone(), barrier.clone(), 0, 0x4433_2211);
    let second = spawn_commit(
        runtime.clone(),
        barrier,
        i32::try_from(TMEMORY_GRANULE_SIZE).unwrap(),
        i32::from_le_bytes([0x55, 0x66, 0x77, 0x08]),
    );
    first.join().unwrap();
    second.join().unwrap();

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened = reopened_store
        .transaction_recover_file_backed_tmemory_for_test(1, Some(1))
        .unwrap();

    assert_eq!(
        reopened.read_committed(0..4).unwrap(),
        vec![0x11, 0x22, 0x33, 0x44]
    );
    assert_eq!(
        reopened
            .read_committed(TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE + 4)
            .unwrap(),
        vec![0x55, 0x66, 0x77, 0x08]
    );

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_late_store_adopts_existing_file_backed_tmemory_after_grow() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0)))
              (tfunc (export "write") (param i32 i32)
                (i32.tstore (local.get 0) (local.get 1)))
              (tfunc (export "read") (param i32) (result i32)
                (i32.tload (local.get 0))))
        "#,
    );

    let grown_addr = 64 * 1024;
    let expected = 0x4433_2211;

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime.clone());
    let first_instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    let grow = first_instance
        .get_typed_func::<i32, i32>(&mut first_store, "grow")
        .unwrap();
    let write = first_instance
        .get_typed_func::<(i32, i32), ()>(&mut first_store, "write")
        .unwrap();
    assert_eq!(grow.call(&mut first_store, 1).unwrap(), 1);
    write
        .call(&mut first_store, (grown_addr, expected))
        .unwrap();

    let mut second_store = crate::Store::new(&engine, ());
    second_store.set_transaction_region_runtime_for_test(runtime);
    let second_instance = crate::Instance::new(&mut second_store, &module, &[]).unwrap();
    let size = second_instance
        .get_typed_func::<(), i32>(&mut second_store, "size")
        .unwrap();
    let read = second_instance
        .get_typed_func::<i32, i32>(&mut second_store, "read")
        .unwrap();

    assert_eq!(size.call(&mut second_store, ()).unwrap(), 2);
    assert_eq!(read.call(&mut second_store, grown_addr).unwrap(), expected);

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_shared_file_backed_tmemory_uses_physical_owner_key_across_stores() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let dummy_module = transaction_test_module(
        &engine,
        r#"
            (module
              (func (export "noop")))
        "#,
    );
    let tmemory_module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1 10)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0))))
        "#,
    );

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime.clone());
    let _dummy = crate::Instance::new(&mut first_store, &dummy_module, &[]).unwrap();
    let first_instance = crate::Instance::new(&mut first_store, &tmemory_module, &[]).unwrap();
    let first_grow = first_instance
        .get_typed_func::<i32, i32>(&mut first_store, "grow")
        .unwrap();
    assert_eq!(first_grow.call(&mut first_store, 1).unwrap(), 1);
    let first_instance_id = first_instance.id();
    drop(first_store);

    let mut second_store = crate::Store::new(&engine, ());
    second_store.set_transaction_region_runtime_for_test(runtime.clone());
    let second_instance = crate::Instance::new(&mut second_store, &tmemory_module, &[]).unwrap();
    let second_size = second_instance
        .get_typed_func::<(), i32>(&mut second_store, "size")
        .unwrap();
    let second_grow = second_instance
        .get_typed_func::<i32, i32>(&mut second_store, "grow")
        .unwrap();
    let second_instance_id = second_instance.id();

    assert_ne!(first_instance_id, second_instance_id);
    assert_eq!(second_size.call(&mut second_store, ()).unwrap(), 2);
    assert_eq!(second_grow.call(&mut second_store, 1).unwrap(), 2);
    drop(second_store);

    let recovered =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(
            &tx_log_path,
        )
        .unwrap();
    assert_eq!(
        recovered.committed_file_backed_tmemory_pages().unwrap(),
        Some(3)
    );
    assert_eq!(recovered.tmemory_size_winners.len(), 1);
    assert_eq!(recovered.tmemory_size_winners[0].owner_instance, None);
    assert_eq!(
        recovered.winners[0].logical_id,
        crate::runtime::vm::pack_tmemory_size_logical_id(None, 0).unwrap()
    );

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened_instance =
        crate::Instance::new(&mut reopened_store, &tmemory_module, &[]).unwrap();
    let reopened_size = reopened_instance
        .get_typed_func::<(), i32>(&mut reopened_store, "size")
        .unwrap();

    assert_eq!(reopened_size.call(&mut reopened_store, ()).unwrap(), 3);

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_store_adoption_waits_for_file_backed_tmemory_write_lock() {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::thread;
    use std::time::Duration;

    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(&engine, "(module (tmemory 1))");

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime.clone());
    let _instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    drop(first_store);

    let (ready_tx, ready_rx) = channel();
    let (done_tx, done_rx) = channel();
    let runtime_for_thread = runtime.clone();

    let mut handle = None;

    runtime
        .with_shared_file_backed_tmemory_commit_write_lock(|| {
            handle = Some(thread::spawn(move || {
                clear_current_thread_transaction_for_test();

                let engine = crate::Engine::default();
                let mut store = crate::Store::new(&engine, ());
                ready_tx.send(()).unwrap();
                store.set_transaction_region_runtime_for_test(runtime_for_thread);
                done_tx.send(()).unwrap();

                clear_current_thread_transaction_for_test();
            }));
            ready_rx.recv().unwrap();
            assert_eq!(
                done_rx.recv_timeout(Duration::from_millis(50)),
                Err(RecvTimeoutError::Timeout)
            );
            Ok(())
        })
        .unwrap();

    done_rx.recv().unwrap();
    handle.take().unwrap().join().unwrap();

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_restart_keeps_file_backed_tmemory_min_size_when_max_is_larger() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1 10)
              (tfunc (export "size") (result i32)
                (tmemory.size)))
        "#,
    );

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime);
    let first_instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    let size = first_instance
        .get_typed_func::<(), i32>(&mut first_store, "size")
        .unwrap();
    assert_eq!(size.call(&mut first_store, ()).unwrap(), 1);
    drop(first_store);

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened_instance = crate::Instance::new(&mut reopened_store, &module, &[]).unwrap();
    let reopened_size = reopened_instance
        .get_typed_func::<(), i32>(&mut reopened_store, "size")
        .unwrap();

    assert_eq!(reopened_size.call(&mut reopened_store, ()).unwrap(), 1);

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_restart_recovers_grown_file_backed_tmemory_size_with_max_limit() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1 10)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0)))
              (tfunc (export "write") (param i32 i32)
                (i32.tstore (local.get 0) (local.get 1)))
              (tfunc (export "read") (param i32) (result i32)
                (i32.tload (local.get 0))))
        "#,
    );

    let grown_addr = 64 * 1024;
    let expected = 0x5566_7788;

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime);
    let first_instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    let grow = first_instance
        .get_typed_func::<i32, i32>(&mut first_store, "grow")
        .unwrap();
    let write = first_instance
        .get_typed_func::<(i32, i32), ()>(&mut first_store, "write")
        .unwrap();
    assert_eq!(grow.call(&mut first_store, 1).unwrap(), 1);
    write
        .call(&mut first_store, (grown_addr, expected))
        .unwrap();
    drop(first_store);

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened_instance = crate::Instance::new(&mut reopened_store, &module, &[]).unwrap();
    let size = reopened_instance
        .get_typed_func::<(), i32>(&mut reopened_store, "size")
        .unwrap();
    let read = reopened_instance
        .get_typed_func::<i32, i32>(&mut reopened_store, "read")
        .unwrap();

    assert_eq!(size.call(&mut reopened_store, ()).unwrap(), 2);
    assert_eq!(
        read.call(&mut reopened_store, grown_addr).unwrap(),
        expected
    );

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_restart_recovers_grow_only_file_backed_tmemory_size_with_max_limit() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1 10)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0))))
        "#,
    );

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime);
    let first_instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    let grow = first_instance
        .get_typed_func::<i32, i32>(&mut first_store, "grow")
        .unwrap();
    assert_eq!(grow.call(&mut first_store, 1).unwrap(), 1);
    drop(first_store);

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened_instance = crate::Instance::new(&mut reopened_store, &module, &[]).unwrap();
    let size = reopened_instance
        .get_typed_func::<(), i32>(&mut reopened_store, "size")
        .unwrap();

    assert_eq!(size.call(&mut reopened_store, ()).unwrap(), 2);

    clear_current_thread_transaction_for_test();
}

#[cfg(all(unix, has_virtual_memory))]
#[test]
fn shared_region_runtime_restart_recovers_grown_file_backed_tmemory_size() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime =
        crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
            64,
        )
        .unwrap();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0)))
              (tfunc (export "write") (param i32 i32)
                (i32.tstore (local.get 0) (local.get 1)))
              (tfunc (export "read") (param i32) (result i32)
                (i32.tload (local.get 0))))
        "#,
    );

    let grown_addr = 64 * 1024;
    let expected = 0x1122_3344;

    let mut first_store = crate::Store::new(&engine, ());
    first_store.set_transaction_region_runtime_for_test(runtime);
    let first_instance = crate::Instance::new(&mut first_store, &module, &[]).unwrap();
    let grow = first_instance
        .get_typed_func::<i32, i32>(&mut first_store, "grow")
        .unwrap();
    let write = first_instance
        .get_typed_func::<(i32, i32), ()>(&mut first_store, "write")
        .unwrap();
    assert_eq!(grow.call(&mut first_store, 1).unwrap(), 1);
    write
        .call(&mut first_store, (grown_addr, expected))
        .unwrap();
    drop(first_store);

    let reopened_runtime =
        crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
            &tmemory_path,
            &tx_log_path,
        )
        .unwrap();
    let mut reopened_store = crate::Store::new(&engine, ());
    reopened_store.set_transaction_region_runtime_for_test(reopened_runtime);
    let reopened_instance = crate::Instance::new(&mut reopened_store, &module, &[]).unwrap();
    let size = reopened_instance
        .get_typed_func::<(), i32>(&mut reopened_store, "size")
        .unwrap();
    let read = reopened_instance
        .get_typed_func::<i32, i32>(&mut reopened_store, "read")
        .unwrap();

    assert_eq!(size.call(&mut reopened_store, ()).unwrap(), 2);
    assert_eq!(
        read.call(&mut reopened_store, grown_addr).unwrap(),
        expected
    );

    clear_current_thread_transaction_for_test();
}

#[test]
fn shared_region_runtime_pre_lp_failure_retires_log_segment() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42))))
        "#,
    );
    let initial_stream_id = runtime
        .current_thread_log_segment_for_test()
        .unwrap()
        .stream_id();
    let mut store = crate::Store::new(&engine, ());
    store.set_transaction_region_runtime_for_test(runtime.clone());
    store
        .transaction_create_file_backed_storage_for_test(tmemory_path, tx_log_path, 64)
        .unwrap();
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();

    store
        .transaction_state_mut()
        .fail_next_commit_before_lp_for_test();
    assert!(write.call(&mut store, ()).is_err());
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 0);

    let next_runtime = runtime.clone();
    let next = std::thread::spawn(move || {
        let stream_id = next_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id();
        next_runtime
            .release_current_thread_log_segment_for_test()
            .unwrap();
        stream_id
    });
    let next_stream_id = next.join().unwrap();

    assert_ne!(next_stream_id, initial_stream_id);
    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 1);

    clear_current_thread_transaction_for_test();
}

#[test]
fn shared_region_runtime_publication_failure_retires_log_segment() {
    use crate::runtime::transaction::persist::RecordingBackendEvent;

    clear_current_thread_transaction_for_test();

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let (durable_log, events) = TxDurableLog::recording_backend_with_flush_log_failure_for_test();

    let first_stream_id = {
        let mut state = TransactionState::default();
        let transaction = state.begin_with_region_runtime(&runtime).unwrap();
        let stream_id = runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id();
        let txid = u32::try_from(transaction.as_raw()).unwrap();
        state.durable_log = durable_log;
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_0042, 1, vec![1, 2, 3, 4]);

        let error = state
            .publish_tmemory_undo_before_in_place_write(stream_id, txid, &undo)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("recording backend flush log failure"),
            "{error:?}"
        );
        state.clear_active().unwrap();
        stream_id
    };

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&RecordingBackendEvent::AppendDataRecord(
        persist::DurableDataStream::TMemoryUndo
    )));
    assert!(events.contains(&RecordingBackendEvent::AppendLogEntry(
        crate::runtime::vm::TxLogEntryRole::TMemoryUndo
    )));
    assert!(events.contains(&RecordingBackendEvent::FlushLog));
    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 0);

    let next_runtime = runtime.clone();
    let next = std::thread::spawn(move || {
        let stream_id = next_runtime
            .current_thread_log_segment_for_test()
            .unwrap()
            .stream_id();
        next_runtime
            .release_current_thread_log_segment_for_test()
            .unwrap();
        stream_id
    });
    let next_stream_id = next.join().unwrap();

    assert_ne!(next_stream_id, first_stream_id);
    assert_eq!(runtime.thread_log_segment_count_for_test().unwrap(), 0);
    assert_eq!(runtime.free_log_segment_count_for_test().unwrap(), 1);
    clear_current_thread_transaction_for_test();
}

#[test]
fn shared_region_runtime_commit_path_uses_transaction_id_in_tmemory_undo_tx_meta() {
    clear_current_thread_transaction_for_test();

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    store.set_transaction_region_runtime_for_test(runtime.clone());
    store
        .transaction_create_file_backed_storage_for_test(tmemory_path, tx_log_path, 64)
        .unwrap();
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    let stream_id = runtime
        .current_thread_log_segment_for_test()
        .unwrap()
        .stream_id();
    let next_txid = runtime.allocate_transaction_id_for_test().unwrap();
    let txid = u32::try_from(next_txid.as_raw() - 1).unwrap();
    let entries = store
        .transaction_state()
        .durable_log_entries_for_test(stream_id);

    assert_eq!(entries.len(), 1);
    assert_ne!(stream_id, txid);
    assert_eq!(entries[0].tx_meta & 1, 1);
    assert_eq!(entries[0].tx_meta >> 1, txid);
    assert_eq!(
        entries[0].role().unwrap(),
        crate::vm::TxLogEntryRole::TMemoryUndo
    );

    clear_current_thread_transaction_for_test();
}

#[test]
fn shared_region_runtime_detects_cross_thread_write_conflict() {
    use crate::runtime::transaction::{GranuleId, TransactionId};
    use std::sync::mpsc;
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let granule = GranuleId::TMemory {
        instance: None,
        memory_index: 0,
        granule_index: 7,
    };
    let (owned_tx, owned_rx) = mpsc::channel();
    let (attempted_tx, attempted_rx) = mpsc::channel();

    let first_runtime = runtime.clone();
    let first = thread::spawn(move || {
        first_runtime
            .acquire_granule_write_for_test(TransactionId::from_raw(1), granule, 0)
            .unwrap();
        owned_tx.send(()).unwrap();
        attempted_rx.recv().unwrap();
        first_runtime
            .release_transaction_for_test(TransactionId::from_raw(1))
            .unwrap();
    });

    owned_rx.recv().unwrap();
    let second_runtime = runtime.clone();
    let second = thread::spawn(move || {
        let result =
            second_runtime.acquire_granule_write_for_test(TransactionId::from_raw(2), granule, 0);
        attempted_tx.send(()).unwrap();
        result.is_err()
    });

    assert!(second.join().unwrap());
    first.join().unwrap();
}

#[test]
fn shared_transaction_states_detect_cross_thread_write_conflict() {
    use crate::runtime::transaction::{GranuleId, TransactionState};
    use std::sync::mpsc;
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let granule = GranuleId::TGlobal {
        instance: Some(1),
        global_index: 7,
    };
    let (owned_tx, owned_rx) = mpsc::channel();
    let (attempted_tx, attempted_rx) = mpsc::channel();

    let first_runtime = runtime.clone();
    let first = thread::spawn(move || {
        let mut state = TransactionState::default();
        state.begin_with_region_runtime(&first_runtime).unwrap();
        state.acquire_granule_write(granule, 0).unwrap();
        owned_tx.send(()).unwrap();
        attempted_rx.recv().unwrap();
        state.abort().unwrap();
    });

    owned_rx.recv().unwrap();
    let second_runtime = runtime.clone();
    let second = thread::spawn(move || {
        let mut state = TransactionState::default();
        state.begin_with_region_runtime(&second_runtime).unwrap();
        let result = state.acquire_granule_write(granule, 0);
        attempted_tx.send(()).unwrap();
        if result.is_err() {
            state.abort().unwrap();
            return true;
        }
        state.abort().unwrap();
        false
    });

    assert!(second.join().unwrap());
    first.join().unwrap();
}

#[test]
fn versioned_granule_version_ignores_poisoned_shared_runtime() {
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let poisoned_runtime = runtime.clone();
    let _ = thread::spawn(move || {
        let _guard = poisoned_runtime.lock().unwrap();
        panic!("poison shared runtime");
    })
    .join();

    let state = TransactionState {
        shared_region_runtime: Some(runtime),
        ..TransactionState::default()
    };

    assert_eq!(
        state.versioned_granule_version(global_granule_id(None, 0)),
        0
    );
}

#[test]
fn shared_region_runtime_conflict_abort_before_terminal_commit_frees_active_allocations() {
    use std::sync::mpsc;
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let (owned_tx, owned_rx) = mpsc::channel();
    let (preempted_tx, preempted_rx) = mpsc::channel();

    let younger_runtime = runtime.clone();
    let younger = thread::spawn(move || {
        let mut objects = ObjectTable::default();
        let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
        let mut state = TransactionState::default();
        state.shared_region_runtime = Some(younger_runtime);
        state
            .enter_transaction(TransactionId::from_raw(100_001))
            .unwrap();
        state.record_allocated_object(allocated).unwrap();
        state.stage_global(0, GlobalSnapshot::I32(100)).unwrap();
        owned_tx.send(()).unwrap();
        preempted_rx.recv().unwrap();

        let error = state
            .begin_terminal_commit_with_cleanup_for_test(&mut objects)
            .unwrap_err();
        assert!(error.to_string().contains("conflict-aborted"), "{error:?}");
        assert_eq!(state.active_transaction(), None);
        assert!(objects.kind(allocated).is_err());
    });

    owned_rx.recv().unwrap();
    let older_runtime = runtime.clone();
    let older = thread::spawn(move || {
        let mut state = TransactionState::default();
        state.shared_region_runtime = Some(older_runtime);
        state.enter_transaction(TransactionId::from_raw(1)).unwrap();
        state.stage_global(0, GlobalSnapshot::I32(200)).unwrap();
        preempted_tx.send(()).unwrap();
        state.abort().unwrap();
    });

    younger.join().unwrap();
    older.join().unwrap();
}

#[test]
fn shared_region_runtime_terminal_owner_cannot_be_preempted() {
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let owner = TransactionId::from_raw(100_001);
    let older = TransactionId::from_raw(1);
    let granule = global_granule_id(None, 0);

    runtime
        .acquire_granule_write_for_test(owner, granule, 0)
        .unwrap();
    runtime.begin_terminal_commit_for_test(owner).unwrap();

    let error = runtime
        .acquire_granule_write_for_test(older, granule, 0)
        .unwrap_err();
    assert!(
        error.to_string().contains("transaction write conflict"),
        "{error:?}"
    );
    assert!(
        !runtime
            .take_conflict_aborted_transaction_for_test(owner)
            .unwrap()
    );
}

#[test]
fn shared_region_runtime_release_clears_terminal_marker() {
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let mut state = TransactionState::default();
    let transaction = state.begin_with_region_runtime(&runtime).unwrap();
    state
        .acquire_granule_write(global_granule_id(None, 0), 0)
        .unwrap();
    runtime.begin_terminal_commit_for_test(transaction).unwrap();
    state.abort().unwrap();

    assert!(
        !runtime
            .transaction_is_terminal_commit_for_test(transaction)
            .unwrap()
    );
}

#[test]
fn shared_region_runtime_poisoned_clear_active_returns_error_without_panic() {
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let poisoned_runtime = runtime.clone();
    let _ = thread::spawn(move || {
        let _guard = poisoned_runtime.lock().unwrap();
        panic!("poison shared runtime");
    })
    .join();

    let transaction = TransactionId::from_raw(7);
    let mut state = TransactionState {
        active: Some(transaction),
        shared_region_runtime: Some(runtime),
        terminal_commit_active: true,
        ..TransactionState::default()
    };

    let error = state.clear_active().unwrap_err();
    assert!(error.to_string().contains("lock poisoned"), "{error:?}");
}

#[test]
fn shared_region_runtime_cleanup_failure_still_clears_local_state() {
    clear_current_thread_transaction_for_test();

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let mut state = TransactionState::default();

    let first = state.begin_with_region_runtime(&runtime).unwrap();
    state.stage_global(0, GlobalSnapshot::I32(1)).unwrap();
    runtime.fail_release_transaction_once_for_test();

    let error = state.clear_active().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected release transaction failure"),
        "{error:?}"
    );
    assert_eq!(state.active_transaction(), None);
    assert_eq!(current_thread_transaction_for_test(), None);

    let second = state.begin_with_region_runtime(&runtime).unwrap();
    assert_ne!(second, first);
    assert_eq!(current_thread_transaction_for_test(), Some(second));
    assert!(state.staged_records().unwrap().is_empty());

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn mock_transaction_store_commits_to_tmemory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 42);
}

#[test]
fn mock_transaction_store8_commits_to_tmemory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore8 (i32.const 8) (i32.const 0xab)))
              (tfunc (export "read") (result i32)
                (i32.tload8_u (i32.const 8))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0xab);
}

#[test]
fn mock_transaction_simd_store_commits_to_tmemory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore))
              (tfunc (export "read_lane0") (result i32)
                (i32.const 0)
                (v128.tload)
                (i32x4.extract_lane 0)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read_lane0 = instance
        .get_typed_func::<(), i32>(&mut store, "read_lane0")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read_lane0.call(&mut store, ()).unwrap(), 287454020);
}

#[test]
fn mock_transaction_simd_lane_store_commits_to_tmemory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write_lane")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore32_lane 1))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_lane = instance
        .get_typed_func::<(), ()>(&mut store, "write_lane")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write_lane.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 1432778632);
}

#[test]
fn mock_transaction_tfunc_store_commits_on_return() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 42);
}

#[test]
fn mock_transaction_nested_tfunc_reuses_active_transaction_until_outer_return() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc $inner
                (i32.tstore (i32.const 0) (i32.const 1)))
              (tfunc (export "outer_fail")
                (call $inner)
                (i32.tstore (i32.const 0) (i32.const 2))
                (tfail))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let outer_fail = instance
        .get_typed_func::<(), ()>(&mut store, "outer_fail")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    outer_fail.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0);
}

#[test]
fn mock_transaction_fail_discards_memory_write() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (func (export "write_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_fail = instance
        .get_typed_func::<(), ()>(&mut store, "write_fail")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write_fail.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0);
}

#[test]
fn mock_transaction_global_i32_set_commits_to_backing_global() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (mut i32) (i32.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 42)))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 42);
}

#[test]
fn mock_transaction_global_i64_fail_discards_staged_write() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (mut i64) (i64.const 7))
              (func (export "write_fail")
                (ttry)
                (tglobal.set $g (i64.const 99))
                (tfail))
              (func (export "read") (result i64)
                (global.get $g)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_fail = instance
        .get_typed_func::<(), ()>(&mut store, "write_fail")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i64>(&mut store, "read")
        .unwrap();

    write_fail.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 7);
}

#[test]
fn mock_transaction_global_get_observes_staged_i32_write() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (mut i32) (i32.const 1))
              (func (export "write_read") (result i32)
                (ttry)
                (tglobal.set $g (i32.const 77))
                (tglobal.get $g)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_read = instance
        .get_typed_func::<(), i32>(&mut store, "write_read")
        .unwrap();

    assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
}

#[test]
fn mock_transaction_global_imported_tglobal_commits_to_backing_global() {
    let engine = crate::Engine::default();
    let provider = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (export "g") (mut i32) (i32.const 5))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
    );
    let consumer = transaction_test_module(
        &engine,
        r#"
            (module
              (import "env" "g" (tglobal $g (mut i32)))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 11))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
    let global = provider.get_global(&mut store, "g").unwrap();
    let consumer = crate::Instance::new(&mut store, &consumer, &[global.into()]).unwrap();
    let write = consumer
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = provider
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 11);
}

#[test]
fn mock_transaction_global_float_sets_commit_bitwise() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $f32 (mut f32) (f32.const 0))
              (tglobal $f64 (mut f64) (f64.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $f32 (f32.const -13.5))
                (tglobal.set $f64 (f64.const 42.25)))
              (func (export "read_f32_bits") (result i32)
                (i32.reinterpret_f32 (global.get $f32)))
              (func (export "read_f64_bits") (result i64)
                (i64.reinterpret_f64 (global.get $f64))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read_f32_bits = instance
        .get_typed_func::<(), i32>(&mut store, "read_f32_bits")
        .unwrap();
    let read_f64_bits = instance
        .get_typed_func::<(), i64>(&mut store, "read_f64_bits")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(
        read_f32_bits.call(&mut store, ()).unwrap() as u32,
        (-13.5f32).to_bits()
    );
    assert_eq!(
        read_f64_bits.call(&mut store, ()).unwrap() as u64,
        42.25f64.to_bits()
    );
}

#[test]
fn mock_transaction_global_v128_get_reads_defined_global() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 287454020 1432778632 16909060 84281096))
              (tfunc (export "read_lane1") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 1)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let read_lane1 = instance
        .get_typed_func::<(), i32>(&mut store, "read_lane1")
        .unwrap();

    assert_eq!(read_lane1.call(&mut store, ()).unwrap(), 1432778632);
}

#[test]
fn mock_transaction_global_v128_set_commits_bitwise() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 0 0 0 0))
              (tfunc (export "write")
                (tglobal.set $g (v128.const i32x4 287454020 1432778632 16909060 84281096)))
              (tfunc (export "read_lane2") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 2)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read_lane2 = instance
        .get_typed_func::<(), i32>(&mut store, "read_lane2")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read_lane2.call(&mut store, ()).unwrap(), 16909060);
}

#[test]
fn mock_transaction_plain_func_memory_size_requires_active_transaction() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 2)
              (func (export "size") (result i32)
                (tmemory.size)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let size = instance
        .get_typed_func::<(), i32>(&mut store, "size")
        .unwrap();

    let error = size.call(&mut store, ()).unwrap_err();

    assert!(format!("{error:?}").contains("transaction operation requires an active transaction"));
}

#[test]
fn mock_transaction_tfunc_size_and_grow_commit_against_tmemory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 0 1)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let size = instance
        .get_typed_func::<(), i32>(&mut store, "size")
        .unwrap();
    let grow = instance
        .get_typed_func::<i32, i32>(&mut store, "grow")
        .unwrap();

    assert_eq!(size.call(&mut store, ()).unwrap(), 0);
    assert_eq!(grow.call(&mut store, 1).unwrap(), 0);
    assert_eq!(size.call(&mut store, ()).unwrap(), 1);
}

#[test]
fn mock_transaction_ttable_funcref_paths_hit_runtime_libcalls() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (ttable $t 1 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "size") (result i32)
                (ttable.size $t))
              (tfunc (export "grow") (result i32)
                (ref.null func)
                (i32.const 2)
                (ttable.grow $t))
              (tfunc (export "is_null") (result i32)
                (i32.const 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "set")
                (i32.const 0)
                (ref.func $target)
                (ttable.set $t)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let size = instance
        .get_typed_func::<(), i32>(&mut store, "size")
        .unwrap();
    let grow = instance
        .get_typed_func::<(), i32>(&mut store, "grow")
        .unwrap();
    let is_null = instance
        .get_typed_func::<(), i32>(&mut store, "is_null")
        .unwrap();
    let set = instance
        .get_typed_func::<(), ()>(&mut store, "set")
        .unwrap();

    assert_eq!(size.call(&mut store, ()).unwrap(), 1);
    assert_eq!(is_null.call(&mut store, ()).unwrap(), 1);
    set.call(&mut store, ()).unwrap();
    assert_eq!(is_null.call(&mut store, ()).unwrap(), 0);
    assert_eq!(grow.call(&mut store, ()).unwrap(), 1);
    assert_eq!(size.call(&mut store, ()).unwrap(), 3);
}

#[test]
fn transaction_ttable_set_is_private_until_commit() {
    use crate::{Caller, Func, Linker, Ref};
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (import "host" "observe" (func $observe))
              (ttable $t (export "t") 1 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "set_then_observe")
                (i32.const 0)
                (ref.func $target)
                (ttable.set $t)
                (call $observe)))
            "#,
    );
    let mut linker = Linker::new(&engine);
    let mut store = crate::Store::new(&engine, ());
    let observed_committed_null = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&observed_committed_null);
    let observe = Func::wrap(&mut store, move |mut caller: Caller<'_, ()>| {
        let table = caller
            .get_export("t")
            .and_then(|export| export.into_table())
            .expect("exported ttable");
        let value = table.get(&mut caller, 0).expect("table element");
        observed.store(matches!(value, Ref::Func(None)), Ordering::SeqCst);
    });
    linker
        .define(&mut store, "host", "observe", observe)
        .unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let table = instance.get_table(&mut store, "t").unwrap();
    let set_then_observe = instance
        .get_typed_func::<(), ()>(&mut store, "set_then_observe")
        .unwrap();

    assert!(matches!(table.get(&mut store, 0).unwrap(), Ref::Func(None)));
    set_then_observe.call(&mut store, ()).unwrap();
    assert!(observed_committed_null.load(Ordering::SeqCst));
    assert!(matches!(
        table.get(&mut store, 0).unwrap(),
        Ref::Func(Some(_))
    ));
}

#[test]
fn transaction_ttable_grow_new_region_uses_staged_overlay() {
    use crate::Ref;

    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (ttable $t (export "t") 1 3 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "grow_get_set_new") (result i32)
                (ref.null func)
                (i32.const 1)
                (ttable.grow $t)
                (drop)
                (i32.const 1)
                (ttable.get $t)
                (ref.is_null)
                (i32.const 1)
                (ref.func $target)
                (ttable.set $t)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let table = instance.get_table(&mut store, "t").unwrap();
    let grow_get_set_new = instance
        .get_typed_func::<(), i32>(&mut store, "grow_get_set_new")
        .unwrap();

    assert_eq!(table.size(&mut store), 1);
    assert_eq!(grow_get_set_new.call(&mut store, ()).unwrap(), 1);
    assert_eq!(table.size(&mut store), 2);
    assert!(matches!(
        table.get(&mut store, 1).unwrap(),
        Ref::Func(Some(_))
    ));
}

#[test]
fn transaction_ttable_grow_is_private_until_commit() {
    use crate::{Caller, Func, Linker, Ref};
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (import "host" "observe" (func $observe))
              (ttable $t (export "t") 1 3 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "grow_then_observe")
                (ref.null func)
                (i32.const 1)
                (ttable.grow $t)
                (drop)
                (i32.const 1)
                (ttable.get $t)
                (drop)
                (i32.const 1)
                (ref.func $target)
                (ttable.set $t)
                (call $observe)))
            "#,
    );
    let mut linker = Linker::new(&engine);
    let mut store = crate::Store::new(&engine, ());
    let observed_size = Arc::new(AtomicU64::new(u64::MAX));
    let observed_new_slot_absent = Arc::new(AtomicBool::new(false));
    let observed_size_for_host = Arc::clone(&observed_size);
    let observed_slot_for_host = Arc::clone(&observed_new_slot_absent);
    let observe = Func::wrap(&mut store, move |mut caller: Caller<'_, ()>| {
        let table = caller
            .get_export("t")
            .and_then(|export| export.into_table())
            .expect("exported ttable");
        observed_size_for_host.store(table.size(&mut caller), Ordering::SeqCst);
        observed_slot_for_host.store(table.get(&mut caller, 1).is_none(), Ordering::SeqCst);
    });
    linker
        .define(&mut store, "host", "observe", observe)
        .unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let table = instance.get_table(&mut store, "t").unwrap();
    let grow_then_observe = instance
        .get_typed_func::<(), ()>(&mut store, "grow_then_observe")
        .unwrap();

    assert_eq!(table.size(&mut store), 1);
    grow_then_observe.call(&mut store, ()).unwrap();
    assert_eq!(observed_size.load(Ordering::SeqCst), 1);
    assert!(observed_new_slot_absent.load(Ordering::SeqCst));
    assert_eq!(table.size(&mut store), 2);
    assert!(matches!(
        table.get(&mut store, 1).unwrap(),
        Ref::Func(Some(_))
    ));
}

#[test]
fn mock_transaction_ttable_bulk_funcref_paths_hit_runtime_libcalls() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (table $t 4 tfuncref)
              (elem $e func $target)
              (func $target)
              (tfunc (export "is_null") (param i32) (result i32)
                (local.get 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "fill")
                (i32.const 0)
                (ref.func $target)
                (i32.const 1)
                (ttable.fill $t))
              (tfunc (export "copy")
                (i32.const 1)
                (i32.const 0)
                (i32.const 1)
                (ttable.copy $t $t))
              (tfunc (export "init")
                (i32.const 2)
                (i32.const 0)
                (i32.const 1)
                (ttable.init $t $e)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let is_null = instance
        .get_typed_func::<i32, i32>(&mut store, "is_null")
        .unwrap();
    let fill = instance
        .get_typed_func::<(), ()>(&mut store, "fill")
        .unwrap();
    let copy = instance
        .get_typed_func::<(), ()>(&mut store, "copy")
        .unwrap();
    let init = instance
        .get_typed_func::<(), ()>(&mut store, "init")
        .unwrap();

    assert_eq!(is_null.call(&mut store, 0).unwrap(), 1);
    assert_eq!(is_null.call(&mut store, 1).unwrap(), 1);
    assert_eq!(is_null.call(&mut store, 2).unwrap(), 1);
    fill.call(&mut store, ()).unwrap();
    assert_eq!(is_null.call(&mut store, 0).unwrap(), 0);
    copy.call(&mut store, ()).unwrap();
    assert_eq!(is_null.call(&mut store, 1).unwrap(), 0);
    init.call(&mut store, ()).unwrap();
    assert_eq!(is_null.call(&mut store, 2).unwrap(), 0);
}

#[test]
fn module_compilation_rejects_transaction_table_get_on_ordinary_table() {
    let engine = crate::Engine::default();
    let error = crate::Module::new(
        &engine,
        wat::parse_str(
            r#"
                (module
                  (table $t 1 funcref)
                  (func (result i32)
                    (i32.const 0)
                    (ttable.get $t)
                    (ref.is_null)))
                "#,
        )
        .unwrap(),
    )
    .unwrap_err();
    let error = format!("{error:?}");
    assert!(
        error.contains("transactional table operator requires ttable"),
        "{error}"
    );
}

#[test]
fn mock_transaction_static_tdata_initializes_tmemory_sidecar() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tdata (i32.const 0) "abcdefgh")
              (tfunc (export "read") (result i64)
                (i64.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let read = instance
        .get_typed_func::<(), i64>(&mut store, "read")
        .unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0x6867_6665_6463_6261);
}

#[test]
fn mock_transaction_read_after_write_uses_pending_store_scratch() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (func (export "write_read") (result i32)
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 77))
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_read = instance
        .get_typed_func::<(), i32>(&mut store, "write_read")
        .unwrap();

    assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
}

#[test]
fn mock_transaction_tmemory_trap_clears_active_transaction() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (tfunc (export "trap")
                (i32.tstore (i32.const 0) (i32.const 42))
                (drop (i32.tload (i32.const 65536))))
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 7)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let trap = instance
        .get_typed_func::<(), ()>(&mut store, "trap")
        .unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    assert!(trap.call(&mut store, ()).is_err());
    assert_eq!(read.call(&mut store, ()).unwrap(), 0);

    write.call(&mut store, ()).unwrap();
    assert_eq!(read.call(&mut store, ()).unwrap(), 7);
}

#[test]
fn mock_transaction_plain_wasm_trap_clears_active_transaction() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $sig (func))
              (tmemory 1)
              (table 0 funcref)
              (tfunc (export "trap_after_tload")
                (drop (i32.tload (i32.const 0)))
                (call_indirect (type $sig) (i32.const 0)))
              (tfunc (export "recover") (result i32)
                (i32.tload (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let trap_after_tload = instance
        .get_typed_func::<(), ()>(&mut store, "trap_after_tload")
        .unwrap();
    let recover = instance
        .get_typed_func::<(), i32>(&mut store, "recover")
        .unwrap();

    trap_after_tload.call(&mut store, ()).unwrap_err();

    assert_eq!(recover.call(&mut store, ()).unwrap(), 0);
}

#[test]
fn mock_transaction_unexecuted_ttry_does_not_commit_caller_transaction() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory 1)
              (func $maybe_begin (param i32)
                (local.get 0)
                (if
                  (then
                    (ttry))))
              (func (export "write_then_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (call $maybe_begin (i32.const 0))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_then_fail = instance
        .get_typed_func::<(), ()>(&mut store, "write_then_fail")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    write_then_fail.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0);
}

#[test]
fn mock_transaction_store_uses_defined_memory_index_after_import() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (import "env" "ordinary" (memory 1))
              (tmemory $tx 1)
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 55)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let ordinary = crate::Memory::new(&mut store, crate::MemoryType::new(1, None)).unwrap();
    let instance = crate::Instance::new(&mut store, &module, &[ordinary.into()]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read_tx = instance
        .get_typed_func::<(), i32>(&mut store, "read_tx")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read_tx.call(&mut store, ()).unwrap(), 55);
}

#[test]
fn mock_transaction_store_uses_imported_tmemory_vmctx() {
    let engine = crate::Engine::default();
    let provider = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
    );
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (import "env" "tx" (tmemory $tx 1))
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 66)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
    let tx = provider.get_memory(&mut store, "tx").unwrap();
    let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
    let write = instance
        .get_typed_func::<(), ()>(&mut store, "write")
        .unwrap();
    let read_tx = instance
        .get_typed_func::<(), i32>(&mut store, "read_tx")
        .unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read_tx.call(&mut store, ()).unwrap(), 66);
}

#[test]
fn mock_transaction_distinguishes_imported_and_local_tmemory_overlays() {
    let engine = crate::Engine::default();
    let provider = transaction_test_module(
        &engine,
        r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
    );
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (import "env" "tx" (tmemory $imported 1))
              (tmemory $local 1)
              (tfunc (export "write_both")
                (i32.tstore $imported (i32.const 0) (i32.const 11))
                (i32.tstore $local (i32.const 0) (i32.const 22)))
              (tfunc (export "read_imported") (result i32)
                (i32.tload $imported (i32.const 0)))
              (tfunc (export "read_local") (result i32)
                (i32.tload $local (i32.const 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
    let tx = provider.get_memory(&mut store, "tx").unwrap();
    let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
    let write_both = instance
        .get_typed_func::<(), ()>(&mut store, "write_both")
        .unwrap();
    let read_imported = instance
        .get_typed_func::<(), i32>(&mut store, "read_imported")
        .unwrap();
    let read_local = instance
        .get_typed_func::<(), i32>(&mut store, "read_local")
        .unwrap();

    write_both.call(&mut store, ()).unwrap();

    assert_eq!(read_imported.call(&mut store, ()).unwrap(), 11);
    assert_eq!(read_local.call(&mut store, ()).unwrap(), 22);
}

#[test]
fn default_transaction_config_uses_minimal_vmemory_runtime() {
    let config = TransactionConfig::default();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::VMemory);
    assert_eq!(
        config.tmemory_persistence_mode(),
        TMemoryPersistenceMode::ResearchPretendPmem
    );
    assert_eq!(config.concurrency_control(), ConcurrencyControl::LockBased);
    assert_eq!(
        config.durability_policy(),
        DurabilityPolicy::VolatileRollbackOnly
    );
    assert_eq!(
        config.conflict_policy(),
        ConflictPolicy::AbortOrWizardDefault
    );
    assert_eq!(
        config.object_index_persistence_policy(),
        ObjectIndexPersistencePolicy::RebuildOnRecovery
    );
}

#[test]
fn transaction_config_rejects_generic_file_backed_backend_selection() {
    assert!(
        TransactionConfig::with_tmemory_backend(TMemoryBackend::VMemory)
            .unwrap()
            .is_vmemory_only()
    );

    let error =
        TransactionConfig::with_tmemory_backend(TMemoryBackend::FileBackedMemory).unwrap_err();
    assert!(error.to_string().contains("requires explicit file backing"));
}

#[test]
fn transaction_config_accepts_file_backed_temp_mode() {
    let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(
        config.tmemory_file_backing(),
        Some(TMemoryFileBacking::Temp)
    );
    assert!(!config.is_vmemory_only());
}

#[test]
fn transaction_config_accepts_file_backed_path_mode() {
    let path = std::path::PathBuf::from("/tmp/wasmtime-transaction-file-backed-test.tmemory");
    let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(
        config.tmemory_file_backing(),
        Some(TMemoryFileBacking::Path(path))
    );
    assert!(!config.is_vmemory_only());
}

#[test]
fn generic_file_backed_backend_selection_still_requires_file_mode() {
    let error =
        TransactionConfig::with_tmemory_backend(TMemoryBackend::FileBackedMemory).unwrap_err();
    assert!(error.to_string().contains("requires explicit file backing"));
}

#[test]
fn transaction_config_accepts_nvmemory_backend() {
    let config = TransactionConfig::with_tmemory_backend(TMemoryBackend::NVMemory).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::NVMemory);
    assert_eq!(
        config.tmemory_persistence_mode(),
        TMemoryPersistenceMode::ResearchPretendPmem
    );
    assert!(!config.is_vmemory_only());
}

#[test]
fn transaction_config_accepts_nvmemory_hardware_persistence_mode() {
    let config = TransactionConfig::with_nvmemory_persistence_mode(
        TMemoryPersistenceMode::RequireHardwarePmem,
    )
    .unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::NVMemory);
    assert_eq!(
        config.tmemory_persistence_mode(),
        TMemoryPersistenceMode::RequireHardwarePmem
    );
    assert!(!config.is_vmemory_only());
}

#[test]
fn transaction_config_defaults_to_rebuild_on_recovery_object_index_policy() {
    let config = TransactionConfig::default();
    assert_eq!(
        config.object_index_persistence_policy(),
        ObjectIndexPersistencePolicy::RebuildOnRecovery
    );
}

#[test]
fn transaction_config_rejects_persistent_index_policy_for_now() {
    let error = TransactionConfig::with_object_index_persistence_policy(
        ObjectIndexPersistencePolicy::PersistentIndex,
    )
    .unwrap_err();
    assert!(error.to_string().contains("PersistentIndex"));
}

#[test]
fn begin_commit_and_abort_clear_active_transaction() {
    clear_current_thread_transaction_for_test();
    let mut state = TransactionState::default();

    let first = state.begin().unwrap();
    assert_eq!(state.active_transaction(), Some(first));
    assert_eq!(current_thread_transaction_for_test(), Some(first));
    state.commit().unwrap();
    assert_eq!(state.active_transaction(), None);
    assert_eq!(current_thread_transaction_for_test(), None);

    let second = state.begin().unwrap();
    assert_eq!(state.active_transaction(), Some(second));
    assert_eq!(current_thread_transaction_for_test(), Some(second));
    state.abort().unwrap();
    assert_eq!(state.active_transaction(), None);
    assert_eq!(current_thread_transaction_for_test(), None);
}

#[test]
fn commit_applies_only_latest_staged_records() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state.stage_memory_size(0, 1).unwrap();
    state.stage_memory_size(0, 2).unwrap();
    state.stage_global(3, GlobalSnapshot::I64(11)).unwrap();
    state.stage_global(3, GlobalSnapshot::I64(22)).unwrap();
    state
        .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
        .unwrap();
    state
        .stage_memory_granule(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
        .unwrap();

    let mut applied = Vec::new();
    state
        .commit_with(|record| {
            applied.push(record.clone());
            Ok(())
        })
        .unwrap();

    assert_eq!(
        applied,
        [
            StagedRecord::Global {
                owner_instance: None,
                global_index: 3,
                value: GlobalSnapshot::I64(22)
            },
            StagedRecord::MemoryGranule {
                owner_instance: None,
                memory_index: 0,
                granule_index: 7,
                bytes: vec![0x22; TMEMORY_GRANULE_SIZE]
            },
            StagedRecord::MemorySize {
                owner_instance: None,
                memory_index: 0,
                new_pages: 2
            },
        ]
    );
}

#[test]
fn abort_drops_staged_records_without_apply() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state.stage_memory_size(0, 9).unwrap();
    state.stage_global(3, GlobalSnapshot::I32(4)).unwrap();
    state
        .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
        .unwrap();

    state.abort().unwrap();
    assert_eq!(state.active_transaction(), None);

    state.begin().unwrap();
    let mut applied = Vec::new();
    state
        .commit_with(|record| {
            applied.push(record.clone());
            Ok(())
        })
        .unwrap();

    assert!(applied.is_empty());
}

#[test]
fn duplicate_granule_write_updates_one_staged_buffer() {
    let mut state = TransactionState::default();
    state.begin().unwrap();

    assert!(
        state
            .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
            .unwrap()
    );
    assert!(
        !state
            .stage_memory_granule(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
            .unwrap()
    );

    let staged = state.staged_memory_granule(0, 7).unwrap();
    assert_eq!(staged, &[0x22; TMEMORY_GRANULE_SIZE]);
}

#[test]
fn memory_granule_acquisition_requires_active_transaction() {
    let mut state = TransactionState::default();

    assert!(state.acquire_memory_granule_read(0, 7).is_err());
    assert!(
        state
            .acquire_memory_granule_write(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
            .is_err()
    );
}

#[test]
fn read_and_write_granule_acquisition_tracks_owned_sets() {
    let mut state = TransactionState::default();
    state.begin().unwrap();

    assert!(state.acquire_memory_granule_read(0, 7).unwrap());
    assert!(!state.acquire_memory_granule_read(0, 7).unwrap());
    assert!(state.owns_memory_granule_read(0, 7));
    assert!(!state.owns_memory_granule_write(0, 7));

    assert!(
        state
            .acquire_memory_granule_write(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
            .unwrap()
    );
    assert!(
        !state
            .acquire_memory_granule_write(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
            .unwrap()
    );
    assert!(state.owns_memory_granule_read(0, 7));
    assert!(state.owns_memory_granule_write(0, 7));

    assert_eq!(
        state.staged_memory_granule(0, 7).unwrap(),
        &[0x22; TMEMORY_GRANULE_SIZE]
    );
}

#[test]
fn stage_memory_write_splits_cross_granule_write() {
    let mut backing = vec![0x11; TMEMORY_GRANULE_SIZE * 2];
    backing[TMEMORY_GRANULE_SIZE..].fill(0x22);
    let mut state = TransactionState::default();
    state.begin().unwrap();

    let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 2).unwrap();
    state
        .stage_memory_write(0, addr, &[0xaa, 0xbb, 0xcc, 0xdd], &backing)
        .unwrap();

    let first = state.staged_memory_granule(0, 0).unwrap();
    let second = state.staged_memory_granule(0, 1).unwrap();
    assert_eq!(
        &first[..TMEMORY_GRANULE_SIZE - 2],
        &backing[..TMEMORY_GRANULE_SIZE - 2]
    );
    assert_eq!(&first[TMEMORY_GRANULE_SIZE - 2..], &[0xaa, 0xbb]);
    assert_eq!(&second[..2], &[0xcc, 0xdd]);
    assert_eq!(&second[2..], &backing[TMEMORY_GRANULE_SIZE + 2..]);
    assert!(state.owns_memory_granule_read(0, 0));
    assert!(state.owns_memory_granule_write(0, 0));
    assert!(state.owns_memory_granule_read(0, 1));
    assert!(state.owns_memory_granule_write(0, 1));
}

#[test]
fn read_memory_overlay_preserves_unwritten_staged_bytes() {
    let backing = vec![0x10; TMEMORY_GRANULE_SIZE];
    let mut state = TransactionState::default();
    state.begin().unwrap();

    state
        .stage_memory_write(0, 10, &[0xaa, 0xbb], &backing)
        .unwrap();
    state.stage_memory_write(0, 12, &[0xcc], &backing).unwrap();

    let read = state.read_memory_overlay(0, 8, 6, &backing).unwrap();
    let mut expected_read = backing[8..14].to_vec();
    expected_read[2..5].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
    assert_eq!(read, expected_read);

    let mut expected_granule = backing.clone();
    expected_granule[10..13].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
    assert_eq!(
        state.staged_memory_granule(0, 0).unwrap(),
        expected_granule.as_slice()
    );
}

#[test]
fn memory_overlay_helpers_reject_out_of_bounds_access() {
    let backing = vec![0; 8];
    let mut state = TransactionState::default();
    state.begin().unwrap();

    let write_error = state
        .stage_memory_write(0, 7, &[0xaa, 0xbb], &backing)
        .unwrap_err();
    assert!(
        write_error
            .to_string()
            .contains("out of bounds tmemory access")
    );

    let read_error = state.read_memory_overlay(0, 7, 2, &backing).unwrap_err();
    assert!(
        read_error
            .to_string()
            .contains("out of bounds tmemory access")
    );
}

#[test]
fn read_memory_overlay_tracks_read_ownership_for_touched_granules() {
    let backing = vec![0; TMEMORY_GRANULE_SIZE * 2];
    let mut state = TransactionState::default();
    state.begin().unwrap();

    let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
    assert_eq!(
        state.read_memory_overlay(0, addr, 2, &backing).unwrap(),
        vec![0, 0]
    );

    assert!(state.owns_memory_granule_read(0, 0));
    assert!(state.owns_memory_granule_read(0, 1));
    assert!(!state.owns_memory_granule_write(0, 0));
    assert!(!state.owns_memory_granule_write(0, 1));
}

#[test]
fn pending_memory_store_scratch_acquires_write_ownership_for_touched_granules() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    let owner = InstanceId::from_u32(1);

    let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
    state
        .set_memory_store_scratch(owner, 0, addr, vec![0xaa, 0xbb])
        .unwrap();

    assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 0));
    assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 1));
}

#[test]
fn commit_releases_lock_based_ownership_for_next_transaction() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state
        .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
        .unwrap();

    state.commit().unwrap();

    state.begin().unwrap();
    state.acquire_memory_granule_read(0, 0).unwrap();
}

#[test]
fn commit_rejects_changed_optimistic_read_version() {
    let mut state = TransactionState::default();
    let transaction = state.begin().unwrap();
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };
    state
        .locks
        .record_read_for_test(transaction, granule, 1)
        .unwrap();

    let error = state.commit().unwrap_err();

    assert!(error.to_string().contains("transaction read conflict"));
    assert_eq!(state.active_transaction(), Some(transaction));
}

#[test]
fn commit_validates_optimistic_read_versions_before_clearing() {
    let mut state = TransactionState::default();
    let transaction = state.begin().unwrap();
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };
    state
        .locks
        .record_read_for_test(transaction, granule, 1)
        .unwrap();

    let error = state
        .commit_with_read_validation(|_| Ok(2), |_| Ok(()))
        .unwrap_err();

    assert!(error.to_string().contains("transaction read conflict"));
    assert_eq!(state.active_transaction(), Some(transaction));
}

#[test]
fn commit_validates_reads_before_apply_callback() {
    let mut state = TransactionState::default();
    let transaction = state.begin().unwrap();
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };
    state.stage_global(3, GlobalSnapshot::I32(7)).unwrap();
    state
        .locks
        .record_read_for_test(transaction, granule, 1)
        .unwrap();
    let mut applied = false;

    let error = state
        .commit_with_read_validation(
            |_| Ok(2),
            |_| {
                applied = true;
                Ok(())
            },
        )
        .unwrap_err();

    assert!(error.to_string().contains("transaction read conflict"));
    assert!(!applied);
    assert_eq!(state.active_transaction(), Some(transaction));
}

#[test]
fn abort_releases_lock_based_ownership_for_next_transaction() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state
        .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
        .unwrap();

    state.abort().unwrap();

    state.begin().unwrap();
    state.acquire_memory_granule_read(0, 0).unwrap();
}

#[test]
fn fail_releases_lock_based_ownership_for_next_transaction() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state
        .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
        .unwrap();

    state.fail().unwrap();

    state.begin().unwrap();
    state.acquire_memory_granule_read(0, 0).unwrap();
}

#[test]
fn lock_based_transaction_ids_keep_separate_workspaces() {
    clear_current_thread_transaction_for_test();
    let mut state = TransactionState::default();
    let first = TransactionId::from_raw(11);
    let second = TransactionId::from_raw(22);

    assert_eq!(state.enter_transaction(first).unwrap(), None);
    assert_eq!(current_thread_transaction_for_test(), Some(first));
    state.stage_global(0, GlobalSnapshot::I32(11)).unwrap();

    assert_eq!(state.enter_transaction(second).unwrap(), Some(first));
    assert_eq!(current_thread_transaction_for_test(), Some(second));
    state.stage_global(1, GlobalSnapshot::I32(22)).unwrap();
    assert_eq!(state.staged_global_owned(None, 0), None);
    assert_eq!(
        state.staged_global_owned(None, 1),
        Some(GlobalSnapshot::I32(22))
    );

    state.restore_transaction(Some(first)).unwrap();
    assert_eq!(current_thread_transaction_for_test(), Some(first));
    assert_eq!(
        state.staged_global_owned(None, 0),
        Some(GlobalSnapshot::I32(11))
    );
    assert_eq!(state.staged_global_owned(None, 1), None);

    state.restore_transaction(None).unwrap();
    assert_eq!(state.active_transaction(), None);
    assert_eq!(current_thread_transaction_for_test(), None);
    assert!(state.transaction_is_open(first));
    assert!(state.transaction_is_open(second));
}

#[test]
fn lower_transaction_id_aborts_higher_suspended_writer() {
    clear_current_thread_transaction_for_test();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    state.stage_global(0, GlobalSnapshot::I32(100)).unwrap();
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(higher));

    state.enter_transaction(lower).unwrap();
    state.stage_global(0, GlobalSnapshot::I32(200)).unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.transaction_is_open(lower));
}

#[test]
fn lower_transaction_id_reader_records_version_after_aborting_writer() {
    clear_current_thread_transaction_for_test();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    state.stage_global(0, GlobalSnapshot::I32(100)).unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state.acquire_global_read_owned(None, 0).unwrap();
    assert!(!state.transaction_is_open(higher));
    state
        .validate_active_read(
            global_granule_id(None, 0),
            state.versioned_granule_version(global_granule_id(None, 0)),
        )
        .unwrap();
}

#[test]
fn lower_transaction_id_aborts_higher_suspended_object_writer() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6601, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(100))
        .unwrap();
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(higher));

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(7))
        .unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.transaction_is_open(lower));
    assert!(state.owns_object_write(object));
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(7)
    );

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1)])
    );
    clear_current_thread_transaction_for_test();
}

#[test]
fn object_conflict_aborted_suspended_transaction_frees_new_object_records() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6604, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(100))
        .unwrap();
    state.restore_transaction(None).unwrap();
    assert_eq!(objects.live_count(), 2);

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.transaction_is_open(lower));
    assert_eq!(objects.live_count(), 1);
    assert!(objects.kind(allocated).is_err());
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1)])
    );

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn tmemory_conflict_aborted_allocations_are_reclaimed_before_object_commit() {
    clear_current_thread_transaction_for_test();
    let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[1, 2, 3, 4], &tmemory)
        .unwrap();
    state.restore_transaction(None).unwrap();
    assert_eq!(objects.live_count(), 1);

    state.enter_transaction(lower).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[5, 6, 7, 8], &tmemory)
        .unwrap();
    assert!(!state.transaction_is_open(higher));
    assert_eq!(objects.live_count(), 1);

    assert!(!state.commit_object_payloads(&mut objects).unwrap());
    assert_eq!(objects.live_count(), 0);
    assert!(objects.kind(allocated).is_err());

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn generic_abort_rejects_active_object_allocations_without_object_table() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.record_allocated_object(allocated).unwrap();
    let error = state.abort().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("object-aware cleanup for allocated objects"),
        "{error:?}"
    );
    assert!(state.active_transaction().is_some());

    state.abort_allocated_objects(&mut objects).unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert!(objects.kind(allocated).is_err());
    clear_current_thread_transaction_for_test();
}

#[test]
fn generic_commit_rejects_pending_conflict_aborted_object_allocations() {
    clear_current_thread_transaction_for_test();
    let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[1, 2, 3, 4], &tmemory)
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[5, 6, 7, 8], &tmemory)
        .unwrap();
    let error = state.complete_commit().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("object-aware cleanup for conflict-aborted allocated objects"),
        "{error:?}"
    );
    assert!(state.active_transaction().is_some());

    assert!(!state.commit_object_payloads(&mut objects).unwrap());
    assert!(objects.kind(allocated).is_err());
    state.complete_commit().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn generic_abort_transaction_rejects_suspended_object_allocations_without_object_table() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    let mut state = TransactionState::default();
    let transaction = TransactionId::from_raw(7);

    state.enter_transaction(transaction).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state.restore_transaction(None).unwrap();

    let error = state.abort_transaction(transaction).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("object-aware cleanup for allocated objects"),
        "{error:?}"
    );
    assert!(state.transaction_is_open(transaction));

    state
        .abort_transaction_allocated_objects(&mut objects, transaction)
        .unwrap();
    assert!(objects.kind(allocated).is_err());
    assert!(!state.transaction_is_open(transaction));
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn higher_transaction_id_cannot_write_lower_owned_object() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6602, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let lower = TransactionId::from_raw(1);
    let higher = TransactionId::from_raw(100_001);

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(11))
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(higher).unwrap();
    let error = state
        .acquire_object_write(&mut objects, object)
        .unwrap_err();
    assert!(
        error.to_string().contains("transaction write conflict"),
        "{error:?}"
    );
    assert!(state.transaction_is_open(lower));
    assert!(state.transaction_is_open(higher));
    assert!(!state.owns_object_write(object));

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    state.restore_transaction(Some(lower)).unwrap();
    assert!(state.owns_object_write(object));
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(11)
    );

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}

#[test]
fn mixed_object_and_tmemory_transaction_survives_object_conflict_and_commits_both() {
    clear_current_thread_transaction_for_test();
    let mut tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6603, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);
    let lower_addr = TMEMORY_GRANULE_SIZE as u64 + 8;

    state.enter_transaction(higher).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(100))
        .unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 4, &[1, 2, 3, 4], &tmemory)
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, lower_addr, &[5, 6, 7, 8], &tmemory)
        .unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(7))
        .unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.owns_object_write(object));
    assert!(state.owns_memory_granule_write_owned(Some(InstanceId::from_u32(0)), 0, 1));

    assert!(state.commit_tmemory_for_test(&mut tmemory).unwrap());
    assert!(state.commit_object_payloads(&mut objects).unwrap());
    state.complete_commit().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);

    assert_eq!(tmemory.read_committed(0..4).unwrap(), vec![0, 0, 0, 0]);
    let lower_range = lower_addr as usize..lower_addr as usize + 4;
    assert_eq!(
        tmemory.read_committed(lower_range).unwrap(),
        vec![5, 6, 7, 8]
    );
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(7)])
    );
    clear_current_thread_transaction_for_test();
}

#[test]
fn aborting_suspended_transaction_releases_only_its_locks() {
    let mut state = TransactionState::default();
    let first = TransactionId::from_raw(11);
    let second = TransactionId::from_raw(22);

    state.enter_transaction(first).unwrap();
    state
        .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
        .unwrap();
    state.enter_transaction(second).unwrap();
    state
        .acquire_memory_granule_write(0, 1, vec![0xbb; TMEMORY_GRANULE_SIZE])
        .unwrap();

    let conflict = state.acquire_memory_granule_read(0, 0).unwrap_err();
    assert!(conflict.to_string().contains("transaction read conflict"));

    assert!(state.abort_transaction(first).unwrap());
    state.acquire_memory_granule_read(0, 0).unwrap();
    assert!(state.owns_memory_granule_write(0, 1));
    assert!(!state.abort_transaction(first).unwrap());
}

#[test]
fn fail_drops_staged_records() {
    let mut state = TransactionState::default();
    state.begin().unwrap();
    state.stage_memory_size(0, 9).unwrap();

    state.fail().unwrap();

    assert_eq!(state.active_transaction(), None);
}

#[test]
fn duplicate_global_write_updates_latest_staged_value() {
    let mut state = TransactionState::default();
    state.begin().unwrap();

    assert!(state.stage_global(3, GlobalSnapshot::I64(11)).unwrap());
    assert!(!state.stage_global(3, GlobalSnapshot::I64(22)).unwrap());

    let mut applied = Vec::new();
    state
        .commit_with(|record| {
            applied.push(record.clone());
            Ok(())
        })
        .unwrap();

    assert_eq!(
        applied,
        [StagedRecord::Global {
            owner_instance: None,
            global_index: 3,
            value: GlobalSnapshot::I64(22)
        }]
    );
}

#[test]
fn owner_instance_zero_does_not_alias_ownerless_globals() {
    let mut state = TransactionState::default();
    let owner0 = InstanceId::from_u32(0);
    state.begin().unwrap();

    assert!(
        state
            .stage_global_owned(None, 0, GlobalSnapshot::I32(1))
            .unwrap()
    );
    assert!(
        state
            .stage_global_owned(Some(owner0), 0, GlobalSnapshot::I32(2))
            .unwrap()
    );

    assert_eq!(
        state.staged_global_owned(None, 0),
        Some(GlobalSnapshot::I32(1))
    );
    assert_eq!(
        state.staged_global_owned(Some(owner0), 0),
        Some(GlobalSnapshot::I32(2))
    );

    let records = state.staged_records().unwrap();
    assert!(records.contains(&StagedRecord::Global {
        owner_instance: None,
        global_index: 0,
        value: GlobalSnapshot::I32(1),
    }));
    assert!(records.contains(&StagedRecord::Global {
        owner_instance: Some(owner0),
        global_index: 0,
        value: GlobalSnapshot::I32(2),
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                StagedRecord::Global {
                    global_index: 0,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn owner_instance_zero_does_not_alias_ownerless_memory_granules() {
    let mut state = TransactionState::default();
    let owner0 = InstanceId::from_u32(0);
    let ownerless_bytes = vec![0x11; TMEMORY_GRANULE_SIZE];
    let owner0_bytes = vec![0x22; TMEMORY_GRANULE_SIZE];
    state.begin().unwrap();

    assert!(
        state
            .stage_memory_granule(0, 0, ownerless_bytes.clone())
            .unwrap()
    );
    assert!(
        state
            .acquire_memory_granule_write_owned(Some(owner0), 0, 0, owner0_bytes.clone())
            .unwrap()
    );

    let records = state.staged_records().unwrap();
    assert!(records.contains(&StagedRecord::MemoryGranule {
        owner_instance: None,
        memory_index: 0,
        granule_index: 0,
        bytes: ownerless_bytes,
    }));
    assert!(records.contains(&StagedRecord::MemoryGranule {
        owner_instance: Some(owner0),
        memory_index: 0,
        granule_index: 0,
        bytes: owner0_bytes,
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                StagedRecord::MemoryGranule {
                    memory_index: 0,
                    granule_index: 0,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn owner_instance_zero_does_not_alias_ownerless_memory_sizes() {
    let mut state = TransactionState::default();
    let owner0 = InstanceId::from_u32(0);
    state.begin().unwrap();

    assert!(state.stage_memory_size_owned(None, 0, 1).unwrap());
    assert!(state.stage_memory_size_owned(Some(owner0), 0, 2).unwrap());

    let records = state.staged_records().unwrap();
    assert!(records.contains(&StagedRecord::MemorySize {
        owner_instance: None,
        memory_index: 0,
        new_pages: 1,
    }));
    assert!(records.contains(&StagedRecord::MemorySize {
        owner_instance: Some(owner0),
        memory_index: 0,
        new_pages: 2,
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                StagedRecord::MemorySize {
                    memory_index: 0,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn workspace_merges_staged_and_committed_granules_with_nonzero_backing_base() {
    let mut state = TransactionState::default();
    let owner0 = InstanceId::from_u32(0);
    let memory_index = 0;
    let backing_base = TMEMORY_GRANULE_SIZE as u64;
    let memory_len = TMEMORY_GRANULE_SIZE * 4;
    let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 2)
        .map(|i| (i % 251) as u8)
        .collect();
    let addr = (TMEMORY_GRANULE_SIZE * 2 - 2) as u64;
    let staged = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    let read_addr = addr - 2;
    let read_len = 10;
    state.begin().unwrap();

    state
        .stage_memory_write_owned_from_backing(
            Some(owner0),
            memory_index,
            addr,
            &staged,
            backing_base,
            &committed,
            memory_len,
        )
        .unwrap();

    let merged = state
        .read_memory_overlay_owned_from_backing(
            Some(owner0),
            memory_index,
            read_addr,
            read_len,
            backing_base,
            &committed,
            memory_len,
        )
        .unwrap();

    let read_start = read_addr as usize - backing_base as usize;
    let mut expected = committed[read_start..read_start + read_len].to_vec();
    let staged_offset = (addr - read_addr) as usize;
    expected[staged_offset..staged_offset + staged.len()].copy_from_slice(&staged);

    assert_eq!(merged, expected);
}

#[test]
fn granule_id_orders_by_object_space_and_index() {
    let mut ids = vec![
        GranuleId::TGlobal {
            instance: None,
            global_index: 2,
        },
        GranuleId::TMemorySize {
            instance: None,
            memory_index: 4,
        },
        GranuleId::TMemory {
            instance: Some(2),
            memory_index: 0,
            granule_index: 0,
        },
        GranuleId::TMemory {
            instance: Some(1),
            memory_index: 7,
            granule_index: 3,
        },
        GranuleId::TGlobal {
            instance: None,
            global_index: 1,
        },
        GranuleId::TMemorySize {
            instance: None,
            memory_index: 1,
        },
        GranuleId::TMemory {
            instance: Some(1),
            memory_index: 7,
            granule_index: 1,
        },
        GranuleId::Object {
            object_id: ObjectId { object_index: 2 },
        },
        GranuleId::Object {
            object_id: ObjectId { object_index: 1 },
        },
        GranuleId::TTableSize {
            instance: None,
            table_index: 3,
        },
        GranuleId::TTable {
            instance: Some(1),
            table_index: 2,
            granule_index: 0,
        },
    ];

    ids.sort();

    assert_eq!(
        ids,
        vec![
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 1,
            },
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 3,
            },
            GranuleId::TMemory {
                instance: Some(2),
                memory_index: 0,
                granule_index: 0,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 1,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 4,
            },
            GranuleId::TGlobal {
                instance: None,
                global_index: 1,
            },
            GranuleId::TGlobal {
                instance: None,
                global_index: 2,
            },
            GranuleId::TTable {
                instance: Some(1),
                table_index: 2,
                granule_index: 0,
            },
            GranuleId::TTableSize {
                instance: None,
                table_index: 3,
            },
            GranuleId::Object {
                object_id: ObjectId { object_index: 1 },
            },
            GranuleId::Object {
                object_id: ObjectId { object_index: 2 },
            },
        ]
    );
}

#[test]
fn workspace_merges_staged_and_committed_granules() {
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));
    let instance = 11;
    let memory_index = 3;
    let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 3)
        .map(|i| (i % 251) as u8)
        .collect();

    state.stage_granule_for_test(
        GranuleId::TMemory {
            instance: Some(instance),
            memory_index,
            granule_index: 0,
        },
        vec![0xAA; TMEMORY_GRANULE_SIZE],
    );
    state.stage_granule_for_test(
        GranuleId::TMemory {
            instance: Some(instance),
            memory_index,
            granule_index: 2,
        },
        vec![0xCC; TMEMORY_GRANULE_SIZE],
    );

    let merged = state
        .read_tmemory_range_for_test(
            Some(instance),
            memory_index,
            (TMEMORY_GRANULE_SIZE - 2) as u64,
            TMEMORY_GRANULE_SIZE + 6,
            &committed,
        )
        .unwrap();

    let mut expected = vec![0xAA; 2];
    expected.extend_from_slice(&committed[TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE * 2]);
    expected.extend_from_slice(&[0xCC; 4]);

    assert_eq!(merged, expected);
}

#[test]
fn transaction_commit_copies_staged_granules_to_tmemory() {
    let mut tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage tmemory");
    state
        .commit_tmemory_for_test(&mut tmemory)
        .expect("commit tmemory");

    assert_eq!(
        tmemory.read_committed(4..8).expect("read committed"),
        vec![9, 8, 7, 6]
    );
}

#[test]
fn transaction_commit_uses_persistent_tmemory_undo_for_nvmemory() {
    let mut tmemory =
        crate::runtime::vm::TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1))
            .expect("tmemory");
    tmemory.commit_range(0, &[1, 2, 3, 4]).unwrap();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(12));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage tmemory");
    state
        .commit_tmemory_for_test(&mut tmemory)
        .expect("commit tmemory");

    assert_eq!(
        tmemory.read_committed(4..8).expect("read committed"),
        vec![9, 8, 7, 6]
    );
    let entries = state.durable_log_entries_for_test(12);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].tx_meta & 1 != 0);
    assert!(entries[0].validate_crc32());
}

#[test]
fn transaction_abort_discards_staged_tmemory_bytes() {
    let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage tmemory");
    state.abort().expect("abort transaction");

    assert_eq!(
        tmemory.read_committed(4..8).expect("read committed"),
        vec![0, 0, 0, 0]
    );
}

#[test]
fn transaction_table_granules_follow_wizard_sixteen_element_chunks() {
    let owner = InstanceId::from_u32(2);

    assert_eq!(
        table_granule_id(Some(owner), 3, 0),
        GranuleId::TTable {
            instance: Some(2),
            table_index: 3,
            granule_index: 0,
        }
    );
    assert_eq!(
        table_granule_id(Some(owner), 3, 15),
        GranuleId::TTable {
            instance: Some(2),
            table_index: 3,
            granule_index: 0,
        }
    );
    assert_eq!(
        table_granule_id(Some(owner), 3, 16),
        GranuleId::TTable {
            instance: Some(2),
            table_index: 3,
            granule_index: 1,
        }
    );
}

#[test]
fn transaction_table_granule_acquisition_tracks_read_and_write_sets() {
    let mut state = TransactionState::default();
    let owner = InstanceId::from_u32(1);
    state.begin().unwrap();

    assert!(
        state
            .acquire_table_granule_read_owned(Some(owner), 2, 15, 4)
            .unwrap()
    );
    assert!(
        !state
            .acquire_table_granule_read_owned(Some(owner), 2, 8, 4)
            .unwrap()
    );
    assert!(state.owns_table_granule_read_owned(Some(owner), 2, 0));
    assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));

    assert!(
        state
            .acquire_table_granule_write_owned(Some(owner), 2, 16, 7)
            .unwrap()
    );
    assert!(state.owns_table_granule_read_owned(Some(owner), 2, 1));
    assert!(state.owns_table_granule_write_owned(Some(owner), 2, 1));
}

#[test]
fn transaction_table_size_granule_is_separate_from_element_granules() {
    let mut state = TransactionState::default();
    let owner = InstanceId::from_u32(1);
    state.begin().unwrap();

    assert!(
        state
            .acquire_table_size_read_owned(Some(owner), 2, 11)
            .unwrap()
    );
    assert!(state.owns_table_size_read_owned(Some(owner), 2));
    assert!(!state.owns_table_granule_read_owned(Some(owner), 2, 0));

    assert!(
        state
            .acquire_table_size_write_owned(Some(owner), 2, 11)
            .unwrap()
    );
    assert!(state.owns_table_size_write_owned(Some(owner), 2));
    assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));
}

#[test]
fn granule_permission_acquisition_tracks_all_granule_kinds() {
    let mut state = TransactionState::default();
    let owner = InstanceId::from_u32(1);
    let object = ObjectId { object_index: 9 };
    let other_object = ObjectId { object_index: 10 };
    let granules = [
        GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 2,
        },
        GranuleId::TMemorySize {
            instance: Some(1),
            memory_index: 0,
        },
        GranuleId::TGlobal {
            instance: Some(1),
            global_index: 3,
        },
        GranuleId::TTable {
            instance: Some(1),
            table_index: 4,
            granule_index: 5,
        },
        GranuleId::TTableSize {
            instance: Some(1),
            table_index: 4,
        },
        GranuleId::Object { object_id: object },
        GranuleId::Object {
            object_id: other_object,
        },
    ];

    state.begin().unwrap();

    for granule in granules {
        assert!(state.acquire_granule_read(granule, 7).unwrap());
        assert!(!state.acquire_granule_read(granule, 7).unwrap());
        assert!(state.owns_granule_read(granule));
        assert!(!state.owns_granule_write(granule));

        assert!(state.acquire_granule_write(granule, 7).unwrap());
        assert!(!state.acquire_granule_write(granule, 7).unwrap());
        assert!(state.owns_granule_read(granule));
        assert!(state.owns_granule_write(granule));
    }

    assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 2));
    assert!(state.owns_table_granule_write_owned(Some(owner), 4, 5));
    assert!(state.owns_table_size_write_owned(Some(owner), 4));
}

#[test]
fn stage_global_acquires_tglobal_write_permission() {
    let mut state = TransactionState::default();
    let owner = InstanceId::from_u32(3);
    let granule = GranuleId::TGlobal {
        instance: Some(3),
        global_index: 1,
    };

    state.begin().unwrap();

    state
        .stage_global_owned(Some(owner), 1, GlobalSnapshot::I32(42))
        .unwrap();

    assert!(state.owns_granule_read(granule));
    assert!(state.owns_granule_write(granule));
}

#[test]
fn tref_cast_acquires_object_permission_for_transaction_handles() {
    let mut objects = ObjectTable::default();
    let struct_object = objects
        .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
        .unwrap();
    let array_object = objects
        .allocate_persistent_array_for_gc_ref(0x22, vec![ObjectValue::I32(9)])
        .unwrap();
    let struct_handle = objects
        .transaction_ref_handle_for_object_id(struct_object)
        .unwrap();
    let array_handle = objects
        .transaction_ref_handle_for_object_id(array_object)
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    assert!(
        state
            .acquire_tref_read_for_transaction_ref_handle(&mut objects, struct_handle)
            .unwrap()
    );
    assert!(state.owns_object_read(struct_object));
    assert!(!state.owns_object_write(struct_object));

    assert!(
        state
            .acquire_tref_write_for_transaction_ref_handle(&mut objects, array_handle)
            .unwrap()
    );
    assert!(state.owns_object_read(array_object));
    assert!(state.owns_object_write(array_object));
}

#[test]
fn tref_cast_ignores_null_and_unknown_refs_without_granting_access() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    assert!(
        !state
            .acquire_tref_read_for_transaction_ref_handle(&mut objects, 0)
            .unwrap()
    );
    assert!(
        !state
            .acquire_tref_write_for_transaction_ref_handle(&mut objects, 0x99)
            .unwrap()
    );
    assert!(!state.owns_object_read(object));
    assert!(!state.owns_object_write(object));
}

#[test]
fn tref_cast_does_not_lock_ordinary_gc_bridge_refs_without_handle() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    assert!(!objects.is_persistent(object).unwrap());
    assert!(
        !state
            .acquire_tref_read_for_transaction_ref_handle(&mut objects, 0x11)
            .unwrap()
    );
    assert!(
        !state
            .acquire_tref_write_for_transaction_ref_handle(&mut objects, 0x11)
            .unwrap()
    );
    assert!(!state.owns_object_read(object));
    assert!(!state.owns_object_write(object));
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(7)
    );
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
        .unwrap();
}

#[test]
fn object_payload_access_requires_prior_tref_permission() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
        .unwrap();
    let handle = objects
        .transaction_ref_handle_for_object_id(object)
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    let error = state.read_struct_field(&objects, object, 0).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transactional object read permission was not acquired")
    );

    state
        .acquire_tref_read_for_transaction_ref_handle(&mut objects, handle)
        .unwrap();
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(7)
    );

    let error = state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transactional object write permission was not acquired")
    );

    state
        .acquire_tref_write_for_transaction_ref_handle(&mut objects, handle)
        .unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
        .unwrap();
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(8)
    );
}

#[test]
fn object_table_allocates_dense_stable_ids_and_reuses_freed_slots() {
    let mut objects = ObjectTable::default();

    let first = objects.allocate(ObjectKind::Struct).unwrap();
    let second = objects.allocate(ObjectKind::Array).unwrap();

    assert_eq!(first, ObjectId { object_index: 0 });
    assert_eq!(second, ObjectId { object_index: 1 });
    assert_eq!(objects.live_count(), 2);
    assert_eq!(objects.slot_count(), 2);
    assert_eq!(objects.kind(first).unwrap(), ObjectKind::Struct);
    assert_eq!(objects.kind(second).unwrap(), ObjectKind::Array);

    let second_version = objects.version(second).unwrap();
    assert!(objects.free(second).unwrap());
    assert_eq!(objects.live_count(), 1);
    assert_eq!(objects.slot_count(), 2);
    assert!(objects.kind(second).is_err());

    let reused = objects.allocate(ObjectKind::Struct).unwrap();

    assert_eq!(reused, second);
    assert_eq!(objects.live_count(), 2);
    assert_eq!(objects.slot_count(), 2);
    assert!(objects.version(reused).unwrap() > second_version);
    assert_eq!(objects.kind(reused).unwrap(), ObjectKind::Struct);
}

#[test]
fn object_table_default_registers_builtin_scalar_type_layouts() {
    let objects = ObjectTable::default();

    assert_eq!(
        objects
            .require_type_layout(type_layout::TypeLayoutId::BUILTIN_I31)
            .unwrap()
            .kind(),
        type_layout::PersistentTypeKind::I31
    );
    assert_eq!(
        objects
            .require_type_layout(type_layout::TypeLayoutId::BUILTIN_EXTERN)
            .unwrap()
            .kind(),
        type_layout::PersistentTypeKind::Extern
    );
    assert_eq!(
        objects
            .require_type_layout(type_layout::TypeLayoutId::BUILTIN_FUNC)
            .unwrap()
            .kind(),
        type_layout::PersistentTypeKind::Func
    );
}

#[test]
fn wasmtime_type_layout_mapping_struct_same_shape_keeps_fingerprint_across_type_indices() {
    let fields = vec![
        WasmtimePersistentFieldLayout {
            field_index: 0,
            field_offset: 0,
            value_size: 20,
            is_object_ref: false,
        },
        WasmtimePersistentFieldLayout {
            field_index: 1,
            field_offset: 20,
            value_size: 20,
            is_object_ref: true,
        },
    ];
    let mut objects = ObjectTable::default();
    let first_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            11,
            27,
            PersistentTypeKind::Struct,
            0xfeed_0000_0000_0001,
        )
        .unwrap();
    let second_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            11,
            28,
            PersistentTypeKind::Struct,
            0xfeed_0000_0000_0001,
        )
        .unwrap();

    let first = persistent_layout_for_wasmtime_struct_type(first_id, 40, &fields).unwrap();
    let second = persistent_layout_for_wasmtime_struct_type(second_id, 40, &fields).unwrap();

    assert_ne!(first.id(), second.id());
    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[test]
fn wasmtime_type_layout_mapping_struct_ref_map_changes_fingerprint() {
    let mut objects = ObjectTable::default();
    let layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            12,
            28,
            PersistentTypeKind::Struct,
            0xfeed_0000_0000_0002,
        )
        .unwrap();
    let scalar_then_ref = persistent_layout_for_wasmtime_struct_type(
        layout_id,
        40,
        &[
            WasmtimePersistentFieldLayout {
                field_index: 0,
                field_offset: 0,
                value_size: 20,
                is_object_ref: false,
            },
            WasmtimePersistentFieldLayout {
                field_index: 1,
                field_offset: 20,
                value_size: 20,
                is_object_ref: true,
            },
        ],
    )
    .unwrap();
    let ref_then_scalar = persistent_layout_for_wasmtime_struct_type(
        layout_id,
        40,
        &[
            WasmtimePersistentFieldLayout {
                field_index: 0,
                field_offset: 0,
                value_size: 20,
                is_object_ref: true,
            },
            WasmtimePersistentFieldLayout {
                field_index: 1,
                field_offset: 20,
                value_size: 20,
                is_object_ref: false,
            },
        ],
    )
    .unwrap();

    assert_ne!(scalar_then_ref.fingerprint(), ref_then_scalar.fingerprint());
}

#[test]
fn wasmtime_type_layout_mapping_struct_uses_descriptor_ref_map_not_payload_values() {
    let mut objects = ObjectTable::default();
    let namespace = 13;
    let field_layouts = vec![
        WasmtimePersistentFieldLayout {
            field_index: 0,
            field_offset: 0,
            value_size: 4,
            is_object_ref: false,
        },
        WasmtimePersistentFieldLayout {
            field_index: 1,
            field_offset: 20,
            value_size: 20,
            is_object_ref: true,
        },
    ];

    let object = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_layout_namespace(
            0x131,
            namespace,
            29,
            field_layouts.clone(),
            vec![ObjectValue::I32(1), ObjectValue::I64(2)],
        )
        .unwrap();
    let layout_id = objects.live_slot(object).unwrap().type_layout_id;
    let layout = objects
        .require_type_layout(type_layout::TypeLayoutId::new(layout_id).unwrap())
        .unwrap();

    assert_eq!(
        layout,
        &persistent_layout_for_wasmtime_struct_type(
            type_layout::TypeLayoutId::new(layout_id).unwrap(),
            40,
            &field_layouts,
        )
        .unwrap()
    );
}

#[test]
fn wasmtime_type_layout_mapping_namespace_avoids_local_type_index_collisions() {
    let mut objects = ObjectTable::default();
    let first = persistent_layout_for_wasmtime_struct_type(
        objects
            .persistent_type_layout_id_for_wasmtime_key(
                21,
                7,
                PersistentTypeKind::Struct,
                0xfeed_0000_0000_0011,
            )
            .unwrap(),
        20,
        &[WasmtimePersistentFieldLayout {
            field_index: 0,
            field_offset: 0,
            value_size: 20,
            is_object_ref: false,
        }],
    )
    .unwrap();
    let second = persistent_layout_for_wasmtime_struct_type(
        objects
            .persistent_type_layout_id_for_wasmtime_key(
                22,
                7,
                PersistentTypeKind::Struct,
                0xfeed_0000_0000_0012,
            )
            .unwrap(),
        20,
        &[WasmtimePersistentFieldLayout {
            field_index: 0,
            field_offset: 0,
            value_size: 20,
            is_object_ref: true,
        }],
    )
    .unwrap();

    assert_ne!(first.id(), second.id());
    objects.register_type_layout(first).unwrap();
    objects.register_type_layout(second).unwrap();
}

#[test]
fn wasmtime_type_layout_mapping_large_namespace_and_type_index_reuses_same_id() {
    let mut objects = ObjectTable::default();
    let first = objects
        .persistent_type_layout_id_for_wasmtime_key(
            u32::MAX - 1,
            u32::MAX,
            PersistentTypeKind::Array,
            0xfeed_0000_0000_00ff,
        )
        .unwrap();
    let second = objects
        .persistent_type_layout_id_for_wasmtime_key(
            u32::MAX - 1,
            u32::MAX,
            PersistentTypeKind::Array,
            0xfeed_0000_0000_00ff,
        )
        .unwrap();

    assert_eq!(first, second);
    assert!(first.get() > TypeLayoutId::BUILTIN_FUNC.get());
}

#[test]
fn wasmtime_type_layout_mapping_reuses_recovered_layout_id_for_same_shape() {
    let mut recovered = TypeLayoutRegistry::default();
    let recovered_layout = persistent_layout_for_wasmtime_array_type(
        type_layout::TypeLayoutId::new(91).unwrap(),
        20,
        true,
    );
    recovered.insert(recovered_layout.clone()).unwrap();

    let mut objects = ObjectTable::default();
    objects.install_recovered_type_layouts(&recovered).unwrap();

    let reused_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            61,
            12,
            PersistentTypeKind::Array,
            recovered_layout.fingerprint(),
        )
        .unwrap();

    assert_eq!(reused_id, recovered_layout.id());
}

#[test]
fn wasmtime_type_layout_mapping_allocates_new_id_for_different_shape_after_recovery() {
    let mut recovered = TypeLayoutRegistry::default();
    let recovered_layout = persistent_layout_for_wasmtime_array_type(
        type_layout::TypeLayoutId::new(92).unwrap(),
        20,
        true,
    );
    recovered.insert(recovered_layout).unwrap();

    let mut objects = ObjectTable::default();
    objects.install_recovered_type_layouts(&recovered).unwrap();

    let new_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            62,
            13,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(20, false),
        )
        .unwrap();

    assert_ne!(new_id, type_layout::TypeLayoutId::new(92).unwrap());
    assert!(new_id.get() > 92);
}

#[test]
fn persistent_object_publication_struct_allocation_uses_wasmtime_layout_id() {
    let (durable_log, events) = TxDurableLog::recording_backend_for_test();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(41), durable_log);
    let mut objects = ObjectTable::default();
    let namespace = 41;
    let expected_layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            44,
            PersistentTypeKind::Struct,
            wasmtime_struct_layout_fingerprint(
                60,
                &[
                    WasmtimePersistentFieldLayout {
                        field_index: 0,
                        field_offset: 0,
                        value_size: 20,
                        is_object_ref: false,
                    },
                    WasmtimePersistentFieldLayout {
                        field_index: 1,
                        field_offset: 20,
                        value_size: 20,
                        is_object_ref: true,
                    },
                    WasmtimePersistentFieldLayout {
                        field_index: 2,
                        field_offset: 40,
                        value_size: 20,
                        is_object_ref: true,
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
    let expected_layout = persistent_layout_for_wasmtime_struct_type(
        expected_layout_id,
        60,
        &[
            WasmtimePersistentFieldLayout {
                field_index: 0,
                field_offset: 0,
                value_size: 20,
                is_object_ref: false,
            },
            WasmtimePersistentFieldLayout {
                field_index: 1,
                field_offset: 20,
                value_size: 20,
                is_object_ref: true,
            },
            WasmtimePersistentFieldLayout {
                field_index: 2,
                field_offset: 40,
                value_size: 20,
                is_object_ref: true,
            },
        ],
    )
    .unwrap();
    let first = objects
        .allocate_persistent_struct_for_gc_ref(0x443, vec![ObjectValue::I32(1)])
        .unwrap();
    let second = objects
        .allocate_persistent_struct_for_gc_ref(0x444, vec![ObjectValue::I32(2)])
        .unwrap();

    let object = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x441,
            namespace,
            44,
            vec![
                ObjectValue::I32(1),
                ObjectValue::Ref(Some(first)),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();
    let second_object = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x442,
            namespace,
            44,
            vec![
                ObjectValue::I32(9),
                ObjectValue::Ref(Some(second)),
                ObjectValue::Ref(Some(first)),
            ],
        )
        .unwrap();
    let handle = objects.current_record_handle_for_test(object).unwrap();
    let header = objects.heap.header(handle).unwrap();

    assert_eq!(
        objects.live_slot(object).unwrap().type_layout_id,
        expected_layout.id().get()
    );
    assert_eq!(
        objects.live_slot(second_object).unwrap().type_layout_id,
        expected_layout.id().get()
    );
    assert_ne!(
        expected_layout.id(),
        type_layout::TypeLayoutId::DEFAULT_STRUCT
    );
    assert_eq!(
        objects.require_type_layout(expected_layout.id()).unwrap(),
        &expected_layout
    );
    assert_eq!(header.type_layout_id, expected_layout.id().get());
    assert_eq!(
        objects.trace_object_ids(object).unwrap(),
        vec![first, second]
    );

    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(7))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    state
        .publish_object_publications_before_commit(41, 41, &objects, &publications)
        .unwrap()
        .unwrap();

    let events = events.lock().unwrap().clone();
    let ensure_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::EnsureTypeLayout(id)
                    if *id == expected_layout.id().get()
            )
        })
        .unwrap();
    let data_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::AppendDataRecord(
                    persist::DurableDataStream::ObjectPublication
                )
            )
        })
        .unwrap();

    assert!(ensure_index < data_index);
}

#[test]
fn persistent_object_publication_array_allocation_uses_wasmtime_layout_id() {
    let (durable_log, events) = TxDurableLog::recording_backend_for_test();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(42), durable_log);
    let mut objects = ObjectTable::default();
    let namespace = 42;
    let expected_layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            55,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(20, true),
        )
        .unwrap();
    let expected_layout = persistent_layout_for_wasmtime_array_type(expected_layout_id, 20, true);
    let first = objects
        .allocate_persistent_struct_for_gc_ref(0x553, vec![ObjectValue::I32(1)])
        .unwrap();
    let second = objects
        .allocate_persistent_struct_for_gc_ref(0x554, vec![ObjectValue::I32(2)])
        .unwrap();

    let object = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_type_namespace(
            0x551,
            namespace,
            55,
            vec![
                ObjectValue::Ref(None),
                ObjectValue::Ref(Some(first)),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();
    let second_object = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_type_namespace(
            0x552,
            namespace,
            55,
            vec![
                ObjectValue::Ref(Some(second)),
                ObjectValue::Ref(Some(first)),
            ],
        )
        .unwrap();
    let handle = objects.current_record_handle_for_test(object).unwrap();
    let header = objects.heap.header(handle).unwrap();

    assert_eq!(
        objects.live_slot(object).unwrap().type_layout_id,
        expected_layout.id().get()
    );
    assert_eq!(
        objects.live_slot(second_object).unwrap().type_layout_id,
        expected_layout.id().get()
    );
    assert_ne!(
        expected_layout.id(),
        type_layout::TypeLayoutId::DEFAULT_ARRAY
    );
    assert_eq!(
        objects.require_type_layout(expected_layout.id()).unwrap(),
        &expected_layout
    );
    assert_eq!(header.type_layout_id, expected_layout.id().get());
    assert_eq!(
        objects.trace_object_ids(object).unwrap(),
        vec![first, second]
    );

    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_array_element(&objects, object, 0, ObjectValue::Ref(Some(first)))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    state
        .publish_object_publications_before_commit(42, 42, &objects, &publications)
        .unwrap()
        .unwrap();

    let events = events.lock().unwrap().clone();
    let ensure_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::EnsureTypeLayout(id)
                    if *id == expected_layout.id().get()
            )
        })
        .unwrap();
    let data_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::AppendDataRecord(
                    persist::DurableDataStream::ObjectPublication
                )
            )
        })
        .unwrap();

    assert!(ensure_index < data_index);
}

#[test]
fn wasmtime_type_layout_mapping_zero_length_array_preserves_ref_initializer_kind() {
    let mut objects = ObjectTable::default();
    let namespace = 43;
    let layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            56,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(20, true),
        )
        .unwrap();
    let first = ObjectId { object_index: 91 };
    let second = ObjectId { object_index: 92 };

    let empty = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_type_and_initializer_namespace(
            0x561,
            namespace,
            56,
            ObjectValue::Ref(Some(first)),
            0,
        )
        .unwrap();
    let non_empty = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_type_namespace(
            0x562,
            namespace,
            56,
            vec![ObjectValue::Ref(Some(second))],
        )
        .unwrap();

    let PersistentTypeLayout::Array { element_kind, .. } =
        objects.require_type_layout(layout_id).unwrap()
    else {
        panic!("expected array layout");
    };
    assert_eq!(*element_kind, TraceSlotKind::ObjectRef);
    assert_eq!(
        objects.live_slot(empty).unwrap().type_layout_id,
        layout_id.get()
    );
    assert_eq!(
        objects.live_slot(non_empty).unwrap().type_layout_id,
        layout_id.get()
    );
    assert_eq!(objects.trace_object_ids(non_empty).unwrap(), vec![second]);
}

#[test]
fn wasmtime_type_layout_mapping_empty_fixed_array_preserves_ref_element_kind() {
    let mut objects = ObjectTable::default();
    let namespace = 44;
    let layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            57,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(20, true),
        )
        .unwrap();

    let empty = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
            0x571,
            namespace,
            57,
            20,
            true,
            Vec::new(),
        )
        .unwrap();

    let PersistentTypeLayout::Array { element_kind, .. } =
        objects.require_type_layout(layout_id).unwrap()
    else {
        panic!("expected array layout");
    };
    assert_eq!(*element_kind, TraceSlotKind::ObjectRef);
    assert_eq!(
        objects.live_slot(empty).unwrap().type_layout_id,
        layout_id.get()
    );
}

#[test]
fn wasmtime_type_layout_mapping_empty_fixed_ref_array_is_compatible_with_later_non_empty_ref_allocation()
 {
    let mut objects = ObjectTable::default();
    let namespace = 45;
    let first = ObjectId { object_index: 101 };
    let second = ObjectId { object_index: 102 };
    let layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            58,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(20, true),
        )
        .unwrap();

    let empty = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
            0x581,
            namespace,
            58,
            20,
            true,
            Vec::new(),
        )
        .unwrap();
    let non_empty = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
            0x582,
            namespace,
            58,
            20,
            true,
            vec![
                ObjectValue::Ref(Some(first)),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();

    assert_eq!(
        objects.live_slot(empty).unwrap().type_layout_id,
        layout_id.get()
    );
    assert_eq!(
        objects.live_slot(non_empty).unwrap().type_layout_id,
        layout_id.get()
    );
    assert_eq!(
        objects.trace_object_ids(non_empty).unwrap(),
        vec![first, second]
    );
}

#[test]
fn wasmtime_type_layout_mapping_scalar_array_uses_descriptor_element_size() {
    let mut objects = ObjectTable::default();
    let namespace = 46;
    let layout_id = objects
        .persistent_type_layout_id_for_wasmtime_key(
            namespace,
            59,
            PersistentTypeKind::Array,
            wasmtime_array_layout_fingerprint(1, false),
        )
        .unwrap();

    let object = objects
        .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
            0x591,
            namespace,
            59,
            1,
            false,
            vec![ObjectValue::I32(1), ObjectValue::I32(2)],
        )
        .unwrap();

    assert_eq!(
        objects.live_slot(object).unwrap().type_layout_id,
        layout_id.get()
    );
    assert_eq!(
        objects.require_type_layout(layout_id).unwrap(),
        &persistent_layout_for_wasmtime_array_type(layout_id, 1, false)
    );
}

#[test]
fn object_table_rejects_allocation_with_missing_struct_layout_id() {
    let mut objects = ObjectTable::default();

    let err = objects
        .allocate_payload_with_type_layout_id_for_test(
            ObjectPayload::Struct(vec![ObjectValue::I32(1)]),
            type_layout::TypeLayoutId::new(99).unwrap(),
        )
        .unwrap_err();

    assert!(err.to_string().contains("unknown persistent type layout"));
}

#[test]
fn object_table_allows_allocation_with_registered_struct_layout_id() {
    let mut objects = ObjectTable::default();
    let layout = type_layout::PersistentTypeLayout::Struct {
        id: type_layout::TypeLayoutId::new(101).unwrap(),
        fingerprint: 0x0101_0000_0000_0001,
        body_size: 16,
        fields: vec![type_layout::StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: 8,
            kind: type_layout::TraceSlotKind::Scalar,
        }],
    };
    objects.register_type_layout(layout.clone()).unwrap();

    let object = objects
        .allocate_payload_with_type_layout_id_for_test(
            ObjectPayload::Struct(vec![ObjectValue::I32(1)]),
            layout.id(),
        )
        .unwrap();
    let handle = objects.current_record_handle_for_test(object).unwrap();
    let header = objects.heap.header(handle).unwrap();

    assert_eq!(
        objects.live_slot(object).unwrap().type_layout_id,
        layout.id().get()
    );
    assert_eq!(header.type_layout_id, layout.id().get());
}

#[test]
fn object_table_public_struct_and_array_allocations_use_nonzero_layout_ids() {
    let mut objects = ObjectTable::default();

    let struct_object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    let array_object = objects.allocate_array(vec![ObjectValue::I32(2)]).unwrap();

    assert_ne!(
        objects.live_slot(struct_object).unwrap().type_layout_id,
        0,
        "struct allocation must not use layout id zero"
    );
    assert_ne!(
        objects.live_slot(array_object).unwrap().type_layout_id,
        0,
        "array allocation must not use layout id zero"
    );
}

#[test]
fn object_table_rejects_inline_value_kind_allocations() {
    let mut objects = ObjectTable::default();

    for kind in [ObjectKind::I31, ObjectKind::Extern, ObjectKind::Func] {
        let err = objects.allocate(kind).unwrap_err();
        assert!(
            err.to_string()
                .contains("durable values, not object-table payloads"),
            "{err:?}"
        );
    }
    assert_eq!(objects.live_count(), 0);
}

#[test]
fn live_bridge_object_id_for_raw_ref_or_func_rejects_unknown_function_ref_without_durable_identity()
{
    let mut objects = ObjectTable::default();
    let raw_ref = u64::from(u32::MAX) + 2;

    let err = objects
        .live_bridge_object_id_for_raw_ref_or_func(raw_ref)
        .unwrap_err();

    assert!(
        err.to_string().contains(
            "transactional function reference promotion requires durable function identity"
        ),
        "{err:?}"
    );
    assert_eq!(objects.live_count(), 0);
}

#[test]
fn object_table_granule_permissions_use_object_identity() {
    let mut objects = ObjectTable::default();
    let struct_object = objects.allocate(ObjectKind::Struct).unwrap();
    let array_object = objects.allocate(ObjectKind::Array).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    assert!(
        state
            .acquire_object_read(&mut objects, struct_object)
            .unwrap()
    );
    assert!(state.owns_granule_read(GranuleId::Object {
        object_id: struct_object,
    }));
    assert!(!state.owns_granule_write(GranuleId::Object {
        object_id: struct_object,
    }));

    assert!(
        state
            .acquire_object_write(&mut objects, array_object)
            .unwrap()
    );
    assert!(state.owns_granule_read(GranuleId::Object {
        object_id: array_object,
    }));
    assert!(state.owns_granule_write(GranuleId::Object {
        object_id: array_object,
    }));

    state.abort().unwrap();

    assert!(!state.owns_granule_read(GranuleId::Object {
        object_id: struct_object,
    }));
    assert!(!state.owns_granule_write(GranuleId::Object {
        object_id: array_object,
    }));
}

#[test]
fn persistent_object_permissions_use_object_granule_identity() {
    let mut objects = ObjectTable::default();
    let struct_object = objects
        .allocate_persistent_struct_for_gc_ref(0x61, vec![ObjectValue::I32(7)])
        .unwrap();
    let array_object = objects
        .allocate_persistent_array_for_gc_ref(0x62, vec![ObjectValue::I32(9)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    assert!(
        state
            .acquire_object_read(&mut objects, struct_object)
            .unwrap()
    );
    assert!(state.owns_granule_read(GranuleId::Object {
        object_id: struct_object,
    }));
    assert!(!state.owns_granule_write(GranuleId::Object {
        object_id: struct_object,
    }));

    assert!(
        state
            .acquire_object_write(&mut objects, array_object)
            .unwrap()
    );
    assert!(state.owns_granule_read(GranuleId::Object {
        object_id: array_object,
    }));
    assert!(state.owns_granule_write(GranuleId::Object {
        object_id: array_object,
    }));
}

#[test]
fn persistent_object_ref_raw_encodes_null_and_object_ids() {
    let null = PersistentObjectRefRaw::from_optional_object_id(None).unwrap();
    assert_eq!(null.as_raw(), 0);
    assert_eq!(PersistentObjectRefRaw::from_raw(0).decode(), None);

    let object = ObjectId { object_index: 41 };
    let encoded = PersistentObjectRefRaw::from_optional_object_id(Some(object)).unwrap();
    assert_eq!(encoded.as_raw(), 42);
    assert_eq!(encoded.decode(), Some(object));

    let max_encodable_object = ObjectId {
        object_index: u64::MAX - 1,
    };
    let encoded =
        PersistentObjectRefRaw::from_optional_object_id(Some(max_encodable_object)).unwrap();
    assert_eq!(encoded.as_raw(), u64::MAX);
    assert_eq!(encoded.decode(), Some(max_encodable_object));
}

#[test]
fn persistent_object_ref_raw_rejects_ref_encoding_overflow() {
    let error = PersistentObjectRefRaw::from_optional_object_id(Some(ObjectId {
        object_index: u64::MAX,
    }))
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("object reference encoding overflow")
    );
}

#[test]
fn transaction_object_receiver_handles_reject_gc_bridge_and_durable_raw_refs() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x715, vec![ObjectValue::I32(7)])
        .unwrap();
    let durable_raw = PersistentObjectRefRaw::from_optional_object_id(Some(object))
        .unwrap()
        .as_raw();
    let handle = objects
        .transaction_ref_handle_for_object_id(object)
        .unwrap();

    assert_eq!(
        objects
            .object_id_for_transaction_ref_handle(handle)
            .unwrap(),
        object
    );

    let gc_bridge_error = objects
        .object_id_for_transaction_ref_handle(0x715)
        .unwrap_err();
    assert!(
        gc_bridge_error
            .to_string()
            .contains("unknown transaction object ref handle"),
        "{gc_bridge_error:?}"
    );

    if let Ok(raw) = u32::try_from(durable_raw) {
        let durable_raw_error = objects
            .object_id_for_transaction_ref_handle(raw)
            .unwrap_err();
        assert!(
            durable_raw_error
                .to_string()
                .contains("unknown transaction object ref handle"),
            "{durable_raw_error:?}"
        );
    }
}

#[test]
fn transaction_object_abi_layout_is_explicit() {
    assert_eq!(
        core::mem::size_of::<PersistentObjectRefRaw>(),
        core::mem::size_of::<u64>()
    );
    assert_eq!(
        core::mem::align_of::<PersistentObjectRefRaw>(),
        core::mem::align_of::<u64>()
    );
    assert_eq!(
        core::mem::size_of::<TransactionObjectRefRaw>(),
        core::mem::size_of::<u32>()
    );
    assert_eq!(
        core::mem::align_of::<TransactionObjectRefRaw>(),
        core::mem::align_of::<u32>()
    );
    assert_eq!(TransactionObjectRefRaw::from_raw(0).decode(), None);
    assert_eq!(core::mem::size_of::<ObjectValueAbi>(), 24);
    assert_eq!(
        core::mem::align_of::<ObjectValueAbi>(),
        core::mem::align_of::<u64>()
    );
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED, 0);
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_GC, 1);
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC, 2);
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_I31, 3);
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN, 4);
    assert_eq!(OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT, 5);
}

#[test]
fn transaction_object_abi_roundtrips_object_values() {
    let func = DurableFuncIdentity {
        module_fingerprint: 0x10_20_30_40_50_60_70_80,
        function_index: 7,
        type_layout_id: type_layout::TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_ = DurableExternIdentity {
        namespace: 4,
        handle: 0xabc,
        type_layout_id: type_layout::TypeLayoutId::BUILTIN_EXTERN,
    };
    let values = [
        ObjectValue::I31(-17),
        ObjectValue::I32(-17),
        ObjectValue::I64(-18),
        ObjectValue::F32(0x7fc0_0001),
        ObjectValue::F64(0x7ff8_0000_0000_0001),
        ObjectValue::V128([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        ObjectValue::Ref(None),
        ObjectValue::Ref(Some(ObjectId { object_index: 8 })),
        ObjectValue::FuncRef(func),
        ObjectValue::ExternRef(extern_),
    ];

    for value in values {
        let abi = ObjectValueAbi::from_object_value(&value).unwrap();
        assert_eq!(abi.to_object_value().unwrap(), value);
    }

    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::Ref(Some(ObjectId { object_index: 8 })))
            .unwrap()
            .as_parts(),
        (OBJECT_VALUE_ABI_TAG_REF, 9, 0)
    );
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::V128([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        ]))
        .unwrap()
        .as_parts(),
        (
            OBJECT_VALUE_ABI_TAG_V128,
            0x0706_0504_0302_0100,
            0x0f0e_0d0c_0b0a_0908,
        )
    );
}

#[test]
fn persistent_object_value_abi_ref_uses_object_id_not_gc_ref_side_map() -> Result<()> {
    let object_id = ObjectId { object_index: 41 };
    let abi = ObjectValueAbi::from_object_value(&ObjectValue::Ref(Some(object_id)))?;

    assert_eq!(
        abi.as_parts(),
        (
            OBJECT_VALUE_ABI_TAG_REF,
            PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
            0
        )
    );
    assert_eq!(abi.to_object_value()?, ObjectValue::Ref(Some(object_id)));
    Ok(())
}

#[test]
fn persistent_ref_abi_ignores_associated_live_gc_ref() -> Result<()> {
    let mut objects = ObjectTable::default();
    let object_id =
        objects.allocate_persistent_struct_for_gc_ref(0x710, vec![ObjectValue::I32(1)])?;

    assert_eq!(
        objects.known_object_id_for_live_gc_ref_bridge(0x710),
        Some(object_id)
    );
    assert_eq!(
        objects.persistent_ref_abi_for_object_id(object_id)?,
        (
            PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        )
    );
    assert_eq!(
        objects.live_bridge_ref_abi_for_object_id(object_id)?,
        (0x710, OBJECT_VALUE_ABI_LIVE_REF_KIND_GC)
    );
    Ok(())
}

#[test]
fn transaction_object_ref_handle_supports_full_width_object_id() -> Result<()> {
    let mut objects = ObjectTable::default();
    let object_id =
        objects.allocate_persistent_struct_for_gc_ref(0x711, vec![ObjectValue::I32(1)])?;
    let missing_full_width_object_id = ObjectId {
        object_index: u64::from(u32::MAX) + 1,
    };

    let durable_ref =
        PersistentObjectRefRaw::from_optional_object_id(Some(missing_full_width_object_id))?;
    assert!(durable_ref.as_raw() > u64::from(u32::MAX));

    let handle = objects.transaction_ref_handle_for_object_id(object_id)?;
    assert_ne!(handle, 0);
    assert_eq!(
        objects.object_id_for_transaction_ref_handle(handle)?,
        object_id
    );
    assert_eq!(
        objects.known_persistent_object_id_for_transaction_ref_handle(handle)?,
        Some(object_id)
    );
    assert_eq!(
        objects.transaction_ref_handle_for_object_id(object_id)?,
        handle
    );

    let error = objects
        .transaction_ref_handle_for_object_id(missing_full_width_object_id)
        .unwrap_err();
    assert!(error.to_string().contains("object table slot is not live"));
    Ok(())
}

#[test]
fn transaction_object_ref_handles_do_not_collide_with_live_gc_bridge_refs() -> Result<()> {
    let mut objects = ObjectTable::default();
    let gc_object = objects.allocate_persistent_struct_for_gc_ref(
        FIRST_TRANSACTION_OBJECT_REF_HANDLE,
        vec![ObjectValue::I32(1)],
    )?;
    let handle_object =
        objects.allocate_persistent_struct_for_gc_ref(0x712, vec![ObjectValue::I32(2)])?;

    assert_eq!(
        objects.object_id_for_live_gc_ref_bridge(FIRST_TRANSACTION_OBJECT_REF_HANDLE)?,
        gc_object
    );
    let handle = objects.transaction_ref_handle_for_object_id(handle_object)?;
    assert_ne!(handle, FIRST_TRANSACTION_OBJECT_REF_HANDLE);
    assert_eq!(
        objects.object_id_for_transaction_ref_handle(handle)?,
        handle_object
    );
    let live_count_before_collision = objects.live_count();

    let error = objects
        .allocate_struct_for_gc_ref(handle, vec![ObjectValue::I32(3)])
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transactional object GC ref collides with transaction object handle")
    );
    assert_eq!(objects.live_count(), live_count_before_collision);

    let error = objects
        .allocate_persistent_struct_for_gc_ref(handle, vec![ObjectValue::I32(4)])
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transactional object GC ref collides with transaction object handle")
    );
    assert_eq!(objects.live_count(), live_count_before_collision);

    let other_object =
        objects.allocate_persistent_struct_for_gc_ref(0x714, vec![ObjectValue::I32(3)])?;
    let error = objects
        .associate_live_gc_ref_for_transaction_bridge(handle, other_object)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transactional object GC ref collides with transaction object handle")
    );
    Ok(())
}

#[test]
fn persistent_ref_abi_live_bridge_rejects_overflow() {
    let error = ObjectTable::persistent_ref_raw_for_live_bridge(ObjectId {
        object_index: u64::from(u32::MAX),
    })
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("persistent object ref does not fit live transaction ref bridge")
    );
}

#[test]
fn persistent_ref_abi_live_bridge_rejects_volatile_object_without_live_ref() -> Result<()> {
    let mut objects = ObjectTable::default();
    let object_id = objects.allocate_struct(vec![ObjectValue::I32(1)])?;

    let error = objects
        .live_bridge_ref_abi_for_object_id(object_id)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("persistent ref ABI requires a persistent object")
    );
    Ok(())
}

#[test]
fn persistent_object_value_abi_func_and_extern_use_durable_identities() -> Result<()> {
    let func = DurableFuncIdentity {
        module_fingerprint: 0x1234,
        function_index: 7,
        type_layout_id: TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_ = DurableExternIdentity {
        namespace: 9,
        handle: 0xabc,
        type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
    };

    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::FuncRef(func))?.to_object_value()?,
        ObjectValue::FuncRef(func)
    );
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::ExternRef(extern_))?.to_object_value()?,
        ObjectValue::ExternRef(extern_)
    );
    Ok(())
}

#[test]
fn durable_func_and_extern_values_are_not_object_payloads() {
    assert!(ObjectPayload::default_for_kind(ObjectKind::Func).is_err());
    assert!(ObjectPayload::default_for_kind(ObjectKind::Extern).is_err());
}

#[test]
fn inline_i31_values_are_not_object_payloads() {
    assert!(ObjectPayload::default_for_kind(ObjectKind::I31).is_err());
}

#[test]
fn object_value_abi_func_extern_does_not_need_live_registry() -> Result<()> {
    let durable_refs = DurableReferenceRegistry::default();
    let func = DurableFuncIdentity {
        module_fingerprint: 0xfeed,
        function_index: 3,
        type_layout_id: TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_ = DurableExternIdentity {
        namespace: 4,
        handle: 0xbeef,
        type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
    };

    assert!(durable_refs.resolve_func_identity(func).is_none());
    assert!(durable_refs.resolve_extern_identity(extern_).is_none());
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::FuncRef(func))?.to_object_value()?,
        ObjectValue::FuncRef(func)
    );
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::ExternRef(extern_))?.to_object_value()?,
        ObjectValue::ExternRef(extern_)
    );
    Ok(())
}

#[test]
fn durable_func_extern_registry_hooks_rebind_live_refs_after_reopen() -> Result<()> {
    let engine = crate::Engine::default();
    let mut store = crate::Store::new(&engine, ());
    let func = crate::Func::wrap(&mut store, || {});
    let func_identity = DurableFuncIdentity {
        module_fingerprint: 0x5142_0000_0000_0001,
        function_index: 4,
        type_layout_id: TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_identity = DurableExternIdentity {
        namespace: 0x5142,
        handle: 0x5142_0000_0000_0002,
        type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
    };

    assert!(
        store
            .transaction_resolve_durable_func_ref_for_test(func_identity)?
            .is_none()
    );
    assert_eq!(
        store.transaction_resolve_durable_extern_ref_for_test(extern_identity),
        None
    );

    store.transaction_register_durable_func_ref_for_test(&func, func_identity)?;
    store.transaction_register_durable_extern_ref_for_test(0x5142, extern_identity)?;

    assert!(
        store
            .transaction_resolve_durable_func_ref_for_test(func_identity)?
            .is_some()
    );
    assert_eq!(
        store.transaction_resolve_durable_extern_ref_for_test(extern_identity),
        Some(0x5142)
    );
    Ok(())
}

#[test]
fn transaction_object_abi_rejects_unknown_and_noncanonical_values() {
    let error = ObjectValueAbi::from_parts(99, 0, 0).unwrap_err();
    assert!(error.to_string().contains("unknown object value ABI tag"));

    let error = ObjectValueAbi::from_parts(OBJECT_VALUE_ABI_TAG_I32, u64::from(u32::MAX) + 1, 0)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("non-canonical i32 object value ABI payload")
    );

    let error =
        ObjectValueAbi::from_parts(OBJECT_VALUE_ABI_TAG_I31, u64::from(I31Value::MASK) + 1, 0)
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("non-canonical i31 object value ABI payload")
    );

    let error = ObjectValueAbi::from_parts(OBJECT_VALUE_ABI_TAG_REF, 0, 1).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("non-canonical ref object value ABI payload")
    );
}

#[test]
fn object_payload_updates_are_copy_on_write_until_commit() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    assert_eq!(
        state.read_object_payload(&objects, object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
    );
    state
        .stage_object_payload(
            &objects,
            object,
            ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
        )
        .unwrap();
    assert_eq!(
        state.read_object_payload(&objects, object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)])
    );
    state.abort().unwrap();
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
    );

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_object_payload(
            &objects,
            object,
            ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
        )
        .unwrap();
    assert!(state.commit_object_payloads(&mut objects).unwrap());
    state.complete_commit().unwrap();

    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)])
    );
}

#[test]
fn transaction_state_publishes_committed_object_payload_to_file_backed_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx-log.bin");
    let durable_log = TxDurableLog::create_file_backed(&path, 32).unwrap();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(
            0x100,
            vec![ObjectValue::I32(1), ObjectValue::I64(2)],
        )
        .unwrap();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(12), durable_log);

    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    let marker = state
        .publish_object_publications_before_commit(12, 12, &objects, &publications)
        .unwrap()
        .unwrap();
    state.publish_commit_lp(12, 12, marker).unwrap();
    drop(state);

    let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, object.object_index);
    assert_eq!(recovered.object_winners[0].version, 2);
}

#[test]
fn transaction_state_publish_commit_lp_succeeds_when_post_lp_retirement_fails() {
    let (durable_log, events) = TxDurableLog::recording_backend_with_retire_failure_for_test();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(18), durable_log);
    let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);

    let marker = state
        .publish_tmemory_undo_before_in_place_write(18, 18, &undo)
        .unwrap();
    assert!(state.publish_commit_lp(18, 18, marker).is_ok());
    state.complete_commit().unwrap();

    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&18)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );
    assert!(events.lock().unwrap().contains(
        &persist::RecordingBackendEvent::RetireCommittedLinearUndoChunk(marker.chunk_start_block)
    ));
    assert!(events.lock().unwrap().contains(
        &persist::RecordingBackendEvent::RetireCommittedLinearUndoChunkFailed(
            marker.chunk_start_block
        )
    ));
}

#[test]
fn post_commit_linear_undo_cleanup_retries_on_abort_and_drains_queue() {
    let (durable_log, events) = TxDurableLog::recording_backend_with_retire_failures_for_test(4);
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(19), durable_log);
    let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_0031, 3, vec![4, 5, 6, 7]);

    let marker = state
        .publish_tmemory_undo_before_in_place_write(19, 19, &undo)
        .unwrap();
    state.publish_commit_lp(19, 19, marker).unwrap();
    state.complete_commit().unwrap();

    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&19)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );

    state.begin().unwrap();
    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&19)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );
    state.abort().unwrap();

    assert!(!state.post_commit_linear_undo_chunks.contains_key(&19));
    let events = events.lock().unwrap();
    let total_attempts =
        count_retire_committed_linear_undo_attempts(&events, marker.chunk_start_block);
    let failed_attempts =
        count_retire_committed_linear_undo_failures(&events, marker.chunk_start_block);
    assert!(failed_attempts >= 1);
    assert!(total_attempts > failed_attempts);
}

#[test]
fn post_commit_linear_undo_cleanup_retries_on_abort_allocated_objects_and_drains_queue() {
    let (durable_log, events) = TxDurableLog::recording_backend_with_retire_failures_for_test(4);
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(20), durable_log);
    let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_0032, 4, vec![7, 8, 9, 10]);

    let marker = state
        .publish_tmemory_undo_before_in_place_write(20, 20, &undo)
        .unwrap();
    state.publish_commit_lp(20, 20, marker).unwrap();
    state.complete_commit().unwrap();

    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&20)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );

    let mut objects = ObjectTable::default();
    state.begin().unwrap();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    state.record_allocated_object(object).unwrap();
    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&20)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );
    state.abort_allocated_objects(&mut objects).unwrap();

    assert!(!state.post_commit_linear_undo_chunks.contains_key(&20));
    let events = events.lock().unwrap();
    let total_attempts =
        count_retire_committed_linear_undo_attempts(&events, marker.chunk_start_block);
    let failed_attempts =
        count_retire_committed_linear_undo_failures(&events, marker.chunk_start_block);
    assert!(failed_attempts >= 1);
    assert!(total_attempts > failed_attempts);
}

#[test]
fn post_commit_linear_undo_cleanup_retries_on_suspended_abort_and_drains_queue() {
    let (durable_log, events) = TxDurableLog::recording_backend_with_retire_failures_for_test(5);
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(21), durable_log);
    let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_0033, 5, vec![11, 12, 13, 14]);

    let marker = state
        .publish_tmemory_undo_before_in_place_write(21, 21, &undo)
        .unwrap();
    state.publish_commit_lp(21, 21, marker).unwrap();
    state.complete_commit().unwrap();

    let suspended = TransactionId::from_raw(121);
    assert_eq!(state.enter_transaction(suspended).unwrap(), None);
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(suspended));
    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&21)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );
    assert!(state.abort_transaction(suspended).unwrap());

    assert!(!state.post_commit_linear_undo_chunks.contains_key(&21));
    let events = events.lock().unwrap();
    let total_attempts =
        count_retire_committed_linear_undo_attempts(&events, marker.chunk_start_block);
    let failed_attempts =
        count_retire_committed_linear_undo_failures(&events, marker.chunk_start_block);
    assert!(failed_attempts >= 1);
    assert!(total_attempts > failed_attempts);
}

#[test]
fn post_commit_linear_undo_cleanup_retries_on_suspended_abort_allocated_objects_and_drains_queue() {
    let (durable_log, events) = TxDurableLog::recording_backend_with_retire_failures_for_test(5);
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(22), durable_log);
    let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_0034, 6, vec![15, 16, 17, 18]);

    let marker = state
        .publish_tmemory_undo_before_in_place_write(22, 22, &undo)
        .unwrap();
    state.publish_commit_lp(22, 22, marker).unwrap();
    state.complete_commit().unwrap();

    let suspended = TransactionId::from_raw(122);
    let mut objects = ObjectTable::default();
    assert_eq!(state.enter_transaction(suspended).unwrap(), None);
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    state.record_allocated_object(object).unwrap();
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(suspended));
    assert_eq!(
        state
            .post_commit_linear_undo_chunks
            .get(&22)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![marker.chunk_start_block]
    );
    assert!(
        state
            .abort_transaction_allocated_objects(&mut objects, suspended)
            .unwrap()
    );

    assert!(!state.post_commit_linear_undo_chunks.contains_key(&22));
    let events = events.lock().unwrap();
    let total_attempts =
        count_retire_committed_linear_undo_attempts(&events, marker.chunk_start_block);
    let failed_attempts =
        count_retire_committed_linear_undo_failures(&events, marker.chunk_start_block);
    assert!(failed_attempts >= 1);
    assert!(total_attempts > failed_attempts);
}

#[test]
fn transaction_state_rejects_unknown_object_publication_layout_before_log_write() {
    let (durable_log, events) = TxDurableLog::recording_backend_for_test();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(18), durable_log);
    let objects = ObjectTable::default();
    let publication = persist::PendingPublication::persistent_object(
        crate::runtime::vm::PackedGranuleDomain::TStruct,
        41,
        7,
        999,
        encode_object_record_for_test(
            41,
            7,
            999,
            &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        )
        .unwrap(),
    )
    .unwrap();

    let err = state
        .publish_object_publications_before_commit(18, 18, &objects, &[publication])
        .unwrap_err()
        .to_string();

    assert!(err.contains("unknown persistent type layout id: 999"));
    assert!(state.durable_log_entries_for_test(18).is_empty());
    let events = events.lock().unwrap();
    assert!(!events.iter().any(|event| {
        matches!(
            event,
            persist::RecordingBackendEvent::AppendDataRecord(
                persist::DurableDataStream::ObjectPublication
            )
        )
    }));
    assert!(!events.iter().any(|event| {
        matches!(
            event,
            persist::RecordingBackendEvent::AppendLogEntry(
                crate::runtime::vm::TxLogEntryRole::TObjectPub
            )
        )
    }));
}

#[test]
fn transaction_state_persists_type_layout_before_object_publication_writes() {
    let (durable_log, events) = TxDurableLog::recording_backend_for_test();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(19), durable_log);
    let mut objects = ObjectTable::default();
    let layout = type_layout::PersistentTypeLayout::Struct {
        id: type_layout::TypeLayoutId::new(101).unwrap(),
        fingerprint: 0x0101_0000_0000_0001,
        body_size: 16,
        fields: vec![type_layout::StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: 8,
            kind: type_layout::TraceSlotKind::Scalar,
        }],
    };
    objects.register_type_layout(layout.clone()).unwrap();
    let publication = persist::PendingPublication::persistent_object(
        crate::runtime::vm::PackedGranuleDomain::TStruct,
        41,
        7,
        layout.id().get(),
        encode_object_record_for_test(
            41,
            7,
            layout.id().get(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        )
        .unwrap(),
    )
    .unwrap();

    let marker = state
        .publish_object_publications_before_commit(19, 19, &objects, &[publication])
        .unwrap()
        .unwrap();
    assert_eq!(marker.role, crate::runtime::vm::TxLogEntryRole::TObjectPub);

    let events = events.lock().unwrap().clone();
    let ensure_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::EnsureTypeLayout(id) if *id == layout.id().get()
            )
        })
        .unwrap();
    let data_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::AppendDataRecord(
                    persist::DurableDataStream::ObjectPublication
                )
            )
        })
        .unwrap();
    let log_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                persist::RecordingBackendEvent::AppendLogEntry(
                    crate::runtime::vm::TxLogEntryRole::TObjectPub
                )
            )
        })
        .unwrap();

    assert!(ensure_index < data_index);
    assert!(ensure_index < log_index);
}

#[test]
fn transaction_state_does_not_publish_volatile_object_payload_to_durable_log() {
    let mut objects = ObjectTable::default();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );

    assert!(publications.is_empty());
}

#[test]
fn file_backed_mixed_commit_recovers_tmemory_and_reuses_committed_linear_undo_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let mut tmemory = crate::runtime::vm::TMemory::new(
        TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
        1,
        Some(1),
    )
    .unwrap();
    let old_granule = vec![0x11; TMEMORY_GRANULE_SIZE];
    let new_granule = vec![0x22; TMEMORY_GRANULE_SIZE];
    tmemory
        .commit_staged_tmemory_granule(0, &old_granule)
        .unwrap();

    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(31), durable_log);
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x101, vec![ObjectValue::I32(1)])
        .unwrap();

    let undo = tmemory
        .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
        .unwrap();
    let tmemory_marker = state
        .publish_tmemory_undo_before_in_place_write(31, 31, &undo)
        .unwrap();
    tmemory
        .commit_staged_tmemory_granule(0, &new_granule)
        .unwrap();

    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    let object_marker = state
        .publish_object_publications_before_commit(31, 31, &objects, &publications)
        .unwrap();
    state
        .publish_commit_lp(31, 31, object_marker.unwrap_or(tmemory_marker))
        .unwrap();
    drop(state);
    clear_current_thread_transaction_for_test();

    let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
    assert!(recovered.tmemory_undo_rollbacks.is_empty());
    tmemory
        .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
            recovered.tmemory_undo_rollbacks,
        )
        .unwrap();
    assert_eq!(
        tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
        new_granule
    );
    let tmemory_file = std::fs::read(&tmemory_path).unwrap();
    assert_eq!(
        &tmemory_file[..TMEMORY_GRANULE_SIZE],
        new_granule.as_slice()
    );

    let recovered_region =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(
            &tx_log_path,
        )
        .unwrap();
    let object_winners = recovered_region.committed_object_winners().unwrap();
    let mut recovered_objects = ObjectTable::default();
    recovered_objects
        .rebuild_from_recovery_for_test(&recovered_region.type_layouts, &object_winners)
        .unwrap();
    assert_eq!(
        recovered_objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(9)])
    );

    let mut region =
        crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(&tx_log_path)
            .unwrap();
    let stream = region.alloc_stream(32).unwrap();
    let record = crate::runtime::vm::TMemory::encode_granule_undo_data_record(
        0x1000_0000_0000_0008,
        1,
        crate::runtime::vm::PackedGranuleDomain::TMemory as u16,
        0,
        &[0x33; 16],
    )
    .unwrap();
    let location = region.append_data_record(stream, &record).unwrap();

    assert_eq!(location.chunk_start_block, tmemory_marker.chunk_start_block);
    assert_eq!(
        region.block_generation(location.data_block).unwrap(),
        tmemory_marker.data_block_generation + 1
    );
}

#[test]
fn file_backed_mixed_loose_end_rolls_back_tmemory_and_drops_object_publication() {
    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let mut tmemory = crate::runtime::vm::TMemory::new(
        TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
        1,
        Some(1),
    )
    .unwrap();
    let old_granule = vec![0x33; TMEMORY_GRANULE_SIZE];
    let new_granule = vec![0x44; TMEMORY_GRANULE_SIZE];
    tmemory
        .commit_staged_tmemory_granule(0, &old_granule)
        .unwrap();

    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(32), durable_log);
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x102, vec![ObjectValue::I32(1)])
        .unwrap();

    let undo = tmemory
        .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
        .unwrap();
    state
        .publish_tmemory_undo_before_in_place_write(32, 32, &undo)
        .unwrap();
    tmemory
        .commit_staged_tmemory_granule(0, &new_granule)
        .unwrap();

    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    state
        .publish_object_publications_before_commit(32, 32, &objects, &publications)
        .unwrap();
    drop(state);
    clear_current_thread_transaction_for_test();

    let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
    assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
    assert!(recovered.object_winners.is_empty());
    tmemory
        .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
            recovered.tmemory_undo_rollbacks,
        )
        .unwrap();
    assert_eq!(
        tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
        old_granule
    );
    let tmemory_file = std::fs::read(&tmemory_path).unwrap();
    assert_eq!(
        &tmemory_file[..TMEMORY_GRANULE_SIZE],
        old_granule.as_slice()
    );

    let object_winners =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
            &tx_log_path,
        )
        .unwrap();
    assert!(object_winners.is_empty());
}

mod file_backed_object_layout_recovery {
    use super::*;

    #[test]
    fn persistent_struct_with_only_scalars_recovers_payload_and_layout() {
        let ((object, layout_id), recovered) =
            recover_file_backed_object_layout_case_for_test(201, |objects, state| {
                let object = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x901,
                        201,
                        1,
                        vec![ObjectValue::I32(1), ObjectValue::I64(2)],
                    )?;
                let layout_id = objects.live_slot(object)?.type_layout_id;
                state.acquire_object_write(objects, object)?;
                state.stage_struct_field(objects, object, 0, ObjectValue::I32(9))?;
                Ok((object, layout_id))
            })
            .unwrap();

        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, object.object_index);
        assert_eq!(recovered.object_winners[0].type_layout_id, layout_id);
        assert_eq!(
            recovered.rebuilt.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(9), ObjectValue::I64(2)])
        );
        assert_eq!(
            recovered.rebuilt.live_slot(object).unwrap().type_layout_id,
            layout_id
        );
        assert!(
            recovered
                .recovered_region
                .type_layouts
                .contains(type_layout::TypeLayoutId::new(layout_id).unwrap())
        );
    }

    #[test]
    fn persistent_struct_with_object_reference_recovers_payload_and_identity() {
        let ((owner, target, owner_layout_id), recovered) =
            recover_file_backed_object_layout_case_for_test(202, |objects, state| {
                let target = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x902,
                        202,
                        2,
                        vec![ObjectValue::I32(1)],
                    )?;
                let owner = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x903,
                        202,
                        3,
                        vec![ObjectValue::I32(4), ObjectValue::Ref(None)],
                    )?;
                let owner_layout_id = objects.live_slot(owner)?.type_layout_id;
                state.acquire_object_write(objects, target)?;
                state.stage_struct_field(objects, target, 0, ObjectValue::I32(7))?;
                state.acquire_object_write(objects, owner)?;
                state.stage_struct_field(objects, owner, 0, ObjectValue::I32(9))?;
                state.stage_struct_field(objects, owner, 1, ObjectValue::Ref(Some(target)))?;
                Ok((owner, target, owner_layout_id))
            })
            .unwrap();

        assert_eq!(
            recovered.rebuilt.payload(target).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(7)])
        );
        assert_eq!(
            recovered.rebuilt.payload(owner).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(9), ObjectValue::Ref(Some(target)),])
        );
        assert_eq!(
            recovered.rebuilt.trace_object_ids(owner).unwrap(),
            vec![target]
        );
        assert_eq!(
            recovered.rebuilt.live_slot(owner).unwrap().type_layout_id,
            owner_layout_id
        );
    }

    #[test]
    fn durable_func_extern_refs_survive_object_payload_recovery() {
        let func = DurableFuncIdentity {
            module_fingerprint: 0x2060_0000_0000_0001,
            function_index: 3,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let extern_ = DurableExternIdentity {
            namespace: 206,
            handle: 0x2060_0000_0000_0002,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };

        let ((object, layout_id), recovered) =
            recover_file_backed_object_layout_case_for_test(206, |objects, state| {
                let object = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x920,
                        206,
                        10,
                        vec![ObjectValue::I32(0), ObjectValue::I32(0)],
                    )?;
                let layout_id = objects.live_slot(object)?.type_layout_id;
                state.acquire_object_write(objects, object)?;
                state.stage_struct_field(objects, object, 0, ObjectValue::FuncRef(func))?;
                state.stage_struct_field(objects, object, 1, ObjectValue::ExternRef(extern_))?;
                Ok((object, layout_id))
            })
            .unwrap();

        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, object.object_index);
        assert_eq!(recovered.object_winners[0].type_layout_id, layout_id);
        assert_eq!(
            recovered.rebuilt.payload(object).unwrap(),
            ObjectPayload::Struct(vec![
                ObjectValue::FuncRef(func),
                ObjectValue::ExternRef(extern_),
            ])
        );
        assert_eq!(
            recovered.rebuilt.trace_object_ids(object).unwrap(),
            Vec::new()
        );
        assert_eq!(recovered.rebuilt.live_count(), 1);
    }

    #[test]
    fn persistent_array_of_scalars_recovers_payload_and_layout() {
        let ((object, layout_id), recovered) =
            recover_file_backed_object_layout_case_for_test(203, |objects, state| {
                let object = objects
                    .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
                        0x904,
                        203,
                        4,
                        4,
                        false,
                        vec![
                            ObjectValue::I32(1),
                            ObjectValue::I32(2),
                            ObjectValue::I32(3),
                        ],
                    )?;
                let layout_id = objects.live_slot(object)?.type_layout_id;
                state.acquire_object_write(objects, object)?;
                state.stage_array_element(objects, object, 1, ObjectValue::I32(9))?;
                Ok((object, layout_id))
            })
            .unwrap();

        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, object.object_index);
        assert_eq!(recovered.object_winners[0].type_layout_id, layout_id);
        assert_eq!(
            recovered.rebuilt.payload(object).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::I32(1),
                ObjectValue::I32(9),
                ObjectValue::I32(3),
            ])
        );
        assert_eq!(
            recovered.rebuilt.live_slot(object).unwrap().type_layout_id,
            layout_id
        );
    }

    #[test]
    fn persistent_array_of_object_references_recovers_payload_and_identity() {
        let ((array, first, second, layout_id), recovered) =
            recover_file_backed_object_layout_case_for_test(204, |objects, state| {
                let first = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x905,
                        204,
                        5,
                        vec![ObjectValue::I32(1)],
                    )?;
                let second = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x906,
                        204,
                        6,
                        vec![ObjectValue::I32(2)],
                    )?;
                let array = objects
                    .allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
                        0x907,
                        204,
                        7,
                        PERSISTENT_OBJECT_ABI_SLOT_SIZE,
                        true,
                        vec![ObjectValue::Ref(None), ObjectValue::Ref(Some(first))],
                    )?;
                let layout_id = objects.live_slot(array)?.type_layout_id;
                state.acquire_object_write(objects, first)?;
                state.stage_struct_field(objects, first, 0, ObjectValue::I32(11))?;
                state.acquire_object_write(objects, second)?;
                state.stage_struct_field(objects, second, 0, ObjectValue::I32(22))?;
                state.acquire_object_write(objects, array)?;
                state.stage_array_element(objects, array, 0, ObjectValue::Ref(Some(second)))?;
                Ok((array, first, second, layout_id))
            })
            .unwrap();

        assert_eq!(
            recovered.rebuilt.payload(array).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::Ref(Some(second)),
                ObjectValue::Ref(Some(first)),
            ])
        );
        assert_eq!(
            recovered.rebuilt.trace_object_ids(array).unwrap(),
            vec![second, first]
        );
        assert_eq!(
            recovered.rebuilt.live_slot(array).unwrap().type_layout_id,
            layout_id
        );
    }

    #[test]
    fn missing_layout_metadata_rejects_recovery() {
        let publication = encoded_object_publication_for_recovery_test(
            41,
            2,
            701,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );

        let recovered =
            recover_file_backed_object_without_layout_metadata_for_test(&publication).unwrap();
        assert_eq!(recovered.root_object_ids, Vec::<u64>::new());
        assert_eq!(recovered.type_layouts.iter().count(), 0);
        let object_winners = recovered.committed_object_winners().unwrap();
        assert_eq!(object_winners.len(), 1);
        assert_eq!(object_winners[0].object_id, 41);
        assert_eq!(object_winners[0].type_layout_id, 701);

        let mut rebuilt = ObjectTable::default();
        let err = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered.type_layouts,
                &object_winners,
                &[41],
            )
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("unknown persistent type layout id: 701"),
            "{err:?}"
        );
    }

    #[test]
    fn volatile_object_table_metadata_is_rebuilt_by_object_id_after_recovery() {
        let ((target, owner), recovered) =
            recover_file_backed_object_layout_case_for_test(205, |objects, state| {
                let target = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x908,
                        205,
                        8,
                        vec![ObjectValue::I32(5)],
                    )?;
                let owner = objects
                    .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                        0x909,
                        205,
                        9,
                        vec![ObjectValue::I32(6), ObjectValue::Ref(None)],
                    )?;
                state.acquire_object_write(objects, target)?;
                state.stage_struct_field(objects, target, 0, ObjectValue::I32(15))?;
                state.acquire_object_write(objects, owner)?;
                state.stage_struct_field(objects, owner, 0, ObjectValue::I32(16))?;
                state.stage_struct_field(objects, owner, 1, ObjectValue::Ref(Some(target)))?;
                Ok((target, owner))
            })
            .unwrap();

        assert!(recovered.rebuilt.live_bridge_gc_refs_to_objects.is_empty());
        assert!(recovered.rebuilt.object_to_live_bridge_gc_ref.is_empty());
        assert_eq!(recovered.rebuilt.live_count(), 2);
        assert_eq!(
            recovered.rebuilt.payload(target).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(15)])
        );
        assert_eq!(
            recovered.rebuilt.payload(owner).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(16), ObjectValue::Ref(Some(target)),])
        );
        assert_eq!(
            recovered.rebuilt.trace_object_ids(owner).unwrap(),
            vec![target]
        );
    }

    #[test]
    fn persistent_gc_file_backed_recovery_rebuilds_only_reachable_object_table() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-recovery.bin");
        let root = ObjectId { object_index: 41 };
        let garbage = ObjectId { object_index: 42 };

        crate::runtime::vm::block_region::create_file_backed_region_image(&tx_log_path, 128)
            .unwrap();
        crate::runtime::vm::block_region::publish_committed_struct_object(
            &tx_log_path,
            1,
            root.object_index,
            1,
            12,
            &[1, 2],
        )
        .unwrap();
        crate::runtime::vm::block_region::publish_committed_struct_object(
            &tx_log_path,
            2,
            garbage.object_index,
            1,
            12,
            &[9, 8],
        )
        .unwrap();
        crate::runtime::vm::block_region::publish_committed_global_object_root(
            &tx_log_path,
            3,
            root.object_index,
        )
        .unwrap();

        let (recovered_region, object_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered_region.type_layouts,
                &object_winners,
                &recovered_region.root_object_ids,
            )
            .unwrap();

        assert_eq!(recovered_region.root_object_ids, vec![root.object_index]);
        assert_eq!(report.installed_winners, vec![root.object_index]);
        assert!(
            report
                .skipped_unreachable_winners
                .contains(&garbage.object_index)
        );
        assert_eq!(
            rebuilt.payload(root).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
        );
        assert!(rebuilt.payload(garbage).is_err());
        assert_eq!(rebuilt.live_count(), 1);
    }

    #[test]
    fn file_backed_persistent_gc_recovery_filters_root_replaced_object() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-root-replaced.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x812),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let object_a = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8120,
                812,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, object_a).unwrap();
        state
            .stage_struct_field(&objects, object_a, 0, ObjectValue::I32(11))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8120))
            .unwrap();
        commit_active_file_backed_publications_for_test(812, 812, &mut objects, &mut state)
            .unwrap();

        let second_tx = state.begin().unwrap();
        let object_b = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8121,
                813,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();
        state.acquire_object_write(&mut objects, object_b).unwrap();
        state
            .stage_struct_field(&objects, object_b, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8121))
            .unwrap();
        commit_active_file_backed_publications_for_test(
            u32::try_from(second_tx.as_raw()).unwrap(),
            u32::try_from(second_tx.as_raw()).unwrap(),
            &mut objects,
            &mut state,
        )
        .unwrap();
        drop(state);

        let (recovered, object_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        assert_eq!(recovered.root_object_ids, vec![object_b.object_index]);
        assert!(
            object_winners
                .iter()
                .any(|winner| winner.object_id == object_a.object_index)
        );
        assert!(
            object_winners
                .iter()
                .any(|winner| winner.object_id == object_b.object_index)
        );

        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered.type_layouts,
                &object_winners,
                &recovered.root_object_ids,
            )
            .unwrap();

        assert_eq!(report.mark.reachable, object_set([object_b]));
        assert_eq!(report.mark.unreachable_persistent, object_set([object_a]));
        assert!(rebuilt.payload(object_a).is_err());
        assert!(rebuilt.payload(object_b).is_ok());
    }

    #[test]
    fn file_backed_persistent_gc_retires_whole_dead_object_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-retire-whole-dead.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x813),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let object_a = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8130,
                813,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, object_a).unwrap();
        state
            .stage_struct_field(&objects, object_a, 0, ObjectValue::I32(11))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8130))
            .unwrap();
        commit_active_file_backed_publications_for_test(813, 813, &mut objects, &mut state)
            .unwrap();

        let second_tx = state.begin().unwrap();
        let object_b = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8131,
                814,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();
        state.acquire_object_write(&mut objects, object_b).unwrap();
        state
            .stage_struct_field(&objects, object_b, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8131))
            .unwrap();
        commit_active_file_backed_publications_for_test(
            u32::try_from(second_tx.as_raw()).unwrap(),
            u32::try_from(second_tx.as_raw()).unwrap(),
            &mut objects,
            &mut state,
        )
        .unwrap();
        drop(state);

        let (recovered, object_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let object_a_winner =
            recovered_object_winner_by_id_for_test(&object_winners, object_a).clone();
        let (retired_chunk_start, retired_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, &object_a_winner).unwrap();

        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered.type_layouts,
                &object_winners,
                &recovered.root_object_ids,
            )
            .unwrap();
        assert_eq!(report.mark.reachable, object_set([object_b]));
        assert_eq!(report.mark.unreachable_persistent, object_set([object_a]));

        let retired =
            retire_unreachable_object_chunks_for_test(&tx_log_path, &[object_a.object_index])
                .unwrap();
        assert_eq!(retired, vec![retired_chunk_start]);

        let object_c = ObjectId { object_index: 91 };
        crate::runtime::vm::block_region::publish_committed_struct_object(
            &tx_log_path,
            91,
            object_c.object_index,
            1,
            12,
            &[3, 4],
        )
        .unwrap();

        let (_, republished_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        assert!(
            republished_winners
                .iter()
                .all(|winner| winner.object_id != object_a.object_index)
        );
        let object_c_winner =
            recovered_object_winner_by_id_for_test(&republished_winners, object_c);
        let (reused_chunk_start, reused_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, object_c_winner).unwrap();

        assert_eq!(reused_chunk_start, retired_chunk_start);
        assert_eq!(reused_generation, retired_generation + 1);
    }

    #[test]
    fn file_backed_persistent_gc_keeps_mixed_live_dead_object_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-mixed-chunk.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x814),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let live = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8140,
                814,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let dead = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8141,
                814,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, live).unwrap();
        state
            .stage_struct_field(&objects, live, 0, ObjectValue::I32(11))
            .unwrap();
        state.acquire_object_write(&mut objects, dead).unwrap();
        state
            .stage_struct_field(&objects, dead, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8140))
            .unwrap();
        commit_active_file_backed_publications_for_test(814, 814, &mut objects, &mut state)
            .unwrap();
        drop(state);

        let (recovered, object_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_winner = recovered_object_winner_by_id_for_test(&object_winners, live);
        let dead_winner = recovered_object_winner_by_id_for_test(&object_winners, dead);
        let (live_chunk_start, _) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, live_winner).unwrap();
        let (dead_chunk_start, _) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, dead_winner).unwrap();
        assert_eq!(live_chunk_start, dead_chunk_start);

        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered.type_layouts,
                &object_winners,
                &recovered.root_object_ids,
            )
            .unwrap();
        assert_eq!(report.mark.reachable, object_set([live]));
        assert_eq!(report.mark.unreachable_persistent, object_set([dead]));

        let retired =
            retire_unreachable_object_chunks_for_test(&tx_log_path, &[dead.object_index]).unwrap();
        assert!(retired.is_empty());
    }

    #[test]
    fn file_backed_persistent_gc_evacuates_mixed_chunk_and_retires_old_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-evacuate-mixed.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x900),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let live = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9000,
                900,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let dead = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9001,
                900,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, live).unwrap();
        state
            .stage_struct_field(&objects, live, 0, ObjectValue::I32(11))
            .unwrap();
        state.acquire_object_write(&mut objects, dead).unwrap();
        state
            .stage_struct_field(&objects, dead, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x9000))
            .unwrap();
        commit_active_file_backed_publications_for_test(0x900, 0x900, &mut objects, &mut state)
            .unwrap();

        let (before_recovered, before_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_before = recovered_object_winner_by_id_for_test(&before_winners, live).clone();
        let dead_before = recovered_object_winner_by_id_for_test(&before_winners, dead).clone();
        let (old_chunk, old_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, &live_before).unwrap();
        assert_eq!(
            old_chunk,
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, &dead_before)
                .unwrap()
                .0
        );

        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &before_recovered.type_layouts,
                &before_winners,
                &before_recovered.root_object_ids,
            )
            .unwrap();
        assert_eq!(report.mark.reachable, object_set([live]));
        assert_eq!(report.mark.unreachable_persistent, object_set([dead]));

        let compact = state
            .compact_persistent_object_chunks_for_test(&mut objects, &report)
            .unwrap();
        assert_eq!(compact.copied_objects, vec![live]);
        assert_eq!(compact.retired_chunks, vec![old_chunk]);

        let (_, after_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_after = recovered_object_winner_by_id_for_test(&after_winners, live);
        assert!(
            after_winners
                .iter()
                .all(|winner| winner.object_id != dead.object_index)
        );
        assert!(live_after.version > live_before.version);
        let (new_chunk, new_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, live_after).unwrap();
        assert_ne!(new_chunk, old_chunk);
        assert_eq!(new_generation, old_generation);

        let object_c = ObjectId { object_index: 901 };
        crate::runtime::vm::block_region::publish_committed_struct_object(
            &tx_log_path,
            901,
            object_c.object_index,
            1,
            12,
            &[3, 4],
        )
        .unwrap();
        let (_, reused_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let object_c_winner = recovered_object_winner_by_id_for_test(&reused_winners, object_c);
        let (reused_chunk, reused_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, object_c_winner).unwrap();
        assert_eq!(reused_chunk, old_chunk);
        assert_eq!(reused_generation, old_generation + 1);
    }

    #[test]
    fn persistent_gc_compaction_crash_before_lp_keeps_old_winner() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-crash-before-lp.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x920),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let live = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9200,
                920,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let dead = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9201,
                920,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, live).unwrap();
        state
            .stage_struct_field(&objects, live, 0, ObjectValue::I32(11))
            .unwrap();
        state.acquire_object_write(&mut objects, dead).unwrap();
        state
            .stage_struct_field(&objects, dead, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x9200))
            .unwrap();
        commit_active_file_backed_publications_for_test(0x920, 0x920, &mut objects, &mut state)
            .unwrap();

        let (_, before_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_before = recovered_object_winner_by_id_for_test(&before_winners, live).clone();
        let (old_chunk, _) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, &live_before).unwrap();

        let publication = objects
            .persistent_gc_copy_publication_for_test(live)
            .unwrap();
        let marker = state
            .publish_object_publications_before_commit(0x921, 0x921, &objects, &[publication])
            .unwrap()
            .unwrap();
        let _ = marker;
        drop(state);

        let (_, after_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_after = recovered_object_winner_by_id_for_test(&after_winners, live);
        let (after_chunk, _) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, live_after).unwrap();
        assert_eq!(live_after.version, live_before.version);
        assert_eq!(after_chunk, old_chunk);
    }

    #[test]
    fn persistent_gc_compaction_crash_after_lp_before_retirement_uses_copy() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("persistent-gc-crash-after-lp.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(0x930),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let live = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9300,
                930,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let dead = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x9301,
                930,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();

        state.acquire_object_write(&mut objects, live).unwrap();
        state
            .stage_struct_field(&objects, live, 0, ObjectValue::I32(11))
            .unwrap();
        state.acquire_object_write(&mut objects, dead).unwrap();
        state
            .stage_struct_field(&objects, dead, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x9300))
            .unwrap();
        commit_active_file_backed_publications_for_test(0x930, 0x930, &mut objects, &mut state)
            .unwrap();

        let (_, before_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_before = recovered_object_winner_by_id_for_test(&before_winners, live).clone();
        let (old_chunk, old_generation) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, &live_before).unwrap();

        let publication = objects
            .persistent_gc_copy_publication_for_test(live)
            .unwrap();
        let marker = state
            .publish_object_publications_before_commit(0x931, 0x931, &objects, &[publication])
            .unwrap()
            .unwrap();
        state.publish_commit_lp(0x931, 0x931, marker).unwrap();
        drop(state);

        let (_, after_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let live_after = recovered_object_winner_by_id_for_test(&after_winners, live);
        let (new_chunk, _) =
            file_backed_recovered_chunk_meta_for_test(&tx_log_path, live_after).unwrap();
        let region = crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(
            &tx_log_path,
        )
        .unwrap();

        assert!(live_after.version > live_before.version);
        assert_ne!(new_chunk, old_chunk);
        assert_eq!(region.block_generation(old_chunk).unwrap(), old_generation);
    }

    #[test]
    fn pre_gc_object_recovery_closure_file_backed_root_replacement_and_table_roots() {
        let dir = tempfile::tempdir().unwrap();
        let tx_log_path = dir.path().join("pre-gc-root-closure.bin");
        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(811),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();

        let old_global_root = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8110,
                811,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let new_global_root = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8111,
                811,
                2,
                vec![ObjectValue::I32(2)],
            )
            .unwrap();
        let table_root = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0x8112,
                811,
                3,
                vec![ObjectValue::I32(3)],
            )
            .unwrap();

        state
            .acquire_object_write(&mut objects, old_global_root)
            .unwrap();
        state
            .stage_struct_field(&objects, old_global_root, 0, ObjectValue::I32(11))
            .unwrap();
        state
            .acquire_object_write(&mut objects, new_global_root)
            .unwrap();
        state
            .stage_struct_field(&objects, new_global_root, 0, ObjectValue::I32(22))
            .unwrap();
        state
            .acquire_object_write(&mut objects, table_root)
            .unwrap();
        state
            .stage_struct_field(&objects, table_root, 0, ObjectValue::I32(33))
            .unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8110))
            .unwrap();
        state
            .stage_table_element_owned(None, 4, 7, TableElementSnapshot::GcRef(0x8112))
            .unwrap();
        commit_active_file_backed_publications_for_test(811, 811, &mut objects, &mut state)
            .unwrap();

        let second_tx = state.begin().unwrap();
        state
            .stage_global(0, GlobalSnapshot::GcRef(0x8111))
            .unwrap();
        commit_active_file_backed_publications_for_test(
            u32::try_from(second_tx.as_raw()).unwrap(),
            u32::try_from(second_tx.as_raw()).unwrap(),
            &mut objects,
            &mut state,
        )
        .unwrap();
        drop(state);

        let (recovered_region, object_winners) =
            recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
        let recovered_roots = recovered_region
            .root_object_ids
            .iter()
            .copied()
            .map(|object_index| ObjectId { object_index })
            .collect::<BTreeSet<_>>();
        assert_eq!(recovered_roots, object_set([new_global_root, table_root]));

        let mut rebuilt = ObjectTable::default();
        let report = rebuilt
            .rebuild_reachable_from_recovery_for_test(
                &recovered_region.type_layouts,
                &object_winners,
                &recovered_region.root_object_ids,
            )
            .unwrap();

        assert_eq!(
            report.mark.reachable,
            object_set([new_global_root, table_root])
        );
        assert!(
            report
                .skipped_unreachable_winners
                .contains(&old_global_root.object_index)
        );
        assert_eq!(
            rebuilt.payload(new_global_root).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(22)])
        );
        assert_eq!(
            rebuilt.payload(table_root).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(33)])
        );
        assert!(rebuilt.payload(old_global_root).is_err());
        assert_eq!(rebuilt.live_count(), 2);
    }
}

mod file_backed_mixed_tmemory_object_recovery {
    use super::*;

    #[test]
    fn pre_gc_object_recovery_closure_mixed_object_linear_memory_transaction_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("tmemory.bin");
        let tx_log_path = dir.path().join("tx-log.bin");
        let mut tmemory = crate::runtime::vm::TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        let old_granule = vec![0x11; TMEMORY_GRANULE_SIZE];
        let new_granule = vec![0x22; TMEMORY_GRANULE_SIZE];
        tmemory
            .commit_staged_tmemory_granule(0, &old_granule)
            .unwrap();

        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(301),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0xA01,
                301,
                1,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();
        let layout_id = objects.live_slot(object).unwrap().type_layout_id;

        let undo = tmemory
            .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
            .unwrap();
        let tmemory_marker = state
            .publish_tmemory_undo_before_in_place_write(301, 301, &undo)
            .unwrap();
        tmemory
            .commit_staged_tmemory_granule(0, &new_granule)
            .unwrap();

        state.acquire_object_write(&mut objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );
        let object_marker = state
            .publish_object_publications_before_commit(301, 301, &objects, &publications)
            .unwrap();
        state
            .publish_commit_lp(301, 301, object_marker.unwrap_or(tmemory_marker))
            .unwrap();
        drop(state);

        let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
        tmemory
            .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                recovered.tmemory_undo_rollbacks,
            )
            .unwrap();
        assert_eq!(
            tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
            new_granule
        );
        let tmemory_file = std::fs::read(&tmemory_path).unwrap();
        assert_eq!(
            &tmemory_file[..TMEMORY_GRANULE_SIZE],
            new_granule.as_slice()
        );

        let rebuilt = recover_file_backed_objects_from_path_for_test(&tx_log_path).unwrap();
        assert_eq!(rebuilt.object_winners.len(), 1);
        assert_eq!(rebuilt.object_winners[0].object_id, object.object_index);
        assert_eq!(rebuilt.object_winners[0].type_layout_id, layout_id);
        assert_eq!(
            rebuilt.rebuilt.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(9)])
        );
        assert_eq!(
            rebuilt.rebuilt.live_slot(object).unwrap().type_layout_id,
            layout_id
        );
        assert_eq!(
            rebuilt
                .rebuilt
                .pending_publication_for_test(object)
                .unwrap()
                .version,
            2
        );
    }

    #[test]
    fn loose_end_rolls_back_tmemory_and_drops_object_publication() {
        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("tmemory.bin");
        let tx_log_path = dir.path().join("tx-log.bin");
        let mut tmemory = crate::runtime::vm::TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        let old_granule = vec![0x33; TMEMORY_GRANULE_SIZE];
        let new_granule = vec![0x44; TMEMORY_GRANULE_SIZE];
        tmemory
            .commit_staged_tmemory_granule(0, &old_granule)
            .unwrap();

        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(302),
            durable_log,
        );
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
                0xA02,
                302,
                2,
                vec![ObjectValue::I32(1)],
            )
            .unwrap();

        let undo = tmemory
            .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
            .unwrap();
        state
            .publish_tmemory_undo_before_in_place_write(302, 302, &undo)
            .unwrap();
        tmemory
            .commit_staged_tmemory_granule(0, &new_granule)
            .unwrap();

        state.acquire_object_write(&mut objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );
        state
            .publish_object_publications_before_commit(302, 302, &objects, &publications)
            .unwrap();
        drop(state);

        let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        assert!(recovered.object_winners.is_empty());
        tmemory
            .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                recovered.tmemory_undo_rollbacks,
            )
            .unwrap();
        assert_eq!(
            tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
            old_granule
        );
        let tmemory_file = std::fs::read(&tmemory_path).unwrap();
        assert_eq!(
            &tmemory_file[..TMEMORY_GRANULE_SIZE],
            old_granule.as_slice()
        );

        let object_winners = crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
                &tx_log_path,
            )
            .unwrap();
        assert!(object_winners.is_empty());
    }
}

#[test]
fn transaction_state_does_not_duplicate_persisted_layout_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx-log.bin");
    let durable_log = TxDurableLog::create_file_backed(&path, 32).unwrap();
    let mut state =
        TransactionState::new_for_test_with_durable_log(TransactionId::from_raw(20), durable_log);
    let mut objects = ObjectTable::default();
    let layout = type_layout::PersistentTypeLayout::Struct {
        id: type_layout::TypeLayoutId::new(102).unwrap(),
        fingerprint: 0x0102_0000_0000_0002,
        body_size: 16,
        fields: vec![type_layout::StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: 8,
            kind: type_layout::TraceSlotKind::Scalar,
        }],
    };
    objects.register_type_layout(layout.clone()).unwrap();

    for (object_id, version, value) in [(41, 7, 9), (42, 8, 10)] {
        let publication = persist::PendingPublication::persistent_object(
            crate::runtime::vm::PackedGranuleDomain::TStruct,
            object_id,
            version,
            layout.id().get(),
            encode_object_record_for_test(
                object_id,
                version,
                layout.id().get(),
                &ObjectPayload::Struct(vec![ObjectValue::I32(value)]),
            )
            .unwrap(),
        )
        .unwrap();

        state
            .publish_object_publications_before_commit(20, 20, &objects, &[publication])
            .unwrap();
    }
    drop(state);

    let region =
        crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(&path)
            .unwrap();
    let registry = region.load_type_layout_metadata().unwrap();
    let layouts = registry.iter().cloned().collect::<Vec<_>>();

    assert_eq!(layouts, vec![layout]);
}

mod model_mixed_participants {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    struct ModelWorld {
        tmemory_granules: BTreeMap<u32, u8>,
        object_fields: BTreeMap<u64, i32>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ModelTMemoryWrite {
        granule: u32,
        old_byte: u8,
        new_byte: u8,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ModelObjectWrite {
        object_id: u64,
        old_value: i32,
        new_value: i32,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct MixedModelCase {
        tmemory_writes: Vec<ModelTMemoryWrite>,
        object_writes: Vec<ModelObjectWrite>,
        committed: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ActualMixedWorld {
        world: ModelWorld,
        object_winner_count: usize,
    }

    impl MixedModelCase {
        fn expected_world(&self) -> ModelWorld {
            let tmemory_granules = self
                .tmemory_writes
                .iter()
                .map(|write| {
                    (
                        write.granule,
                        if self.committed {
                            write.new_byte
                        } else {
                            write.old_byte
                        },
                    )
                })
                .collect();
            let object_fields = if self.committed {
                self.object_writes
                    .iter()
                    .map(|write| (write.object_id, write.new_value))
                    .collect()
            } else {
                BTreeMap::new()
            };
            ModelWorld {
                tmemory_granules,
                object_fields,
            }
        }
    }

    fn repeated_granule(byte: u8) -> Vec<u8> {
        vec![byte; TMEMORY_GRANULE_SIZE]
    }

    fn file_backed_granule_byte(bytes: &[u8], granule: u32) -> Result<u8> {
        let granule = usize::try_from(granule)
            .context("tmemory model granule index does not fit host usize")?;
        let start = granule
            .checked_mul(TMEMORY_GRANULE_SIZE)
            .context("tmemory model granule start overflow")?;
        let end = start
            .checked_add(TMEMORY_GRANULE_SIZE)
            .context("tmemory model granule end overflow")?;
        ensure!(
            end <= bytes.len(),
            "tmemory model granule range is out of bounds"
        );
        let first = *bytes[start..end]
            .first()
            .context("tmemory model granule is unexpectedly empty")?;
        ensure!(
            bytes[start..end].iter().all(|byte| *byte == first),
            "tmemory model expected one repeated byte per granule"
        );
        Ok(first)
    }

    fn model_case_strategy() -> impl Strategy<Value = MixedModelCase> {
        (
            prop::collection::vec(any::<bool>(), 3)
                .prop_filter("must write at least one tmemory granule", |flags| {
                    flags.iter().any(|flag| *flag)
                }),
            prop::collection::vec(
                (any::<u8>(), any::<u8>())
                    .prop_filter("old and new tmemory bytes must differ", |(old, new)| {
                        old != new
                    }),
                3,
            ),
            prop::collection::vec(any::<bool>(), 2),
            prop::collection::vec((0i32..=9, 10i32..=19), 2),
            any::<bool>(),
        )
            .prop_map(
                |(tmemory_flags, tmemory_bytes, object_flags, object_values, committed)| {
                    let tmemory_writes = tmemory_flags
                        .into_iter()
                        .zip(tmemory_bytes)
                        .enumerate()
                        .filter_map(|(granule, (selected, (old_byte, new_byte)))| {
                            selected.then_some(ModelTMemoryWrite {
                                granule: u32::try_from(granule).unwrap(),
                                old_byte,
                                new_byte,
                            })
                        })
                        .collect();
                    let object_writes = object_flags
                        .into_iter()
                        .zip(object_values)
                        .enumerate()
                        .filter_map(|(object_id, (selected, (old_value, new_value)))| {
                            selected.then_some(ModelObjectWrite {
                                object_id: u64::try_from(object_id).unwrap(),
                                old_value,
                                new_value,
                            })
                        })
                        .collect();
                    MixedModelCase {
                        tmemory_writes,
                        object_writes,
                        committed,
                    }
                },
            )
    }

    fn run_mixed_model_case(case: &MixedModelCase) -> Result<ActualMixedWorld> {
        let outcome = (|| -> Result<ActualMixedWorld> {
            let dir = tempfile::tempdir()?;
            let tmemory_path = dir.path().join("tmemory.bin");
            let tx_log_path = dir.path().join("tx-log.bin");
            let mut tmemory = crate::runtime::vm::TMemory::new(
                TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
                1,
                Some(1),
            )?;

            for write in &case.tmemory_writes {
                tmemory.commit_staged_tmemory_granule(
                    u64::from(write.granule),
                    &repeated_granule(write.old_byte),
                )?;
            }

            let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64)?;
            let mut state = TransactionState::new_for_test_with_durable_log(
                TransactionId::from_raw(71),
                durable_log,
            );
            let mut objects = ObjectTable::default();
            let mut object_handles = Vec::new();
            for object_id in 0..2u64 {
                let object = objects.allocate_persistent_struct_for_gc_ref(
                    u32::try_from(0x200 + object_id)
                        .context("object model gc ref does not fit u32")?,
                    vec![ObjectValue::I32(0)],
                )?;
                assert_eq!(object.object_index, object_id);
                object_handles.push(object);
            }
            for write in &case.object_writes {
                let handle = object_handles
                    .get(
                        usize::try_from(write.object_id)
                            .context("object model id does not fit host usize")?,
                    )
                    .copied()
                    .context("object model id is outside the prepared object slots")?;
                objects.update_payload(
                    handle,
                    ObjectPayload::Struct(vec![ObjectValue::I32(write.old_value)]),
                )?;
            }

            let mut final_marker = None;
            for write in &case.tmemory_writes {
                let new_granule = repeated_granule(write.new_byte);
                let marker = state.publish_tmemory_undo_before_in_place_write(
                    71,
                    71,
                    &tmemory.prepare_tmemory_undo_record(
                        Some(7),
                        0,
                        u64::from(write.granule),
                        &new_granule,
                    )?,
                )?;
                tmemory.commit_staged_tmemory_granule(u64::from(write.granule), &new_granule)?;
                final_marker = Some(marker);
            }

            for write in &case.object_writes {
                let handle = object_handles
                    .get(
                        usize::try_from(write.object_id)
                            .context("object model id does not fit host usize")?,
                    )
                    .copied()
                    .context("object model id is outside the prepared object slots")?;
                state.acquire_object_write(&mut objects, handle)?;
                state.stage_struct_field(&objects, handle, 0, ObjectValue::I32(write.new_value))?;
            }

            let mut publications = Vec::new();
            state.commit_object_payloads_into(&mut objects, &mut publications)?;
            if let Some(marker) =
                state.publish_object_publications_before_commit(71, 71, &objects, &publications)?
            {
                final_marker = Some(marker);
            }
            if case.committed {
                state.publish_commit_lp(
                    71,
                    71,
                    final_marker.context(
                        "mixed model case must publish at least one tmemory or object record",
                    )?,
                )?;
            }
            drop(state);

            let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path)?;
            tmemory.apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                recovered.tmemory_undo_rollbacks,
            )?;
            let tmemory_file = std::fs::read(&tmemory_path)?;
            let mut world = ModelWorld::default();
            for write in &case.tmemory_writes {
                world.tmemory_granules.insert(
                    write.granule,
                    file_backed_granule_byte(&tmemory_file, write.granule)?,
                );
            }

            let recovered_region =
                crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(
                    &tx_log_path,
                )?;
            let object_winners = recovered_region.committed_object_winners()?;
            let object_winner_count = object_winners.len();
            let mut recovered_objects = ObjectTable::default();
            recovered_objects
                .rebuild_from_recovery_for_test(&recovered_region.type_layouts, &object_winners)?;
            for winner in &object_winners {
                let payload = recovered_objects.payload(ObjectId {
                    object_index: winner.object_id,
                })?;
                let ObjectPayload::Struct(fields) = payload else {
                    bail!("mixed model expected recovered object winner to be a struct");
                };
                let field = fields
                    .first()
                    .context("mixed model expected recovered object to have one field")?;
                let ObjectValue::I32(value) = field else {
                    bail!("mixed model expected recovered object field to be i32");
                };
                world.object_fields.insert(winner.object_id, *value);
            }

            Ok(ActualMixedWorld {
                world,
                object_winner_count,
            })
        })();
        clear_current_thread_transaction_for_test();
        outcome
    }

    #[test]
    fn model_mixed_file_backed_commit_matches_reference_world() {
        let case = MixedModelCase {
            tmemory_writes: vec![ModelTMemoryWrite {
                granule: 0,
                old_byte: 0x11,
                new_byte: 0x22,
            }],
            object_writes: vec![ModelObjectWrite {
                object_id: 0,
                old_value: 1,
                new_value: 9,
            }],
            committed: true,
        };

        let actual = run_mixed_model_case(&case).unwrap();

        assert_eq!(actual.world, case.expected_world());
        assert_eq!(actual.world.tmemory_granules.get(&0), Some(&0x22));
        assert_eq!(actual.world.object_fields.get(&0), Some(&9));
        assert_eq!(actual.object_winner_count, 1);
    }

    #[test]
    fn model_mixed_file_backed_loose_end_matches_reference_world() {
        let case = MixedModelCase {
            tmemory_writes: vec![ModelTMemoryWrite {
                granule: 0,
                old_byte: 0x33,
                new_byte: 0x44,
            }],
            object_writes: vec![ModelObjectWrite {
                object_id: 0,
                old_value: 1,
                new_value: 9,
            }],
            committed: false,
        };

        let actual = run_mixed_model_case(&case).unwrap();

        assert_eq!(actual.world, case.expected_world());
        assert_eq!(actual.world.tmemory_granules.get(&0), Some(&0x33));
        assert!(actual.world.object_fields.is_empty());
        assert_eq!(actual.object_winner_count, 0);
    }

    #[test]
    fn model_mixed_file_backed_generated_histories_match_reference_world() {
        let mut runner = TestRunner::new(Config {
            cases: 48,
            failure_persistence: None,
            max_shrink_iters: 256,
            rng_seed: RngSeed::Fixed(0x5eed_0003),
            ..Config::default()
        });

        runner
            .run(&model_case_strategy(), |case| {
                let actual = run_mixed_model_case(&case)
                    .map_err(|error| TestCaseError::fail(error.to_string()))?;
                prop_assert_eq!(actual.world, case.expected_world());
                if case.committed {
                    prop_assert_eq!(actual.object_winner_count, case.object_writes.len());
                } else {
                    prop_assert_eq!(actual.object_winner_count, 0);
                }
                Ok(())
            })
            .unwrap();
    }
}

mod transaction_active {
    use super::*;
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex};

    fn assert_requires_active<T: Debug>(result: Result<T>) {
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transaction operation requires an active transaction"),
            "{error:?}"
        );
    }

    fn observed_transaction_ids(observed: &Arc<Mutex<Vec<Option<u64>>>>) -> Vec<Option<u64>> {
        observed.lock().unwrap().clone()
    }

    fn assert_active_transaction_id(id: Option<u64>) {
        let Some(id) = id else {
            panic!("expected an active transaction id to be observed");
        };
        assert!(id > 0, "expected a nonzero active transaction id, got {id}");
    }

    fn observing_import(
        store: &mut crate::Store<()>,
        observed: Arc<Mutex<Vec<Option<u64>>>>,
    ) -> crate::Func {
        crate::Func::wrap(store, move || {
            observed
                .lock()
                .unwrap()
                .push(current_thread_transaction_for_test().map(TransactionId::as_raw));
        })
    }

    #[test]
    fn direct_transactional_operations_do_not_succeed_without_active_transaction() {
        clear_current_thread_transaction_for_test();
        let mut state = TransactionState::default();
        let backing = vec![0; TMEMORY_GRANULE_SIZE];
        let mut objects = ObjectTable::default();
        let struct_object = objects
            .allocate_persistent_struct_for_gc_ref(0x701, vec![ObjectValue::I32(7)])
            .unwrap();
        let array_object = objects
            .allocate_persistent_array_for_gc_ref(0x702, vec![ObjectValue::I32(9)])
            .unwrap();

        assert_requires_active(state.acquire_memory_granule_read(0, 0));
        assert_requires_active(state.acquire_memory_granule_write(
            0,
            0,
            vec![0xaa; TMEMORY_GRANULE_SIZE],
        ));
        assert_requires_active(state.acquire_memory_size_read_owned(None, 0));
        assert_requires_active(state.acquire_memory_size_write_owned(None, 0));
        assert_requires_active(state.read_memory_overlay(0, 0, 1, &backing));
        assert_requires_active(state.stage_memory_write(0, 0, &[0xaa], &backing));

        assert_requires_active(state.acquire_global_read_owned(None, 0));
        assert_requires_active(state.acquire_global_write_owned(None, 0));
        assert_requires_active(state.stage_global(0, GlobalSnapshot::I32(11)));

        assert_requires_active(state.acquire_table_granule_read_owned(None, 0, 0, 0));
        assert_requires_active(state.acquire_table_granule_write_owned(None, 0, 0, 0));
        assert_requires_active(state.acquire_table_granule_read_range_owned(None, 0, 0, 1, 0));
        assert_requires_active(state.acquire_table_granule_write_range_owned(None, 0, 0, 1, 0));
        assert_requires_active(state.acquire_table_size_read_owned(None, 0, 0));
        assert_requires_active(state.acquire_table_size_write_owned(None, 0, 0));

        assert_requires_active(state.acquire_object_read(&mut objects, struct_object));
        assert_requires_active(state.acquire_object_write(&mut objects, struct_object));
        assert!(
            state.read_struct_field(&objects, struct_object, 0).is_err(),
            "persistent object read without an active transaction should not succeed"
        );
        assert!(
            state
                .stage_struct_field(&objects, struct_object, 0, ObjectValue::I32(12))
                .is_err(),
            "persistent object write without an active transaction should not succeed"
        );
        assert!(
            state.read_array_element(&objects, array_object, 0).is_err(),
            "persistent array read without an active transaction should not succeed"
        );
        assert!(
            state
                .stage_array_element(&objects, array_object, 0, ObjectValue::I32(12))
                .is_err(),
            "persistent array write without an active transaction should not succeed"
        );
        assert_eq!(current_thread_transaction_for_test(), None);
    }

    #[test]
    fn exported_tfunc_starts_and_clears_transaction_state() {
        clear_current_thread_transaction_for_test();
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc (export "first")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 1)))
                  (tfunc (export "second")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 2)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observe = observing_import(&mut store, Arc::clone(&observed));
        let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
        let first = instance
            .get_typed_func::<(), ()>(&mut store, "first")
            .unwrap();
        let second = instance
            .get_typed_func::<(), ()>(&mut store, "second")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        assert_eq!(current_thread_transaction_for_test(), None);
        first.call(&mut store, ()).unwrap();
        assert_eq!(read.call(&mut store, ()).unwrap(), 1);
        assert_eq!(current_thread_transaction_for_test(), None);

        second.call(&mut store, ()).unwrap();
        assert_eq!(read.call(&mut store, ()).unwrap(), 2);
        assert_eq!(current_thread_transaction_for_test(), None);

        let observed = observed_transaction_ids(&observed);
        assert_eq!(observed.len(), 2);
        assert_active_transaction_id(observed[0]);
        assert_active_transaction_id(observed[1]);
        assert_ne!(observed[0], observed[1]);
    }

    #[test]
    fn trap_path_aborts_and_clears_transaction_state() {
        clear_current_thread_transaction_for_test();
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc (export "trap")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 42))
                    (unreachable))
                  (tfunc (export "write")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 7)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observe = observing_import(&mut store, Arc::clone(&observed));
        let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
        let trap = instance
            .get_typed_func::<(), ()>(&mut store, "trap")
            .unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        assert!(trap.call(&mut store, ()).is_err());
        assert_eq!(current_thread_transaction_for_test(), None);
        assert_eq!(read.call(&mut store, ()).unwrap(), 0);

        write.call(&mut store, ()).unwrap();
        assert_eq!(current_thread_transaction_for_test(), None);
        assert_eq!(read.call(&mut store, ()).unwrap(), 7);

        let observed = observed_transaction_ids(&observed);
        assert_eq!(observed.len(), 2);
        assert_active_transaction_id(observed[0]);
        assert_active_transaction_id(observed[1]);
        assert_ne!(observed[0], observed[1]);
    }

    #[test]
    fn nested_tcall_reuses_active_transaction_until_outer_return() {
        clear_current_thread_transaction_for_test();
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc $inner
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 1)))
                  (tfunc (export "outer")
                    (call $observe)
                    (tcall $inner)
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 2)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observe = observing_import(&mut store, Arc::clone(&observed));
        let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
        let outer = instance
            .get_typed_func::<(), ()>(&mut store, "outer")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        outer.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 2);
        assert_eq!(current_thread_transaction_for_test(), None);

        let observed = observed_transaction_ids(&observed);
        assert_eq!(observed.len(), 3);
        assert!(observed.iter().all(Option::is_some));
        assert_eq!(observed[0], observed[1]);
        assert_eq!(observed[1], observed[2]);
    }
}

mod model_lock_based {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

    const MODEL_LOCK_BASED_EXHAUSTIVE_ENV: &str = "WASMTIME_TRANSACTION_LOCK_MODEL_EXHAUSTIVE";
    const MODEL_LOCK_BASED_DEFAULT_MAX_EXHAUSTIVE_LEN: usize = 3;
    const MODEL_LOCK_BASED_DEFAULT_EXHAUSTIVE_SCHEDULES: usize = 8_421;
    const MODEL_LOCK_BASED_FULL_MAX_EXHAUSTIVE_LEN: usize = 5;
    const MODEL_LOCK_BASED_FULL_EXHAUSTIVE_SCHEDULES: usize = 3_368_421;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum LockModelOp {
        Read { tx: u64, granule: u8, version: u64 },
        Write { tx: u64, granule: u8, version: u64 },
        Abort { tx: u64 },
        Release { tx: u64 },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum LockModelErrorKind {
        ReadOwnedByOther,
        ReadVersionMismatch,
        WriteOwnedByOther,
        WriteVersionMismatch,
    }

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct LockModelStep {
        error_kind: Option<LockModelErrorKind>,
        released_owner: Option<u64>,
    }

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    struct LockModelState {
        owners: BTreeMap<u8, u64>,
        owner_sets: BTreeMap<u64, BTreeSet<u8>>,
        read_versions: BTreeMap<(u64, u8), u64>,
    }

    impl LockModelErrorKind {
        fn as_actual(self) -> LockBasedConflictKindForTest {
            match self {
                Self::ReadOwnedByOther => LockBasedConflictKindForTest::ReadOwnedByOther,
                Self::ReadVersionMismatch => LockBasedConflictKindForTest::ReadVersionMismatch,
                Self::WriteOwnedByOther => LockBasedConflictKindForTest::WriteOwnedByOther,
                Self::WriteVersionMismatch => LockBasedConflictKindForTest::WriteVersionMismatch,
            }
        }
    }

    impl LockModelState {
        fn apply(&mut self, op: LockModelOp) -> LockModelStep {
            match op {
                LockModelOp::Read {
                    tx,
                    granule,
                    version,
                } => self.record_read(tx, granule, version),
                LockModelOp::Write {
                    tx,
                    granule,
                    version,
                } => self.acquire_write(tx, granule, version),
                LockModelOp::Abort { tx } | LockModelOp::Release { tx } => {
                    self.release_transaction(tx);
                    LockModelStep::default()
                }
            }
        }

        fn record_read(&mut self, tx: u64, granule: u8, version: u64) -> LockModelStep {
            let mut step = LockModelStep::default();
            match self.resolve_writer_conflict(tx, granule, false) {
                Ok(released_owner) => step.released_owner = released_owner,
                Err(error_kind) => {
                    step.error_kind = Some(error_kind);
                    return step;
                }
            }

            match self.read_versions.entry((tx, granule)) {
                alloc::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(version);
                }
                alloc::collections::btree_map::Entry::Occupied(entry) => {
                    if *entry.get() != version {
                        step.error_kind = Some(LockModelErrorKind::ReadVersionMismatch);
                    }
                }
            }

            step
        }

        fn acquire_write(&mut self, tx: u64, granule: u8, version: u64) -> LockModelStep {
            let mut step = LockModelStep::default();
            match self.resolve_writer_conflict(tx, granule, true) {
                Ok(released_owner) => step.released_owner = released_owner,
                Err(error_kind) => {
                    step.error_kind = Some(error_kind);
                    return step;
                }
            }

            if self
                .read_versions
                .get(&(tx, granule))
                .is_some_and(|current| *current != version)
            {
                step.error_kind = Some(LockModelErrorKind::WriteVersionMismatch);
                return step;
            }

            if let Some(previous_owner) = self.owners.insert(granule, tx) {
                if previous_owner != tx {
                    self.owner_sets
                        .get_mut(&previous_owner)
                        .unwrap()
                        .remove(&granule);
                    if self
                        .owner_sets
                        .get(&previous_owner)
                        .is_some_and(BTreeSet::is_empty)
                    {
                        self.owner_sets.remove(&previous_owner);
                    }
                }
            }
            self.owner_sets.entry(tx).or_default().insert(granule);
            step
        }

        fn resolve_writer_conflict(
            &mut self,
            tx: u64,
            granule: u8,
            is_write: bool,
        ) -> core::result::Result<Option<u64>, LockModelErrorKind> {
            let Some(owner) = self.owners.get(&granule).copied() else {
                return Ok(None);
            };
            if owner == tx {
                return Ok(None);
            }
            if tx < owner {
                self.release_transaction(owner);
                return Ok(Some(owner));
            }
            Err(if is_write {
                LockModelErrorKind::WriteOwnedByOther
            } else {
                LockModelErrorKind::ReadOwnedByOther
            })
        }

        fn release_transaction(&mut self, tx: u64) {
            if let Some(granules) = self.owner_sets.remove(&tx) {
                for granule in granules {
                    self.owners.remove(&granule);
                }
            }
            self.read_versions.retain(|(reader, _), _| *reader != tx);
        }

        fn assert_internal_invariants(&self) -> Result<()> {
            let owner_count: usize = self.owner_sets.values().map(BTreeSet::len).sum();
            ensure!(
                owner_count == self.owners.len(),
                "owner-set size {owner_count} does not match owner map size {}",
                self.owners.len()
            );
            for (&granule, &owner) in &self.owners {
                ensure!(
                    self.owner_sets
                        .get(&owner)
                        .is_some_and(|granules| granules.contains(&granule)),
                    "owner map says tx {owner} owns granule {granule}, but owner set disagrees"
                );
            }
            for (&tx, granules) in &self.owner_sets {
                ensure!(
                    !granules.is_empty(),
                    "owner set for tx {tx} should not be empty"
                );
                for &granule in granules {
                    ensure!(
                        self.owners.get(&granule) == Some(&tx),
                        "owner set says tx {tx} owns granule {granule}, but owner map disagrees"
                    );
                }
            }
            Ok(())
        }
    }

    fn model_tx(tx: u64) -> TransactionId {
        TransactionId::from_raw(tx)
    }

    fn model_granule(granule: u8) -> GranuleId {
        GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: u64::from(granule),
        }
    }

    fn decode_model_granule(granule: GranuleId) -> Result<u8> {
        let GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index,
        } = granule
        else {
            bail!("unexpected lock model granule in snapshot: {granule:?}");
        };
        let granule =
            u8::try_from(granule_index).context("lock model granule index does not fit u8")?;
        ensure!(
            granule <= 1,
            "lock model granule index {granule} is outside the Wave 5 domain"
        );
        Ok(granule)
    }

    fn snapshot_lock_state(snapshot: &LockBasedSnapshotForTest) -> Result<LockModelState> {
        let mut state = LockModelState::default();
        for (&granule, &owner) in &snapshot.owners {
            let granule = decode_model_granule(granule)?;
            let owner = owner.as_raw();
            state.owners.insert(granule, owner);
            state.owner_sets.entry(owner).or_default().insert(granule);
        }
        for (&(reader, granule), &version) in &snapshot.read_versions {
            let reader = reader.as_raw();
            let granule = decode_model_granule(granule)?;
            state.read_versions.insert((reader, granule), version);
        }
        state.assert_internal_invariants()?;
        Ok(state)
    }

    fn apply_lock_model_op(
        locks: &mut LockBased,
        op: LockModelOp,
    ) -> Option<LockBasedConflictKindForTest> {
        match op {
            LockModelOp::Read {
                tx,
                granule,
                version,
            } => locks
                .record_read_result_for_test(model_tx(tx), model_granule(granule), version)
                .err()
                .map(|error| error),
            LockModelOp::Write {
                tx,
                granule,
                version,
            } => locks
                .acquire_write_result_for_test(model_tx(tx), model_granule(granule), version)
                .err()
                .map(|error| error),
            LockModelOp::Abort { tx } => {
                locks.abort_for_test(model_tx(tx));
                None
            }
            LockModelOp::Release { tx } => {
                locks.release_transaction(model_tx(tx));
                None
            }
        }
    }

    fn assert_lock_step(
        prefix: &[LockModelOp],
        op: LockModelOp,
        before: &LockModelState,
        after: &LockModelState,
        step: LockModelStep,
        actual_error: Option<LockBasedConflictKindForTest>,
    ) -> Result<()> {
        if let Some(error_kind) = step.error_kind {
            let actual_error =
                actual_error.context("real LockBased op succeeded when model expected error")?;
            ensure!(
                actual_error == error_kind.as_actual(),
                "schedule prefix {prefix:?} expected error {:?}, got {:?}",
                error_kind.as_actual(),
                actual_error
            );
        } else {
            ensure!(
                actual_error.is_none(),
                "schedule prefix {prefix:?} unexpectedly failed with {actual_error:?}"
            );
        }

        if let Some(released_owner) = step.released_owner {
            ensure!(
                !after.owner_sets.contains_key(&released_owner),
                "schedule prefix {prefix:?} should release tx {released_owner} ownership"
            );
            ensure!(
                after.owners.values().all(|owner| *owner != released_owner),
                "schedule prefix {prefix:?} still shows tx {released_owner} in owner map"
            );
            ensure!(
                after
                    .read_versions
                    .keys()
                    .all(|(reader, _)| *reader != released_owner),
                "schedule prefix {prefix:?} still shows tx {released_owner} in read set"
            );
        }

        match op {
            LockModelOp::Abort { tx } | LockModelOp::Release { tx } => {
                ensure!(
                    !after.owner_sets.contains_key(&tx),
                    "schedule prefix {prefix:?} should clear owner set for tx {tx}"
                );
                ensure!(
                    after.owners.values().all(|owner| *owner != tx),
                    "schedule prefix {prefix:?} should clear owner map entries for tx {tx}"
                );
                ensure!(
                    after.read_versions.keys().all(|(reader, _)| *reader != tx),
                    "schedule prefix {prefix:?} should clear read versions for tx {tx}"
                );
            }
            LockModelOp::Read { tx, granule, .. } => {
                if step.error_kind == Some(LockModelErrorKind::ReadOwnedByOther) {
                    ensure!(
                        after.owners.get(&granule) == before.owners.get(&granule),
                        "schedule prefix {prefix:?} should not change owner on failed read conflict"
                    );
                }
                if step.error_kind == Some(LockModelErrorKind::ReadVersionMismatch) {
                    ensure!(
                        before.read_versions.get(&(tx, granule))
                            == after.read_versions.get(&(tx, granule)),
                        "schedule prefix {prefix:?} should preserve the original read version on mismatch"
                    );
                }
            }
            LockModelOp::Write { granule, .. } => {
                if step.error_kind == Some(LockModelErrorKind::WriteOwnedByOther) {
                    ensure!(
                        after.owners.get(&granule) == before.owners.get(&granule),
                        "schedule prefix {prefix:?} should not steal ownership on failed write conflict"
                    );
                }
            }
        }

        Ok(())
    }

    fn run_lock_model_schedule(schedule: &[LockModelOp]) -> Result<LockModelState> {
        let mut locks = LockBased::default();
        let mut model = LockModelState::default();
        let mut prefix = Vec::with_capacity(schedule.len());

        let actual_initial = snapshot_lock_state(&locks.snapshot_for_test())?;
        ensure!(
            actual_initial == model,
            "initial lock state diverged before running any schedule"
        );

        for &op in schedule {
            prefix.push(op);
            let before = model.clone();
            let step = model.apply(op);
            let actual_error = apply_lock_model_op(&mut locks, op);
            let actual = snapshot_lock_state(&locks.snapshot_for_test())?;
            model.assert_internal_invariants()?;
            ensure!(
                actual == model,
                "schedule prefix {prefix:?} diverged\nexpected: {model:?}\nactual:   {actual:?}"
            );
            assert_lock_step(&prefix, op, &before, &model, step, actual_error)?;
        }

        Ok(model)
    }

    fn exhaustive_lock_model_alphabet() -> Vec<LockModelOp> {
        let mut ops = Vec::new();
        for tx in [1, 2] {
            for granule in [0, 1] {
                for version in [0, 1] {
                    ops.push(LockModelOp::Read {
                        tx,
                        granule,
                        version,
                    });
                    ops.push(LockModelOp::Write {
                        tx,
                        granule,
                        version,
                    });
                }
            }
            ops.push(LockModelOp::Abort { tx });
            ops.push(LockModelOp::Release { tx });
        }
        ops
    }

    fn run_exhaustive_lock_model_schedules(max_len: usize) -> Result<usize> {
        fn visit(
            prefix: &mut Vec<LockModelOp>,
            real: &LockBased,
            model: &LockModelState,
            alphabet: &[LockModelOp],
            remaining: usize,
        ) -> Result<usize> {
            let mut schedule_count = 1;
            if remaining == 0 {
                return Ok(schedule_count);
            }

            for &op in alphabet {
                prefix.push(op);
                let mut next_real = real.clone_for_test();
                let mut next_model = model.clone();
                let step = next_model.apply(op);
                let actual_error = apply_lock_model_op(&mut next_real, op);
                let actual = snapshot_lock_state(&next_real.snapshot_for_test())?;
                next_model.assert_internal_invariants()?;
                ensure!(
                    actual == next_model,
                    "schedule prefix {prefix:?} diverged\nexpected: {next_model:?}\nactual:   {actual:?}"
                );
                let before = model.clone();
                assert_lock_step(prefix, op, &before, &next_model, step, actual_error)?;
                schedule_count += visit(prefix, &next_real, &next_model, alphabet, remaining - 1)?;
                prefix.pop();
            }

            Ok(schedule_count)
        }

        let alphabet = exhaustive_lock_model_alphabet();
        let mut prefix = Vec::new();
        let real = LockBased::default();
        let model = LockModelState::default();
        visit(&mut prefix, &real, &model, &alphabet, max_len)
    }

    fn lock_model_op_strategy() -> impl Strategy<Value = LockModelOp> {
        prop_oneof![
            (1u64..=2, 0u8..=1, 0u64..=1).prop_map(|(tx, granule, version)| {
                LockModelOp::Read {
                    tx,
                    granule,
                    version,
                }
            }),
            (1u64..=2, 0u8..=1, 0u64..=1).prop_map(|(tx, granule, version)| {
                LockModelOp::Write {
                    tx,
                    granule,
                    version,
                }
            }),
            (1u64..=2).prop_map(|tx| LockModelOp::Abort { tx }),
            (1u64..=2).prop_map(|tx| LockModelOp::Release { tx }),
        ]
    }

    #[test]
    fn model_lock_based_deterministic_schedule_matches_reference_world() {
        let schedule = vec![
            LockModelOp::Write {
                tx: 2,
                granule: 0,
                version: 0,
            },
            LockModelOp::Read {
                tx: 1,
                granule: 0,
                version: 0,
            },
            LockModelOp::Write {
                tx: 1,
                granule: 0,
                version: 0,
            },
            LockModelOp::Write {
                tx: 1,
                granule: 1,
                version: 1,
            },
            LockModelOp::Release { tx: 1 },
        ];

        let outcome = run_lock_model_schedule(&schedule).unwrap();

        assert!(outcome.owners.is_empty());
        assert!(outcome.owner_sets.is_empty());
        assert!(outcome.read_versions.is_empty());
    }

    #[test]
    fn model_lock_based_exhaustive_small_schedules_match_reference_world() {
        // Default runs stay quick by covering the full alphabet through length 3.
        // Opt in to the full depth-5 enumeration when explicitly requested.
        let full_exhaustive =
            std::env::var(MODEL_LOCK_BASED_EXHAUSTIVE_ENV).is_ok_and(|value| value == "1");
        let (max_len, expected_schedule_count) = if full_exhaustive {
            (
                MODEL_LOCK_BASED_FULL_MAX_EXHAUSTIVE_LEN,
                MODEL_LOCK_BASED_FULL_EXHAUSTIVE_SCHEDULES,
            )
        } else {
            (
                MODEL_LOCK_BASED_DEFAULT_MAX_EXHAUSTIVE_LEN,
                MODEL_LOCK_BASED_DEFAULT_EXHAUSTIVE_SCHEDULES,
            )
        };

        let schedule_count = run_exhaustive_lock_model_schedules(max_len).unwrap();

        assert_eq!(schedule_count, expected_schedule_count);
    }

    #[test]
    fn model_lock_based_generated_schedules_match_reference_world() {
        // Exhaustive coverage already owns the tiny prefix space. These fixed-seed
        // profiles spend the same budget on longer churn histories instead.
        for (cases, seed, len_range) in [
            (64, 0x5eed_0005, 4..=12usize),
            (32, 0x5eed_0505, 24..=48usize),
        ] {
            let mut runner = TestRunner::new(Config {
                cases,
                failure_persistence: None,
                max_shrink_iters: 256,
                rng_seed: RngSeed::Fixed(seed),
                ..Config::default()
            });

            runner
                .run(
                    &prop::collection::vec(lock_model_op_strategy(), len_range),
                    |schedule| {
                        run_lock_model_schedule(&schedule)
                            .map_err(|error| TestCaseError::fail(error.to_string()))?;
                        Ok(())
                    },
                )
                .unwrap();
        }
    }
}

mod model_object_rebuild {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ModelObjectRecord {
        object_id: u64,
        version: u32,
        kind: u16,
        field: i32,
        committed: bool,
    }

    #[derive(Clone, Copy, Debug)]
    struct ModelObjectSeed {
        object_id: u64,
        field: i32,
        committed: bool,
    }

    #[derive(Debug)]
    struct RecoveredObjectHistory {
        rebuilt: ObjectTable,
        recovered_region: crate::runtime::vm::RecoveredRegion,
        object_winners: Vec<crate::runtime::vm::RecoveredObjectWinner>,
    }

    fn model_kind_strategy() -> impl Strategy<Value = u16> {
        prop_oneof![
            Just(ObjectKind::Struct as u16),
            Just(ObjectKind::Array as u16),
        ]
    }

    fn model_object_history_strategy() -> impl Strategy<Value = Vec<ModelObjectRecord>> {
        prop::collection::vec(model_kind_strategy(), 4).prop_flat_map(|kinds| {
            prop::collection::vec(
                (0u64..=3u64, -20i32..=20i32, any::<bool>()).prop_map(
                    |(object_id, field, committed)| ModelObjectSeed {
                        object_id,
                        field,
                        committed,
                    },
                ),
                1..=8usize,
            )
            .prop_map(move |seeds| {
                let mut versions = BTreeMap::<u64, u32>::new();
                seeds
                    .into_iter()
                    .map(|seed| {
                        let next_version = versions
                            .entry(seed.object_id)
                            .and_modify(|version| *version += 1)
                            .or_insert(1);
                        let kind_index = usize::try_from(seed.object_id).unwrap();
                        ModelObjectRecord {
                            object_id: seed.object_id,
                            version: *next_version,
                            kind: kinds[kind_index],
                            field: seed.field,
                            committed: seed.committed,
                        }
                    })
                    .collect()
            })
        })
    }

    fn model_payload(record: &ModelObjectRecord) -> Result<ObjectPayload> {
        Ok(match object_kind_from_u16(record.kind)? {
            ObjectKind::Struct => ObjectPayload::Struct(vec![ObjectValue::I32(record.field)]),
            ObjectKind::Array => ObjectPayload::Array(vec![ObjectValue::I32(record.field)]),
            kind => bail!("unsupported model object kind for rebuild test: {kind:?}"),
        })
    }

    fn model_domain(record: &ModelObjectRecord) -> Result<crate::runtime::vm::PackedGranuleDomain> {
        Ok(match object_kind_from_u16(record.kind)? {
            ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
            ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
            kind => bail!("unsupported model object kind for durable publication: {kind:?}"),
        })
    }

    fn model_type_layout_id(record: &ModelObjectRecord) -> Result<u32> {
        u32::try_from(record.object_id)
            .context("model object id does not fit u32 for type layout id")
            .map(|id| 100 + id)
    }

    fn model_type_layout(record: &ModelObjectRecord) -> Result<PersistentTypeLayout> {
        let id = type_layout::TypeLayoutId::new(model_type_layout_id(record)?)
            .context("model type layout id cannot be zero")?;
        Ok(match object_kind_from_u16(record.kind)? {
            ObjectKind::Struct => PersistentTypeLayout::Struct {
                id,
                fingerprint: 0x5354_5255_4354_1000 | u64::from(id.get()),
                body_size: 4,
                fields: vec![type_layout::StructTraceField {
                    field_index: 0,
                    field_offset: 0,
                    value_size: 4,
                    kind: type_layout::TraceSlotKind::Scalar,
                }],
            },
            ObjectKind::Array => PersistentTypeLayout::Array {
                id,
                fingerprint: 0x4152_5241_5900_1000 | u64::from(id.get()),
                element_size: 4,
                element_kind: type_layout::TraceSlotKind::Scalar,
            },
            kind => bail!("unsupported model object kind for type layout: {kind:?}"),
        })
    }

    fn model_record_bytes(record: &ModelObjectRecord) -> Result<Vec<u8>> {
        encode_object_record_for_test(
            record.object_id,
            record.version,
            model_type_layout_id(record)?,
            &model_payload(record)?,
        )
    }

    fn model_publication(record: &ModelObjectRecord) -> Result<persist::PendingPublication> {
        persist::PendingPublication::persistent_object(
            model_domain(record)?,
            record.object_id,
            record.version,
            model_type_layout_id(record)?,
            model_record_bytes(record)?,
        )
    }

    fn expected_latest_committed_by_object(
        history: &[ModelObjectRecord],
    ) -> BTreeMap<u64, ModelObjectRecord> {
        let mut latest = BTreeMap::<u64, ModelObjectRecord>::new();
        let mut committed_versions = BTreeSet::<(u64, u32)>::new();
        for &record in history {
            if !record.committed {
                continue;
            }
            assert!(
                committed_versions.insert((record.object_id, record.version)),
                "generated model history should not contain duplicate committed object/version pairs"
            );
            match latest.get(&record.object_id).copied() {
                Some(current) if current.version > record.version => {}
                Some(current) if current.version == record.version => unreachable!(),
                _ => {
                    latest.insert(record.object_id, record);
                }
            }
        }
        latest
    }

    fn rebuild_object_table_from_winners(
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
    ) -> Result<ObjectTable> {
        let mut objects = ObjectTable::default();
        objects.rebuild_from_recovery_for_test(recovered_type_layouts, winners)?;
        Ok(objects)
    }

    fn recover_object_history(history: &[ModelObjectRecord]) -> Result<RecoveredObjectHistory> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 64)?;

        for record in history {
            log.ensure_type_layout(&model_type_layout(record)?)?;
        }

        for (index, record) in history.iter().enumerate() {
            let stream_id = u32::try_from(
                index
                    .checked_add(1)
                    .context("model object history index overflow while assigning stream id")?,
            )
            .context("model object history stream id does not fit u32")?;
            let txid = stream_id
                .checked_add(400)
                .context("model object history txid overflow")?;
            let publication = model_publication(record)?;
            let marker = {
                let mut sink = log.stream_sink(stream_id);
                let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
                publisher.publish_object_publication_before_commit(&publication)?
            };
            if record.committed {
                let mut sink = log.stream_sink(stream_id);
                let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
                publisher.publish_commit_lp(marker)?;
            }
        }
        drop(log);

        let recovered_region =
            crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(
                &path,
            )?;
        let object_winners = recovered_region.committed_object_winners()?;
        let mut rebuilt = ObjectTable::default();
        rebuilt.rebuild_from_recovery_for_test(&recovered_region.type_layouts, &object_winners)?;

        Ok(RecoveredObjectHistory {
            rebuilt,
            recovered_region,
            object_winners,
        })
    }

    fn recover_rebuilt_object_history(history: &[ModelObjectRecord]) -> Result<ObjectTable> {
        Ok(recover_object_history(history)?.rebuilt)
    }

    fn expected_logical_id(record: &ModelObjectRecord) -> Result<u64> {
        crate::runtime::vm::pack_object_granule_id(model_domain(record)?, record.object_id)
    }

    fn assert_model_object_rebuild_matches_real_recovery(
        history: &[ModelObjectRecord],
    ) -> Result<()> {
        let expected = expected_latest_committed_by_object(history);
        let recovered = recover_object_history(history)?;
        let rebuilt = &recovered.rebuilt;
        let recovered_versions = recovered
            .recovered_region
            .object_winners
            .iter()
            .map(|winner| (winner.object_id, winner.version))
            .collect::<BTreeMap<_, _>>();

        ensure!(
            rebuilt.live_count() == expected.len(),
            "rebuilt object count mismatch\nexpected: {}\nactual:   {}",
            expected.len(),
            rebuilt.live_count()
        );

        for (&object_id, record) in &expected {
            let object = ObjectId {
                object_index: object_id,
            };
            ensure!(
                rebuilt.kind(object)? == object_kind_from_u16(record.kind)?,
                "rebuilt kind mismatch for object {object_id}"
            );
            ensure!(
                rebuilt.is_persistent(object)?,
                "rebuilt object {object_id} should stay persistent"
            );
            ensure!(
                rebuilt.payload(object)? == model_payload(record)?,
                "rebuilt payload mismatch for object {object_id}"
            );
            ensure!(
                rebuilt.pending_publication_for_test(object)?.logical_id
                    == expected_logical_id(record)?,
                "rebuilt logical id mismatch for object {object_id}"
            );
            ensure!(
                rebuilt.pending_publication_for_test(object)?.version == record.version,
                "rebuilt publication version mismatch for object {object_id}"
            );
            ensure!(
                rebuilt.pending_publication_for_test(object)?.type_layout_id
                    == model_type_layout_id(record)?,
                "rebuilt publication type layout mismatch for object {object_id}"
            );
            ensure!(
                rebuilt.trace_object_ids(object)?.is_empty(),
                "model payloads should not carry object references"
            );
            ensure!(
                recovered_versions.get(&object_id) == Some(&record.version),
                "recovered summary version mismatch for object {object_id}"
            );
        }

        for record in history {
            if expected.contains_key(&record.object_id) {
                continue;
            }
            ensure!(
                rebuilt
                    .kind(ObjectId {
                        object_index: record.object_id,
                    })
                    .is_err(),
                "loose-end object {} should not rebuild into a live slot",
                record.object_id
            );
        }

        ensure!(
            recovered.object_winners.len() == expected.len(),
            "recovered object winner count mismatch\nexpected: {}\nactual:   {}",
            expected.len(),
            recovered.object_winners.len()
        );

        Ok(())
    }

    fn corrupt_payload_header_winner() -> crate::runtime::vm::RecoveredObjectWinner {
        let payload = ObjectPayload::Array(vec![ObjectValue::I32(9)]);
        let mut record_bytes = encode_object_record_for_test(7, 3, 207, &payload).unwrap();
        record_bytes[20..22].copy_from_slice(&(ObjectKind::Struct as u16).to_le_bytes());

        recovered_object_winner_for_test(7, 3, ObjectKind::Struct as u16, 207, record_bytes)
    }

    #[test]
    fn model_object_rebuild_latest_committed_version_wins() {
        let rebuilt = recover_rebuilt_object_history(&[
            ModelObjectRecord {
                object_id: 41,
                version: 2,
                kind: ObjectKind::Struct as u16,
                field: 20,
                committed: true,
            },
            ModelObjectRecord {
                object_id: 41,
                version: 9,
                kind: ObjectKind::Struct as u16,
                field: 90,
                committed: true,
            },
            ModelObjectRecord {
                object_id: 41,
                version: 7,
                kind: ObjectKind::Struct as u16,
                field: 70,
                committed: true,
            },
        ])
        .unwrap();

        assert_eq!(rebuilt.live_count(), 1);
        assert!(
            rebuilt
                .is_persistent(ObjectId { object_index: 41 })
                .unwrap()
        );
        assert_eq!(
            rebuilt.payload(ObjectId { object_index: 41 }).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(90)])
        );
        assert_eq!(
            rebuilt
                .pending_publication_for_test(ObjectId { object_index: 41 })
                .unwrap()
                .version,
            9
        );
        assert_eq!(
            rebuilt
                .pending_publication_for_test(ObjectId { object_index: 41 })
                .unwrap()
                .type_layout_id,
            141
        );
    }

    #[test]
    fn model_object_rebuild_ignores_loose_end_publication() {
        let rebuilt = recover_rebuilt_object_history(&[
            ModelObjectRecord {
                object_id: 52,
                version: 1,
                kind: ObjectKind::Struct as u16,
                field: 10,
                committed: true,
            },
            ModelObjectRecord {
                object_id: 52,
                version: 3,
                kind: ObjectKind::Struct as u16,
                field: 30,
                committed: false,
            },
        ])
        .unwrap();

        assert_eq!(rebuilt.live_count(), 1);
        assert_eq!(
            rebuilt.payload(ObjectId { object_index: 52 }).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(10)])
        );
    }

    #[test]
    fn model_object_rebuild_preserves_recovered_object_identity() {
        let rebuilt = recover_rebuilt_object_history(&[
            ModelObjectRecord {
                object_id: 77,
                version: 4,
                kind: ObjectKind::Struct as u16,
                field: 17,
                committed: true,
            },
            ModelObjectRecord {
                object_id: 103,
                version: 6,
                kind: ObjectKind::Struct as u16,
                field: 88,
                committed: true,
            },
        ])
        .unwrap();

        let object = ObjectId { object_index: 103 };
        assert_eq!(rebuilt.kind(object).unwrap(), ObjectKind::Struct);
        assert_eq!(
            rebuilt.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(88)])
        );
        assert_eq!(
            rebuilt
                .pending_publication_for_test(object)
                .unwrap()
                .logical_id,
            crate::runtime::vm::pack_object_granule_id(
                crate::runtime::vm::PackedGranuleDomain::TStruct,
                103,
            )
            .unwrap()
        );
        assert_eq!(
            rebuilt
                .pending_publication_for_test(object)
                .unwrap()
                .type_layout_id,
            203
        );
    }

    #[test]
    fn model_object_rebuild_rejects_corrupt_payload_header_mismatch() {
        let mut recovered_type_layouts = TypeLayoutRegistry::default();
        recovered_type_layouts
            .insert(PersistentTypeLayout::Struct {
                id: type_layout::TypeLayoutId::new(207).unwrap(),
                fingerprint: 0x5354_5255_4354_0207,
                body_size: 4,
                fields: vec![type_layout::StructTraceField {
                    field_index: 0,
                    field_offset: 0,
                    value_size: 4,
                    kind: type_layout::TraceSlotKind::Scalar,
                }],
            })
            .unwrap();
        let err = rebuild_object_table_from_winners(
            &recovered_type_layouts,
            &[corrupt_payload_header_winner()],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("serialized struct payload length is not a multiple of object ABI size")
        );
    }

    #[test]
    fn model_object_rebuild_rejects_duplicate_committed_same_version() {
        let err = recover_object_history(&[
            ModelObjectRecord {
                object_id: 9,
                version: 4,
                kind: ObjectKind::Struct as u16,
                field: 11,
                committed: true,
            },
            ModelObjectRecord {
                object_id: 9,
                version: 4,
                kind: ObjectKind::Struct as u16,
                field: 22,
                committed: true,
            },
        ])
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("duplicate committed object version 4 for object id 9")
        );
    }

    #[test]
    fn model_object_rebuild_generated_histories_match_real_recovery_and_reference_model() {
        let mut runner = TestRunner::new(Config {
            cases: 64,
            failure_persistence: None,
            max_shrink_iters: 256,
            rng_seed: RngSeed::Fixed(0x5eed_0009),
            ..Config::default()
        });

        runner
            .run(&model_object_history_strategy(), |history| {
                assert_model_object_rebuild_matches_real_recovery(&history)
                    .map_err(|error| TestCaseError::fail(error.to_string()))
            })
            .unwrap();
    }
}

mod model_permissions {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PermissionModelOp {
        Begin,
        GrantRead,
        GrantWrite,
        Read,
        Write(i32),
        Downgrade,
        Abort,
        CommitRelease,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PermissionStepOutcome {
        Bool { ok: bool, value: Option<bool> },
        Read { ok: bool, value: Option<i32> },
        Write { ok: bool },
        Unit { ok: bool },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PermissionRuntimeSnapshot {
        active: bool,
        committed_value: i32,
        staged_value: Option<i32>,
        read: bool,
        write: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PermissionTraceStep {
        outcome: PermissionStepOutcome,
        snapshot: PermissionRuntimeSnapshot,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PermissionModelState {
        snapshot: PermissionRuntimeSnapshot,
    }

    impl Default for PermissionModelState {
        fn default() -> Self {
            Self {
                snapshot: PermissionRuntimeSnapshot {
                    active: true,
                    committed_value: 7,
                    staged_value: None,
                    read: false,
                    write: false,
                },
            }
        }
    }

    impl PermissionModelState {
        fn apply(&mut self, op: PermissionModelOp) -> PermissionStepOutcome {
            match op {
                PermissionModelOp::Begin => {
                    if self.snapshot.active {
                        return PermissionStepOutcome::Unit { ok: false };
                    }
                    self.snapshot.active = true;
                    self.snapshot.staged_value = None;
                    self.snapshot.read = false;
                    self.snapshot.write = false;
                    PermissionStepOutcome::Unit { ok: true }
                }
                PermissionModelOp::GrantRead => {
                    if !self.snapshot.active {
                        return PermissionStepOutcome::Bool {
                            ok: false,
                            value: None,
                        };
                    }
                    let granted = !self.snapshot.read;
                    self.snapshot.read = true;
                    PermissionStepOutcome::Bool {
                        ok: true,
                        value: Some(granted),
                    }
                }
                PermissionModelOp::GrantWrite => {
                    if !self.snapshot.active {
                        return PermissionStepOutcome::Bool {
                            ok: false,
                            value: None,
                        };
                    }
                    let granted = !self.snapshot.write;
                    self.snapshot.read = true;
                    self.snapshot.write = true;
                    PermissionStepOutcome::Bool {
                        ok: true,
                        value: Some(granted),
                    }
                }
                PermissionModelOp::Read => {
                    if !self.snapshot.active || !self.snapshot.read {
                        return PermissionStepOutcome::Read {
                            ok: false,
                            value: None,
                        };
                    }
                    PermissionStepOutcome::Read {
                        ok: true,
                        value: Some(
                            self.snapshot
                                .staged_value
                                .unwrap_or(self.snapshot.committed_value),
                        ),
                    }
                }
                PermissionModelOp::Write(value) => {
                    if !self.snapshot.active || !self.snapshot.write {
                        return PermissionStepOutcome::Write { ok: false };
                    }
                    self.snapshot.staged_value = Some(value);
                    PermissionStepOutcome::Write { ok: true }
                }
                PermissionModelOp::Downgrade => {
                    if !self.snapshot.active {
                        return PermissionStepOutcome::Bool {
                            ok: false,
                            value: None,
                        };
                    }
                    let removed = self.snapshot.write;
                    self.snapshot.write = false;
                    PermissionStepOutcome::Bool {
                        ok: true,
                        value: Some(removed),
                    }
                }
                PermissionModelOp::Abort => {
                    if !self.snapshot.active {
                        return PermissionStepOutcome::Unit { ok: false };
                    }
                    self.snapshot.active = false;
                    self.snapshot.staged_value = None;
                    self.snapshot.read = false;
                    self.snapshot.write = false;
                    PermissionStepOutcome::Unit { ok: true }
                }
                PermissionModelOp::CommitRelease => {
                    if !self.snapshot.active {
                        return PermissionStepOutcome::Unit { ok: false };
                    }
                    if let Some(value) = self.snapshot.staged_value {
                        self.snapshot.committed_value = value;
                    }
                    self.snapshot.active = false;
                    self.snapshot.staged_value = None;
                    self.snapshot.read = false;
                    self.snapshot.write = false;
                    PermissionStepOutcome::Unit { ok: true }
                }
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct GranulePermissionModelState {
        active: bool,
        read: bool,
        write: bool,
    }

    impl Default for GranulePermissionModelState {
        fn default() -> Self {
            Self {
                active: true,
                read: false,
                write: false,
            }
        }
    }

    fn permission_model_object() -> Result<(ObjectTable, ObjectId)> {
        let mut objects = ObjectTable::default();
        let object =
            objects.allocate_persistent_struct_for_gc_ref(0x611, vec![ObjectValue::I32(7)])?;
        Ok((objects, object))
    }

    fn payload_i32(payload: &ObjectPayload) -> Result<i32> {
        let ObjectPayload::Struct(fields) = payload else {
            bail!("permission model expected struct payload");
        };
        let field = fields
            .first()
            .context("permission model expected one struct field")?;
        let ObjectValue::I32(value) = field else {
            bail!("permission model expected i32 field");
        };
        Ok(*value)
    }

    fn downgrade_granule_write_for_test(
        state: &mut TransactionState,
        granule: GranuleId,
    ) -> Result<bool> {
        let Some(transaction) = state.active else {
            bail!("transaction operation requires an active transaction");
        };
        let removed = state.write_granules.remove(&granule);
        if removed {
            ensure!(
                state.read_granules.contains(&granule),
                "permission downgrade should preserve read permission for {granule:?}"
            );
            ensure!(
                state.locks.owners.remove(&granule) == Some(transaction),
                "permission downgrade expected tx {transaction:?} to own {granule:?}"
            );
        }
        Ok(removed)
    }

    fn apply_permission_runtime_op(
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        object: ObjectId,
        op: PermissionModelOp,
    ) -> Result<PermissionStepOutcome> {
        match op {
            PermissionModelOp::GrantRead => match state.acquire_object_read(objects, object) {
                Ok(granted) => Ok(PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(granted),
                }),
                Err(_) => Ok(PermissionStepOutcome::Bool {
                    ok: false,
                    value: None,
                }),
            },
            PermissionModelOp::Begin => match state.begin() {
                Ok(_) => Ok(PermissionStepOutcome::Unit { ok: true }),
                Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
            },
            PermissionModelOp::GrantWrite => match state.acquire_object_write(objects, object) {
                Ok(granted) => Ok(PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(granted),
                }),
                Err(_) => Ok(PermissionStepOutcome::Bool {
                    ok: false,
                    value: None,
                }),
            },
            PermissionModelOp::Read => match state.read_struct_field(objects, object, 0) {
                Ok(ObjectValue::I32(value)) => Ok(PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(value),
                }),
                Ok(other) => bail!("permission model expected i32 field, got {other:?}"),
                Err(_) => Ok(PermissionStepOutcome::Read {
                    ok: false,
                    value: None,
                }),
            },
            PermissionModelOp::Write(value) => {
                match state.stage_struct_field(objects, object, 0, ObjectValue::I32(value)) {
                    Ok(()) => Ok(PermissionStepOutcome::Write { ok: true }),
                    Err(_) => Ok(PermissionStepOutcome::Write { ok: false }),
                }
            }
            PermissionModelOp::Downgrade => {
                let granule = objects.granule_id(object)?;
                match downgrade_granule_write_for_test(state, granule) {
                    Ok(removed) => Ok(PermissionStepOutcome::Bool {
                        ok: true,
                        value: Some(removed),
                    }),
                    Err(_) => Ok(PermissionStepOutcome::Bool {
                        ok: false,
                        value: None,
                    }),
                }
            }
            PermissionModelOp::Abort => match state.abort() {
                Ok(()) => Ok(PermissionStepOutcome::Unit { ok: true }),
                Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
            },
            PermissionModelOp::CommitRelease => match state
                .commit_object_payloads(objects)
                .and_then(|_| state.complete_commit())
            {
                Ok(()) => Ok(PermissionStepOutcome::Unit { ok: true }),
                Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
            },
        }
    }

    fn permission_runtime_snapshot(
        state: &TransactionState,
        objects: &ObjectTable,
        object: ObjectId,
    ) -> Result<PermissionRuntimeSnapshot> {
        let granule = objects.granule_id(object)?;
        let committed_value = payload_i32(&objects.payload(object)?)?;
        let staged_value = state
            .staged_objects
            .get(&object)
            .map(payload_i32)
            .transpose()?;
        Ok(PermissionRuntimeSnapshot {
            active: state.active.is_some(),
            committed_value,
            staged_value,
            read: state.read_granules.contains(&granule),
            write: state.write_granules.contains(&granule),
        })
    }

    fn run_permission_semantic_schedule(
        schedule: &[PermissionModelOp],
    ) -> Result<Vec<PermissionTraceStep>> {
        let outcome = (|| -> Result<Vec<PermissionTraceStep>> {
            let (mut objects, object) = permission_model_object()?;
            let mut state = TransactionState::new_for_test(TransactionId::from_raw(91));
            let mut model = PermissionModelState::default();
            let mut prefix = Vec::with_capacity(schedule.len());
            let mut trace = Vec::with_capacity(schedule.len());

            let initial = permission_runtime_snapshot(&state, &objects, object)?;
            ensure!(
                initial == model.snapshot,
                "initial permission snapshot diverged\nexpected: {:?}\nactual:   {:?}",
                model.snapshot,
                initial
            );

            for &op in schedule {
                prefix.push(op);
                let expected_outcome = model.apply(op);
                let actual_outcome =
                    apply_permission_runtime_op(&mut state, &mut objects, object, op)?;
                ensure!(
                    actual_outcome == expected_outcome,
                    "permission prefix {prefix:?} produced the wrong outcome\nexpected: {:?}\nactual:   {:?}",
                    expected_outcome,
                    actual_outcome
                );
                let actual_snapshot = permission_runtime_snapshot(&state, &objects, object)?;
                ensure!(
                    actual_snapshot == model.snapshot,
                    "permission prefix {prefix:?} diverged\nexpected: {:?}\nactual:   {:?}",
                    model.snapshot,
                    actual_snapshot
                );
                trace.push(PermissionTraceStep {
                    outcome: actual_outcome,
                    snapshot: actual_snapshot,
                });
            }

            Ok(trace)
        })();
        clear_current_thread_transaction_for_test();
        outcome
    }

    fn granule_permission_snapshot(
        state: &TransactionState,
        granule: GranuleId,
    ) -> GranulePermissionModelState {
        GranulePermissionModelState {
            active: state.active.is_some(),
            read: state.read_granules.contains(&granule),
            write: state.write_granules.contains(&granule),
        }
    }

    fn assert_generic_granule_reacquires_after_abort(granule: GranuleId) {
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(101));
        assert!(state.acquire_granule_write(granule, 0).unwrap());
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: true,
                read: true,
                write: true,
            }
        );
        state.abort().unwrap();
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: false,
                read: false,
                write: false,
            }
        );
        state.begin().unwrap();
        assert!(state.acquire_granule_write(granule, 1).unwrap());
        state.abort().unwrap();
        clear_current_thread_transaction_for_test();
    }

    fn assert_generic_granule_reacquires_after_commit(granule: GranuleId) {
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(102));
        assert!(state.acquire_granule_read(granule, 0).unwrap());
        assert!(state.acquire_granule_write(granule, 0).unwrap());
        state.complete_commit().unwrap();
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: false,
                read: false,
                write: false,
            }
        );
        state.begin().unwrap();
        assert!(state.acquire_granule_read(granule, 1).unwrap());
        assert!(state.acquire_granule_write(granule, 1).unwrap());
        state.abort().unwrap();
        clear_current_thread_transaction_for_test();
    }

    #[test]
    fn model_permissions_write_without_permission_fails_without_mutating_state() {
        let trace = run_permission_semantic_schedule(&[PermissionModelOp::Write(9)]).unwrap();

        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].snapshot, PermissionModelState::default().snapshot);
        assert_eq!(trace[0].outcome, PermissionStepOutcome::Write { ok: false });
    }

    #[test]
    fn model_permissions_read_permission_allows_read_without_write() {
        let trace = run_permission_semantic_schedule(&[
            PermissionModelOp::GrantRead,
            PermissionModelOp::Read,
            PermissionModelOp::Write(9),
        ])
        .unwrap();

        assert_eq!(trace[2].outcome, PermissionStepOutcome::Write { ok: false });
        assert_eq!(
            trace[0].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[1].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(7),
            }
        );
        assert_eq!(
            trace[2].snapshot,
            PermissionRuntimeSnapshot {
                active: true,
                committed_value: 7,
                staged_value: None,
                read: true,
                write: false,
            }
        );
    }

    #[test]
    fn model_permissions_write_permission_grants_staged_mutation_and_reads_staged_value() {
        let trace = run_permission_semantic_schedule(&[
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(9),
            PermissionModelOp::Read,
        ])
        .unwrap();

        assert_eq!(
            trace[0].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(trace[1].outcome, PermissionStepOutcome::Write { ok: true });
        assert_eq!(
            trace[2].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(9),
            }
        );
        assert_eq!(
            trace[2].snapshot,
            PermissionRuntimeSnapshot {
                active: true,
                committed_value: 7,
                staged_value: Some(9),
                read: true,
                write: true,
            }
        );
    }

    #[test]
    fn model_permissions_downgrade_rejects_future_writes_until_reacquired() {
        let trace = run_permission_semantic_schedule(&[
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(9),
            PermissionModelOp::Downgrade,
            PermissionModelOp::Read,
            PermissionModelOp::Write(10),
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(10),
            PermissionModelOp::Read,
        ])
        .unwrap();

        assert_eq!(trace[4].outcome, PermissionStepOutcome::Write { ok: false });
        assert_eq!(
            trace[2].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[3].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(9),
            }
        );
        assert_eq!(
            trace[5].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[7].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(10),
            }
        );
    }

    #[test]
    fn model_permissions_abort_discards_state() {
        let trace = run_permission_semantic_schedule(&[
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(9),
            PermissionModelOp::Abort,
            PermissionModelOp::Read,
            PermissionModelOp::Begin,
            PermissionModelOp::GrantRead,
            PermissionModelOp::Read,
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(10),
            PermissionModelOp::Read,
        ])
        .unwrap();

        assert_eq!(
            trace[3].outcome,
            PermissionStepOutcome::Read {
                ok: false,
                value: None,
            }
        );
        assert_eq!(trace[2].outcome, PermissionStepOutcome::Unit { ok: true });
        assert_eq!(trace[4].outcome, PermissionStepOutcome::Unit { ok: true });
        assert_eq!(
            trace[5].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[6].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(7),
            }
        );
        assert_eq!(
            trace[7].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[9].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(10),
            }
        );
        assert_eq!(
            trace[3].snapshot,
            PermissionRuntimeSnapshot {
                active: false,
                committed_value: 7,
                staged_value: None,
                read: false,
                write: false,
            }
        );
    }

    #[test]
    fn model_permissions_commit_release_clears_permissions_and_commits_value() {
        let trace = run_permission_semantic_schedule(&[
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(9),
            PermissionModelOp::CommitRelease,
            PermissionModelOp::Read,
            PermissionModelOp::Begin,
            PermissionModelOp::GrantRead,
            PermissionModelOp::Read,
            PermissionModelOp::GrantWrite,
            PermissionModelOp::Write(10),
            PermissionModelOp::Read,
        ])
        .unwrap();

        assert_eq!(
            trace[3].outcome,
            PermissionStepOutcome::Read {
                ok: false,
                value: None,
            }
        );
        assert_eq!(trace[2].outcome, PermissionStepOutcome::Unit { ok: true });
        assert_eq!(trace[4].outcome, PermissionStepOutcome::Unit { ok: true });
        assert_eq!(
            trace[5].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[6].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(9),
            }
        );
        assert_eq!(
            trace[7].outcome,
            PermissionStepOutcome::Bool {
                ok: true,
                value: Some(true),
            }
        );
        assert_eq!(
            trace[9].outcome,
            PermissionStepOutcome::Read {
                ok: true,
                value: Some(10),
            }
        );
        assert_eq!(
            trace[3].snapshot,
            PermissionRuntimeSnapshot {
                active: false,
                committed_value: 9,
                staged_value: None,
                read: false,
                write: false,
            }
        );
    }

    #[test]
    fn model_permissions_generic_granule_state_is_not_object_specific() {
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(92));
        let granule = GranuleId::TGlobal {
            instance: Some(4),
            global_index: 1,
        };

        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState::default()
        );

        assert!(state.acquire_granule_read(granule, 0).unwrap());
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: true,
                read: true,
                write: false,
            }
        );

        assert!(state.acquire_granule_write(granule, 0).unwrap());
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: true,
                read: true,
                write: true,
            }
        );

        state.complete_commit().unwrap();
        assert_eq!(
            granule_permission_snapshot(&state, granule),
            GranulePermissionModelState {
                active: false,
                read: false,
                write: false,
            }
        );
        clear_current_thread_transaction_for_test();
    }

    #[test]
    fn model_permissions_ttable_granule_reacquires_after_abort() {
        assert_generic_granule_reacquires_after_abort(GranuleId::TTable {
            instance: Some(3),
            table_index: 2,
            granule_index: 1,
        });
    }

    #[test]
    fn model_permissions_ttable_size_reacquires_after_commit() {
        assert_generic_granule_reacquires_after_commit(GranuleId::TTableSize {
            instance: Some(3),
            table_index: 2,
        });
    }

    #[test]
    fn model_permissions_tglobal_reacquires_after_commit() {
        assert_generic_granule_reacquires_after_commit(GranuleId::TGlobal {
            instance: Some(3),
            global_index: 4,
        });
    }

    fn permission_model_op_strategy() -> impl Strategy<Value = PermissionModelOp> {
        prop_oneof![
            Just(PermissionModelOp::Begin),
            Just(PermissionModelOp::GrantRead),
            Just(PermissionModelOp::GrantWrite),
            Just(PermissionModelOp::Read),
            (0i32..=15).prop_map(PermissionModelOp::Write),
            Just(PermissionModelOp::Downgrade),
            Just(PermissionModelOp::Abort),
            Just(PermissionModelOp::CommitRelease),
        ]
    }

    #[test]
    fn model_permissions_generated_short_sequences_match_reference_world() {
        let mut runner = TestRunner::new(Config {
            cases: 64,
            failure_persistence: None,
            max_shrink_iters: 256,
            rng_seed: RngSeed::Fixed(0x5eed_0006),
            ..Config::default()
        });

        runner
            .run(
                &prop::collection::vec(permission_model_op_strategy(), 1..=10usize),
                |schedule| {
                    run_permission_semantic_schedule(&schedule)
                        .map_err(|error| TestCaseError::fail(error.to_string()))?;
                    Ok(())
                },
            )
            .unwrap();
    }
}

#[test]
fn transaction_object_tstruct_field_helpers_read_staged_payload_before_committed_record() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::I64(2)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();

    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(1)
    );
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
        .unwrap();

    assert_eq!(objects.version(object).unwrap(), 1);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::I64(2)])
    );
    assert_eq!(
        state.read_struct_field(&objects, object, 0).unwrap(),
        ObjectValue::I32(9)
    );
    assert!(state.owns_object_write(object));

    assert!(state.commit_object_payloads(&mut objects).unwrap());
    state.complete_commit().unwrap();

    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(9), ObjectValue::I64(2)])
    );
    assert!(!state.owns_object_write(object));
}

#[test]
fn transaction_object_tstruct_field_abort_discards_staged_payload() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 1, ObjectValue::I32(7))
        .unwrap();
    assert_eq!(
        state.read_struct_field(&objects, object, 1).unwrap(),
        ObjectValue::I32(7)
    );

    state.abort().unwrap();

    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
    );
    assert!(!state.owns_object_write(object));
}

#[test]
fn transaction_object_abort_frees_objects_allocated_by_active_transaction() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    state.record_allocated_object(object).unwrap();
    assert_eq!(objects.live_count(), 1);

    state.abort_allocated_objects(&mut objects).unwrap();

    assert!(state.active_transaction().is_none());
    assert_eq!(objects.live_count(), 0);
    assert!(objects.kind(object).is_err());

    let reused = objects.allocate_array(vec![ObjectValue::I32(2)]).unwrap();
    assert_eq!(reused, object);
}

#[test]
fn transaction_object_commit_keeps_objects_allocated_by_active_transaction() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    state.record_allocated_object(object).unwrap();
    state.complete_commit().unwrap();

    assert_eq!(objects.live_count(), 1);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1)])
    );

    state.begin().unwrap();
    state.abort_allocated_objects(&mut objects).unwrap();

    assert_eq!(objects.live_count(), 1);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1)])
    );
}

#[test]
fn transaction_object_tarray_helpers_stage_whole_object_and_commit_ranges() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_array(vec![
            ObjectValue::I32(0),
            ObjectValue::I32(1),
            ObjectValue::I32(2),
            ObjectValue::I32(3),
            ObjectValue::I32(4),
        ])
        .unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();

    assert_eq!(state.read_array_len(&objects, object).unwrap(), 5);
    assert_eq!(
        state.read_array_element(&objects, object, 2).unwrap(),
        ObjectValue::I32(2)
    );

    state
        .stage_array_element(&objects, object, 2, ObjectValue::I32(20))
        .unwrap();
    state
        .fill_array_range(&objects, object, 3, 2, ObjectValue::I32(9))
        .unwrap();
    state
        .copy_array_range(&objects, object, 0, object, 2, 3)
        .unwrap();
    state
        .write_array_range(
            &objects,
            object,
            1,
            vec![ObjectValue::I32(30), ObjectValue::I32(31)],
        )
        .unwrap();

    assert_eq!(
        state.read_object_payload(&objects, object).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::I32(20),
            ObjectValue::I32(30),
            ObjectValue::I32(31),
            ObjectValue::I32(9),
            ObjectValue::I32(9),
        ])
    );
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::I32(0),
            ObjectValue::I32(1),
            ObjectValue::I32(2),
            ObjectValue::I32(3),
            ObjectValue::I32(4),
        ])
    );
    assert!(state.owns_object_write(object));

    assert!(state.commit_object_payloads(&mut objects).unwrap());
    state.complete_commit().unwrap();

    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::I32(20),
            ObjectValue::I32(30),
            ObjectValue::I32(31),
            ObjectValue::I32(9),
            ObjectValue::I32(9),
        ])
    );
}

#[test]
fn transaction_object_tarray_range_helpers_validate_bounds() {
    let mut objects = ObjectTable::default();
    let object = objects.allocate_array(vec![ObjectValue::I32(0)]).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();

    let error = state.read_array_element(&objects, object, 1).unwrap_err();
    assert!(error.to_string().contains("out of bounds array access"));

    let error = state
        .fill_array_range(&objects, object, 1, 1, ObjectValue::I32(3))
        .unwrap_err();
    assert!(error.to_string().contains("out of bounds array access"));
}

#[test]
fn transaction_object_ti31_is_immediate_and_does_not_allocate_object_record() {
    let objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();

    let positive = state.create_i31(0x4000_0001).unwrap();
    assert_eq!(positive.get_u(), 0x4000_0001);
    assert_eq!(positive.get_s(), -0x3fff_ffff);

    let negative = state.create_i31(-1).unwrap();
    assert_eq!(negative.get_u(), 0x7fff_ffff);
    assert_eq!(negative.get_s(), -1);

    assert_eq!(objects.live_count(), 0);
    assert_eq!(objects.slot_count(), 0);
}

#[test]
fn transaction_object_unsupported_textern_promotion_aborts_transaction() {
    let mut state = TransactionState::default();

    state.begin().unwrap();

    let error = state
        .promote_extern_ref_for_persistence(0x1234)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("persistent extern object promotion is not supported")
    );
    assert!(state.active_transaction().is_none());
}

#[test]
fn object_payload_commit_validates_object_read_versions() {
    let mut objects = ObjectTable::default();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_read(&mut objects, object).unwrap();
    state.read_object_payload(&objects, object).unwrap();
    objects
        .update_payload(object, ObjectPayload::Struct(vec![ObjectValue::I32(2)]))
        .unwrap();

    let error = state.commit_object_payloads(&mut objects).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("optimistic read version changed")
    );
}

#[test]
fn transaction_object_read_validation_uses_object_table_versions() {
    let mut objects = ObjectTable::default();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    state.acquire_object_read(&mut objects, object).unwrap();

    state.validate_active_object_reads(&objects).unwrap();

    objects
        .update_payload(object, ObjectPayload::Struct(vec![ObjectValue::I32(2)]))
        .unwrap();

    let error = state.validate_active_object_reads(&objects).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("optimistic read version changed")
    );
}

#[test]
fn transaction_object_header_records_object_metadata() {
    let mut heap = object_heap::ObjectHeap::default();
    let object_id = ObjectId { object_index: 17 };
    let payload = ObjectPayload::Struct(vec![
        ObjectValue::I32(11),
        ObjectValue::Ref(Some(ObjectId { object_index: 3 })),
    ]);

    let handle = heap
        .allocate_record(object_id, 1, ObjectKind::Struct, 0x23, 41, &payload)
        .unwrap();
    let header = heap.header(handle).unwrap();

    assert_eq!(header.object_id, object_id.object_index);
    assert_eq!(header.version, 1);
    assert_eq!(header.kind, ObjectKind::Struct as u16);
    assert_eq!(header.flags, 0x23);
    assert_eq!(header.type_layout_id, 41);
    assert_eq!(header.record_len, heap.record_len(handle).unwrap());
}

#[test]
fn transaction_object_array_header_records_length() {
    let mut heap = object_heap::ObjectHeap::default();
    let object_id = ObjectId { object_index: 9 };
    let payload = ObjectPayload::Array(vec![
        ObjectValue::I64(1),
        ObjectValue::Ref(Some(ObjectId { object_index: 4 })),
        ObjectValue::Ref(None),
    ]);

    let handle = heap
        .allocate_record(object_id, 2, ObjectKind::Array, 0, 7, &payload)
        .unwrap();
    let header = heap.array_header(handle).unwrap();

    assert_eq!(header.base.object_id, object_id.object_index);
    assert_eq!(header.base.version, 2);
    assert_eq!(header.base.kind, ObjectKind::Array as u16);
    assert_eq!(header.base.type_layout_id, 7);
    assert_eq!(header.length, 3);
    assert_eq!(header.base.record_len, heap.record_len(handle).unwrap());
}

#[test]
fn transaction_object_heap_writes_records_to_block_region() {
    let mut heap = object_heap::ObjectHeap::default();
    let payload = ObjectPayload::Struct(vec![ObjectValue::I32(0x1122_3344)]);

    let handle = heap
        .allocate_record(
            ObjectId { object_index: 17 },
            3,
            ObjectKind::Struct,
            0x23,
            41,
            &payload,
        )
        .unwrap();

    let bytes = heap.record_bytes_for_test(handle).unwrap();
    let payload_offset = core::mem::size_of::<object_heap::TxObjectHeader>();
    assert_eq!(heap.record_offset_for_test(handle).unwrap(), 0);
    assert_eq!(
        u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        heap.record_len(handle).unwrap()
    );
    assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 17);
    assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 3);
    assert_eq!(
        u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
        ObjectKind::Struct as u16
    );
    assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 0x23);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 41);
    assert_eq!(
        ObjectValueAbi::from_parts(
            u32::from_le_bytes(
                bytes[payload_offset..payload_offset + 4]
                    .try_into()
                    .unwrap()
            ),
            u64::from_le_bytes(
                bytes[payload_offset + 4..payload_offset + 12]
                    .try_into()
                    .unwrap()
            ),
            u64::from_le_bytes(
                bytes[payload_offset + 12..payload_offset + 20]
                    .try_into()
                    .unwrap()
            ),
        )
        .unwrap()
        .to_object_value()
        .unwrap(),
        ObjectValue::I32(0x1122_3344)
    );
}

#[test]
fn transaction_object_heap_marks_allocated_lines() {
    let mut heap = object_heap::ObjectHeap::default();
    let handle = heap
        .allocate_record(
            ObjectId { object_index: 3 },
            4,
            ObjectKind::Struct,
            0,
            0,
            &ObjectPayload::Struct(vec![ObjectValue::I64(9)]),
        )
        .unwrap();

    assert_eq!(heap.block_region_block_size_for_test(), 512 * 1024);
    assert_eq!(heap.immix_line_size_for_test(), 256);
    assert!(heap.line_mark_for_record_for_test(handle).unwrap());
}

#[test]
fn transaction_object_heap_grows_past_initial_chunk() {
    let mut heap = object_heap::ObjectHeap::default();
    let payload = ObjectPayload::Array(vec![
        ObjectValue::I32(7);
        (heap.block_region_block_size_for_test() * 4 / 20) + 1
    ]);

    let handle = heap
        .allocate_record(
            ObjectId { object_index: 4 },
            1,
            ObjectKind::Array,
            0,
            0,
            &payload,
        )
        .unwrap();
    let location = heap.record_location_for_test(handle).unwrap();

    assert!(location.chunk_blocks > 4);
    assert_eq!(location.data_block, location.chunk_start_block);
    assert_eq!(
        heap.record_bytes_for_test(handle).unwrap().len() as u64,
        heap.record_len(handle).unwrap()
    );
    assert!(heap.line_mark_for_record_for_test(handle).unwrap());
}

#[test]
fn transaction_object_heap_records_block_locations() {
    let mut heap = object_heap::ObjectHeap::default();
    let payload = ObjectPayload::Struct(vec![ObjectValue::I64(1)]);

    let first = heap
        .allocate_record(
            ObjectId { object_index: 5 },
            1,
            ObjectKind::Struct,
            0,
            0,
            &payload,
        )
        .unwrap();
    let second = heap
        .allocate_record(
            ObjectId { object_index: 6 },
            1,
            ObjectKind::Struct,
            0,
            0,
            &payload,
        )
        .unwrap();

    let first_location = heap.record_location_for_test(first).unwrap();
    let second_location = heap.record_location_for_test(second).unwrap();

    assert_eq!(first_location.chunk_start_block, 0);
    assert_eq!(first_location.data_block, 0);
    assert_eq!(first_location.data_offset, 0);
    assert_eq!(first_location.record_len, heap.record_len(first).unwrap());
    assert_eq!(first_location.chunk_blocks, 4);
    assert_eq!(second_location.chunk_start_block, 0);
    assert_eq!(second_location.data_block, 0);
    assert!(second_location.data_offset > first_location.data_offset);
}

#[test]
fn transaction_object_id_stays_stable_when_record_handle_changes() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        .unwrap();

    let first_handle = objects.current_record_handle_for_test(object).unwrap();
    let first_version = objects.version(object).unwrap();

    objects
        .update_payload(
            object,
            ObjectPayload::Struct(vec![
                ObjectValue::I32(2),
                ObjectValue::Ref(Some(ObjectId { object_index: 99 })),
            ]),
        )
        .unwrap();

    let second_handle = objects.current_record_handle_for_test(object).unwrap();
    let second_version = objects.version(object).unwrap();

    assert_eq!(object, ObjectId { object_index: 0 });
    assert_ne!(first_handle, second_handle);
    assert!(second_version > first_version);
}

#[test]
fn persistent_gc_copy_refreshes_record_version_without_changing_object_id() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x9100,
            910,
            1,
            vec![ObjectValue::I32(7)],
        )
        .unwrap();
    let before = objects.pending_publication_for_test(object).unwrap();
    let copied = objects
        .persistent_gc_copy_publication_for_test(object)
        .unwrap();

    assert_eq!(before.logical_id, copied.logical_id);
    assert!(copied.version > before.version);
    assert_eq!(
        objects
            .pending_publication_for_test(object)
            .unwrap()
            .version,
        before.version
    );
    assert_eq!(
        objects
            .install_persistent_gc_copied_publication_for_test(&copied)
            .unwrap(),
        object
    );
    assert_eq!(
        objects
            .pending_publication_for_test(object)
            .unwrap()
            .version,
        copied.version
    );
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(7)])
    );
}

#[test]
fn transaction_object_records_serialize_version() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        .unwrap();

    let first_handle = objects.current_record_handle_for_test(object).unwrap();
    let first_bytes = objects.heap.record_bytes_for_test(first_handle).unwrap();
    let first_version = u32::from_le_bytes(first_bytes[16..20].try_into().unwrap());

    objects
        .update_payload(
            object,
            ObjectPayload::Struct(vec![
                ObjectValue::I32(2),
                ObjectValue::Ref(Some(ObjectId { object_index: 99 })),
            ]),
        )
        .unwrap();

    let second_handle = objects.current_record_handle_for_test(object).unwrap();
    let second_bytes = objects.heap.record_bytes_for_test(second_handle).unwrap();
    let second_version = u32::from_le_bytes(second_bytes[16..20].try_into().unwrap());

    assert_eq!(first_version, 1);
    assert_eq!(second_version, 2);
}

#[test]
fn commit_publishes_struct_object_update() {
    let mut objects = ObjectTable::default();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();

    let pub_ = objects.pending_publication_for_test(object).unwrap();
    let (domain, object_id) =
        crate::runtime::vm::unpack_object_granule_id(pub_.logical_id).unwrap();
    assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TStruct);
    assert_eq!(object_id, object.object_index);
    assert_eq!(
        pub_.kind,
        crate::runtime::vm::PackedGranuleDomain::TStruct as u16
    );
    assert_eq!(pub_.version, 1);
}

#[test]
fn commit_publishes_array_object_update() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_array(vec![ObjectValue::Ref(None), ObjectValue::I64(9)])
        .unwrap();

    let pub_ = objects.pending_publication_for_test(object).unwrap();
    let (domain, object_id) =
        crate::runtime::vm::unpack_object_granule_id(pub_.logical_id).unwrap();
    assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TArray);
    assert_eq!(object_id, object.object_index);
    assert_eq!(
        pub_.kind,
        crate::runtime::vm::PackedGranuleDomain::TArray as u16
    );
    assert_eq!(pub_.version, 1);
}

#[test]
fn persistent_publication_traces_object_id_after_live_bridge_maps_are_cleared() -> Result<()> {
    let mut objects = ObjectTable::default();
    let child = objects.allocate_persistent_struct_for_gc_ref(0x801, vec![ObjectValue::I32(10)])?;
    let parent = objects
        .allocate_persistent_struct_for_gc_ref(0x802, vec![ObjectValue::Ref(Some(child))])?;

    let publication = objects.pending_publication_for_test(parent)?;
    objects.live_bridge_gc_refs_to_objects.clear();
    objects.object_to_live_bridge_gc_ref.clear();

    let refs = objects.trace_object_ids(parent)?;
    assert_eq!(refs, vec![child]);
    let (domain, object_id) = crate::runtime::vm::unpack_object_granule_id(publication.logical_id)?;
    assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TStruct);
    assert_eq!(object_id, parent.object_index);
    Ok(())
}

#[test]
fn object_table_rebuilds_latest_slots_from_heap_publication_metadata() {
    let mut objects = ObjectTable::default();
    let first = objects
        .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        .unwrap();
    let second = objects
        .allocate_array(vec![ObjectValue::Ref(Some(first)), ObjectValue::I64(7)])
        .unwrap();

    objects
        .update_payload(
            first,
            ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(Some(second))]),
        )
        .unwrap();

    let first_latest_handle = objects.current_record_handle_for_test(first).unwrap();
    let second_latest_handle = objects.current_record_handle_for_test(second).unwrap();
    let first_latest_payload = objects.payload(first).unwrap();
    let second_latest_payload = objects.payload(second).unwrap();

    objects.slots.clear();
    objects.free_list.clear();
    objects.live_bridge_gc_refs_to_objects.clear();
    objects.object_to_live_bridge_gc_ref.clear();
    objects.next_version = 0;
    objects.live_count = 0;

    objects.rebuild_volatile_index_from_heap().unwrap();

    assert_eq!(
        objects.current_record_handle_for_test(first).unwrap(),
        first_latest_handle
    );
    assert_eq!(
        objects.current_record_handle_for_test(second).unwrap(),
        second_latest_handle
    );
    assert_eq!(objects.payload(first).unwrap(), first_latest_payload);
    assert_eq!(objects.payload(second).unwrap(), second_latest_payload);
    assert_eq!(objects.live_count(), 2);
}

#[test]
fn object_index_rebuilds_from_recovered_object_winners() {
    let region = sample_region_with_two_object_winners();
    let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
    let winners = recovered.committed_object_winners().unwrap();
    let mut objects = ObjectTable::default();

    objects
        .rebuild_from_recovery_for_test(&recovered.type_layouts, &winners)
        .unwrap();

    assert_eq!(objects.live_count(), 2);
    assert!(
        objects
            .type_layouts()
            .contains(type_layout::TypeLayoutId::new(7).unwrap())
    );
    assert!(
        objects
            .type_layouts()
            .contains(type_layout::TypeLayoutId::new(9).unwrap())
    );
    assert!(objects.live_bridge_gc_refs_to_objects.is_empty());
    assert!(objects.object_to_live_bridge_gc_ref.is_empty());
    assert_eq!(
        objects.kind(ObjectId { object_index: 41 }).unwrap(),
        ObjectKind::Struct
    );
    assert_eq!(
        objects
            .live_slot(ObjectId { object_index: 41 })
            .unwrap()
            .type_layout_id,
        7
    );
    assert_eq!(
        objects.payload(ObjectId { object_index: 41 }).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
    );
    assert_eq!(
        objects.kind(ObjectId { object_index: 42 }).unwrap(),
        ObjectKind::Array
    );
    assert_eq!(
        objects
            .live_slot(ObjectId { object_index: 42 })
            .unwrap()
            .type_layout_id,
        9
    );
    assert_eq!(
        objects.payload(ObjectId { object_index: 42 }).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
            ObjectValue::I64(9),
        ])
    );
}

#[test]
fn transaction_object_scanner_returns_embedded_refs_from_struct_and_array_payloads() {
    let mut struct_heap = object_heap::ObjectHeap::default();
    let struct_handle = struct_heap
        .allocate_record(
            ObjectId { object_index: 1 },
            5,
            ObjectKind::Struct,
            0,
            3,
            &ObjectPayload::Struct(vec![
                ObjectValue::I32(7),
                ObjectValue::Ref(Some(ObjectId { object_index: 11 })),
                ObjectValue::Ref(None),
                ObjectValue::V128([0; 16]),
                ObjectValue::Ref(Some(ObjectId { object_index: 12 })),
            ]),
        )
        .unwrap();

    assert_eq!(
        struct_heap.trace_object_ids(struct_handle).unwrap(),
        vec![ObjectId { object_index: 11 }, ObjectId { object_index: 12 }]
    );

    let mut array_heap = object_heap::ObjectHeap::default();
    let array_handle = array_heap
        .allocate_record(
            ObjectId { object_index: 2 },
            6,
            ObjectKind::Array,
            0,
            4,
            &ObjectPayload::Array(vec![
                ObjectValue::Ref(None),
                ObjectValue::Ref(Some(ObjectId { object_index: 21 })),
                ObjectValue::I64(4),
                ObjectValue::Ref(Some(ObjectId { object_index: 22 })),
            ]),
        )
        .unwrap();

    assert_eq!(
        array_heap.trace_object_ids(array_handle).unwrap(),
        vec![ObjectId { object_index: 21 }, ObjectId { object_index: 22 }]
    );
}

struct FileBackedRecoveredObjectCase {
    recovered_region: crate::runtime::vm::RecoveredRegion,
    object_winners: Vec<crate::runtime::vm::RecoveredObjectWinner>,
    rebuilt: ObjectTable,
}

fn recover_file_backed_object_layout_case_for_test<T, F>(
    txid: u32,
    prepare: F,
) -> Result<(T, FileBackedRecoveredObjectCase)>
where
    F: FnOnce(&mut ObjectTable, &mut TransactionState) -> Result<T>,
{
    let dir = tempfile::tempdir()?;
    let tx_log_path = dir.path().join("tx-log.bin");
    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64)?;
    let mut state = TransactionState::new_for_test_with_durable_log(
        TransactionId::from_raw(u64::from(txid)),
        durable_log,
    );
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut objects = ObjectTable::default();
    let expected = prepare(&mut objects, &mut state)?;
    let mut publications = Vec::new();
    ensure!(
        state.commit_object_payloads_into(&mut objects, &mut publications)?,
        "expected staged persistent object publications"
    );
    let marker = state
        .publish_object_publications_before_commit(txid, txid, &objects, &publications)?
        .context("expected committed persistent object publication")?;
    state.publish_commit_lp(txid, txid, marker)?;
    drop(state);
    let recovered = recover_file_backed_objects_from_path_for_test(&tx_log_path)?;
    Ok((expected, recovered))
}

fn recover_file_backed_objects_from_path_for_test(
    path: &std::path::Path,
) -> Result<FileBackedRecoveredObjectCase> {
    let recovered_region =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(path)?;
    let object_winners = recovered_region.committed_object_winners()?;
    let mut rebuilt = ObjectTable::default();
    rebuilt.rebuild_from_recovery_for_test(&recovered_region.type_layouts, &object_winners)?;
    Ok(FileBackedRecoveredObjectCase {
        recovered_region,
        object_winners,
        rebuilt,
    })
}

fn recover_file_backed_recovery_inputs_for_test(
    path: &std::path::Path,
) -> Result<(
    crate::runtime::vm::RecoveredRegion,
    Vec<crate::runtime::vm::RecoveredObjectWinner>,
)> {
    let recovered_region =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(path)?;
    let object_winners = recovered_region.committed_object_winners()?;
    Ok((recovered_region, object_winners))
}

fn recovered_object_winner_by_id_for_test<'a>(
    winners: &'a [crate::runtime::vm::RecoveredObjectWinner],
    object_id: ObjectId,
) -> &'a crate::runtime::vm::RecoveredObjectWinner {
    winners
        .iter()
        .find(|winner| winner.object_id == object_id.object_index)
        .unwrap_or_else(|| {
            panic!(
                "missing recovered object winner for object id {}; recovered object ids: {:?}",
                object_id.object_index,
                winners
                    .iter()
                    .map(|winner| winner.object_id)
                    .collect::<Vec<_>>()
            )
        })
}

fn file_backed_recovered_chunk_meta_for_test(
    path: &std::path::Path,
    winner: &crate::runtime::vm::RecoveredObjectWinner,
) -> Result<(u32, u32)> {
    let region =
        crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(path)?;
    let meta = region.block_meta(winner.data_block).with_context(|| {
        format!(
            "failed to read chunk metadata for recovered object {} at data block {}",
            winner.object_id, winner.data_block
        )
    })?;
    Ok((meta.chunk_start, meta.generation))
}

fn commit_file_backed_publications_for_test<T, F>(
    txid: u32,
    prepare: F,
) -> Result<(
    T,
    crate::runtime::vm::RecoveredRegion,
    Vec<crate::runtime::vm::RecoveredObjectWinner>,
)>
where
    F: FnOnce(&mut ObjectTable, &mut TransactionState) -> Result<T>,
{
    let dir = tempfile::tempdir()?;
    let tx_log_path = dir.path().join("tx-log.bin");
    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64)?;
    let mut state = TransactionState::new_for_test_with_durable_log(
        TransactionId::from_raw(u64::from(txid)),
        durable_log,
    );
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut objects = ObjectTable::default();
    let expected = prepare(&mut objects, &mut state)?;

    let mut publications = Vec::new();
    state.promote_persistent_references_before_commit(&mut objects)?;
    state.commit_object_payloads_into(&mut objects, &mut publications)?;
    let root_delta = state.staged_persistent_root_delta(&objects)?;
    let persistent_gc_delta = state.persistent_gc_commit_delta(&objects, &publications)?;
    publications.extend(state.persistent_root_publications(&root_delta)?);

    if let Some(marker) =
        state.publish_object_publications_before_commit(txid, txid, &objects, &publications)?
    {
        state.publish_commit_lp(txid, txid, marker)?;
    }
    state.complete_commit()?;
    state.apply_committed_persistent_root_delta(root_delta)?;
    let _ =
        state.observe_persistent_gc_commit_delta_after_commit(&objects, &persistent_gc_delta)?;
    drop(state);

    let (recovered_region, object_winners) =
        recover_file_backed_recovery_inputs_for_test(&tx_log_path)?;
    Ok((expected, recovered_region, object_winners))
}

fn commit_active_file_backed_publications_for_test(
    stream_id: u32,
    txid: u32,
    objects: &mut ObjectTable,
    state: &mut TransactionState,
) -> Result<()> {
    let mut publications = Vec::new();
    state.promote_persistent_references_before_commit(objects)?;
    state.commit_object_payloads_into(objects, &mut publications)?;
    let root_delta = state.staged_persistent_root_delta(objects)?;
    let persistent_gc_delta = state.persistent_gc_commit_delta(objects, &publications)?;
    publications.extend(state.persistent_root_publications(&root_delta)?);

    if let Some(marker) =
        state.publish_object_publications_before_commit(stream_id, txid, objects, &publications)?
    {
        state.publish_commit_lp(stream_id, txid, marker)?;
    }
    state.complete_commit()?;
    state.apply_committed_persistent_root_delta(root_delta)?;
    let _ = state.observe_persistent_gc_commit_delta_after_commit(objects, &persistent_gc_delta)?;
    Ok(())
}

fn recover_file_backed_object_without_layout_metadata_for_test(
    publication: &persist::PendingPublication,
) -> Result<crate::runtime::vm::RecoveredRegion> {
    let dir = tempfile::tempdir()?;
    let tx_log_path = dir.path().join("tx-log.bin");
    let mut durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64)?;
    let stream_id = 41;
    let txid = 41;
    let marker = {
        let mut sink = durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        publisher.publish_object_publication_before_commit(publication)?
    };
    {
        let mut sink = durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        publisher.publish_commit_lp(marker)?;
    }
    drop(durable_log);
    crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(&tx_log_path)
}

#[test]
fn persistent_root_commit_publishes_global_root_record() {
    let (root, recovered_region, _) =
        commit_file_backed_publications_for_test(701, |objects, state| {
            let root = objects
                .allocate_persistent_struct_for_gc_ref(0x701, vec![ObjectValue::I32(1)])
                .unwrap();
            state.acquire_object_write(objects, root)?;
            state.stage_struct_field(objects, root, 0, ObjectValue::I32(9))?;
            state.stage_global(0, GlobalSnapshot::GcRef(0x701))?;
            Ok(root)
        })
        .unwrap();

    assert_eq!(recovered_region.root_object_ids, vec![root.object_index]);
}

#[test]
fn persistent_promotion_commit_path_publishes_promoted_root_object() {
    let (source, recovered_region, object_winners) =
        commit_file_backed_publications_for_test(703, |objects, state| {
            let source = objects
                .allocate_struct_for_gc_ref(0x736, vec![ObjectValue::I32(36)])
                .unwrap();
            state.stage_global(0, GlobalSnapshot::GcRef(0x736))?;
            Ok(source)
        })
        .unwrap();

    assert_ne!(recovered_region.root_object_ids, vec![source.object_index]);
    assert_eq!(object_winners.len(), 1);
    assert_eq!(
        recovered_region.root_object_ids,
        vec![object_winners[0].object_id]
    );
}

#[test]
fn persistent_root_commit_root_only_writes_final_marker() {
    let (root, recovered_region, object_winners) =
        commit_file_backed_publications_for_test(702, |objects, state| {
            let root = objects
                .allocate_persistent_struct_for_gc_ref(0x702, vec![ObjectValue::I32(2)])
                .unwrap();
            state.stage_global(0, GlobalSnapshot::GcRef(0x702))?;
            Ok(root)
        })
        .unwrap();

    assert!(object_winners.is_empty());
    assert_eq!(recovered_region.root_object_ids, vec![root.object_index]);
}

#[test]
fn persistent_root_publications_distinguish_global_instances() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x703, vec![ObjectValue::I32(3)])
        .unwrap();
    objects
        .allocate_persistent_struct_for_gc_ref(0x704, vec![ObjectValue::I32(4)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(703));
    state
        .stage_global_owned(
            Some(InstanceId::from_u32(1)),
            0,
            GlobalSnapshot::GcRef(0x703),
        )
        .unwrap();
    state
        .stage_global_owned(
            Some(InstanceId::from_u32(2)),
            0,
            GlobalSnapshot::GcRef(0x704),
        )
        .unwrap();

    let delta = state.staged_persistent_root_delta(&objects).unwrap();
    let publications = state.persistent_root_publications(&delta).unwrap();

    assert_eq!(publications.len(), 2);
    assert_ne!(publications[0].logical_id, publications[1].logical_id);
}

mod persistent_ref_promotion_boundary {
    use super::*;

    #[test]
    fn persistent_root_commit_rejects_unknown_volatile_gc_ref() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let objects = ObjectTable::default();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(704));

        state.stage_global(0, GlobalSnapshot::GcRef(0x704)).unwrap();
        let err = state
            .staged_persistent_root_delta(&objects)
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "volatile GC reference promotion into persistent object graph is not implemented yet"
        );
    }

    #[test]
    fn persistent_root_commit_rejects_known_non_persistent_gc_ref() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut objects = ObjectTable::default();
        objects
            .allocate_struct_for_gc_ref(0x706, vec![ObjectValue::I32(6)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(706));

        state.stage_global(0, GlobalSnapshot::GcRef(0x706)).unwrap();
        let err = state
            .staged_persistent_root_delta(&objects)
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "volatile GC reference promotion into persistent object graph is not implemented yet"
        );
    }

    #[test]
    fn persistent_object_payload_commit_rejects_unknown_volatile_gc_ref() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(
                0x705,
                vec![ObjectValue::Ref(Some(ObjectId { object_index: 77 }))],
            )
            .unwrap();

        let err = objects
            .object_pending_publication(object)
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "volatile GC reference promotion into persistent object graph is not implemented yet"
        );
    }

    #[test]
    fn persistent_payload_rejects_unpromoted_volatile_ref() -> Result<()> {
        let mut objects = ObjectTable::default();
        let volatile = objects.allocate_struct_for_gc_ref(0x901, vec![ObjectValue::I32(1)])?;
        let persistent = objects
            .allocate_persistent_struct_for_gc_ref(0x902, vec![ObjectValue::Ref(Some(volatile))])?;

        let err = objects
            .pending_publication_for_test(persistent)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED)
        );
        Ok(())
    }
}

mod persistent_promotion_reservation {
    use super::*;

    #[test]
    fn promotion_reserves_new_persistent_object_id_without_reusing_source() {
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x710, vec![ObjectValue::I32(7)])
            .unwrap();

        let promoted = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Struct,
                TypeLayoutId::DEFAULT_STRUCT,
            )
            .unwrap();

        assert_ne!(promoted, source);
        assert!(!objects.is_persistent(source).unwrap());
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            objects.payload(promoted).unwrap(),
            ObjectPayload::Struct(Vec::new())
        );
        assert_eq!(
            objects.known_object_id_for_live_gc_ref_bridge(0x710),
            Some(source)
        );
    }

    #[test]
    fn promotion_fill_preserves_payload_and_type_layout() {
        let mut objects = ObjectTable::default();
        let promoted = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Array,
                TypeLayoutId::DEFAULT_ARRAY,
            )
            .unwrap();

        objects
            .fill_reserved_persistent_object_for_promotion(
                promoted,
                ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)]),
            )
            .unwrap();

        assert_eq!(
            objects.payload(promoted).unwrap(),
            ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
        );
        assert_eq!(
            objects.live_slot(promoted).unwrap().type_layout_id,
            TypeLayoutId::DEFAULT_ARRAY.get()
        );
    }

    #[test]
    fn promotion_fill_preserves_registered_non_default_type_layout() {
        let mut objects = ObjectTable::default();
        let layout = type_layout::PersistentTypeLayout::Struct {
            id: type_layout::TypeLayoutId::new(111).unwrap(),
            fingerprint: 0x0111_0000_0000_0011,
            body_size: 16,
            fields: vec![type_layout::StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 8,
                kind: type_layout::TraceSlotKind::Scalar,
            }],
        };
        objects.register_type_layout(layout.clone()).unwrap();

        let promoted = objects
            .reserve_persistent_object_id_for_promotion(ObjectKind::Struct, layout.id())
            .unwrap();

        assert_eq!(
            objects.live_slot(promoted).unwrap().type_layout_id,
            layout.id().get()
        );

        objects
            .fill_reserved_persistent_object_for_promotion(
                promoted,
                ObjectPayload::Struct(vec![ObjectValue::I32(3)]),
            )
            .unwrap();

        assert_eq!(
            objects.live_slot(promoted).unwrap().type_layout_id,
            layout.id().get()
        );
        assert_eq!(
            objects.payload(promoted).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(3)])
        );
    }

    #[test]
    fn promotion_fill_rejects_non_persistent_target() {
        let mut objects = ObjectTable::default();
        let target = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();

        let err = objects
            .fill_reserved_persistent_object_for_promotion(
                target,
                ObjectPayload::Struct(vec![ObjectValue::I32(2)]),
            )
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("promotion target object must be persistent")
        );
    }

    #[test]
    fn promotion_fill_rejects_kind_mismatch() {
        let mut objects = ObjectTable::default();
        let target = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Struct,
                TypeLayoutId::DEFAULT_STRUCT,
            )
            .unwrap();

        let err = objects
            .fill_reserved_persistent_object_for_promotion(
                target,
                ObjectPayload::Array(vec![ObjectValue::I32(2)]),
            )
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("promotion target object kind does not match promoted payload")
        );
    }

    #[test]
    fn promotion_fill_rejects_payload_with_volatile_object_ref() {
        let mut objects = ObjectTable::default();
        let volatile = objects.allocate_struct(vec![ObjectValue::I32(4)]).unwrap();
        let target = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Struct,
                TypeLayoutId::DEFAULT_STRUCT,
            )
            .unwrap();

        let err = objects
            .fill_reserved_persistent_object_for_promotion(
                target,
                ObjectPayload::Struct(vec![ObjectValue::Ref(Some(volatile))]),
            )
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "volatile GC reference promotion into persistent object graph is not implemented yet"
        );
    }

    #[test]
    fn promotion_fill_rejects_payload_with_missing_object_ref() {
        let mut objects = ObjectTable::default();
        let target = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Struct,
                TypeLayoutId::DEFAULT_STRUCT,
            )
            .unwrap();

        let err = objects
            .fill_reserved_persistent_object_for_promotion(
                target,
                ObjectPayload::Struct(vec![ObjectValue::Ref(Some(ObjectId { object_index: 404 }))]),
            )
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "volatile GC reference promotion into persistent object graph is not implemented yet"
        );
    }
}

mod persistent_promotion_graph {
    use super::*;

    #[test]
    fn promotion_maps_volatile_struct_to_new_persistent_object_id() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x720, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(720));

        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();

        assert_ne!(promoted, source);
        assert!(!objects.is_persistent(source).unwrap());
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(state.promoted_object_for_test(source), Some(promoted));
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(7)])
        );
    }

    #[test]
    fn promotion_rewrites_child_refs_to_promoted_targets() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let child = objects
            .allocate_struct_for_gc_ref(0x721, vec![ObjectValue::I32(1)])
            .unwrap();
        let root = objects
            .allocate_struct_for_gc_ref(0x722, vec![ObjectValue::Ref(Some(child))])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(721));

        let promoted_root = state
            .promote_transaction_object_graph_for_test(&mut objects, root)
            .unwrap();
        let promoted_child = state.promoted_object_for_test(child).unwrap();

        assert_ne!(promoted_root, root);
        assert_ne!(promoted_child, child);
        assert_eq!(
            state.staged_object_payload_for_test(promoted_root).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_child))])
        );
        assert_eq!(
            state
                .staged_object_payload_for_test(promoted_child)
                .unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(1)])
        );
    }

    #[test]
    fn promotion_preserves_cycles_with_reserved_object_ids() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let left = objects
            .allocate_struct_for_gc_ref(0x723, vec![ObjectValue::Ref(None)])
            .unwrap();
        let right = objects
            .allocate_struct_for_gc_ref(0x724, vec![ObjectValue::Ref(Some(left))])
            .unwrap();
        objects
            .update_payload(
                left,
                ObjectPayload::Struct(vec![ObjectValue::Ref(Some(right))]),
            )
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(723));

        let promoted_left = state
            .promote_transaction_object_graph_for_test(&mut objects, left)
            .unwrap();
        let promoted_right = state.promoted_object_for_test(right).unwrap();

        assert_eq!(
            state.staged_object_payload_for_test(promoted_left).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_right))])
        );
        assert_eq!(
            state
                .staged_object_payload_for_test(promoted_right)
                .unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_left))])
        );
    }

    #[test]
    fn promotion_preserves_inline_durable_values_inside_payload() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let func = DurableFuncIdentity {
            module_fingerprint: 0x724,
            function_index: 1,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let extern_ = DurableExternIdentity {
            namespace: 7,
            handle: 0x726,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let payload = ObjectPayload::Struct(vec![
            ObjectValue::I31(-17),
            ObjectValue::FuncRef(func),
            ObjectValue::ExternRef(extern_),
        ]);
        let source = objects
            .allocate_struct_for_gc_ref(
                0x724,
                match &payload {
                    ObjectPayload::Struct(fields) => fields.clone(),
                    ObjectPayload::Array(_) => unreachable!(),
                },
            )
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(724));

        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();

        assert_ne!(promoted, source);
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(state.allocated_object_count_for_test(), 1);
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &payload
        );
    }

    #[test]
    fn promotion_failure_rolls_back_earlier_promoted_siblings() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let good_child = objects
            .allocate_struct_for_gc_ref(0x727, vec![ObjectValue::I32(1)])
            .unwrap();
        let bad_child = ObjectId { object_index: 999 };
        let parent = objects
            .allocate_struct_for_gc_ref(
                0x728,
                vec![
                    ObjectValue::Ref(Some(good_child)),
                    ObjectValue::Ref(Some(bad_child)),
                ],
            )
            .unwrap();
        let initial_live_count = objects.live_count();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(727));

        let err = state
            .promote_transaction_object_graph_for_test(&mut objects, parent)
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "object table slot is not live: ObjectId { object_index: 999 }"
        );
        assert_eq!(state.promoted_object_for_test(parent), None);
        assert_eq!(state.promoted_object_for_test(good_child), None);
        assert_eq!(state.promoted_object_for_test(bad_child), None);
        assert_eq!(state.allocated_object_count_for_test(), 0);
        assert_eq!(objects.live_count(), initial_live_count);
    }

    #[test]
    fn promotion_maps_survive_suspend_resume_and_clear_on_abort() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x731, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::default();
        let first = TransactionId::from_raw(731);
        let second = TransactionId::from_raw(732);

        assert_eq!(state.enter_transaction(first).unwrap(), None);
        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();
        assert_eq!(state.promoted_object_for_test(source), Some(promoted));
        assert!(state.staged_object_payload_for_test(promoted).is_some());

        assert_eq!(state.enter_transaction(second).unwrap(), Some(first));
        assert_eq!(state.promoted_object_for_test(source), None);

        state.restore_transaction(Some(first)).unwrap();
        assert_eq!(state.promoted_object_for_test(source), Some(promoted));
        assert!(state.staged_object_payload_for_test(promoted).is_some());

        state.abort_allocated_objects(&mut objects).unwrap();
        assert_eq!(state.active_transaction(), None);
        assert_eq!(state.promoted_object_for_test(source), None);
        assert!(state.staged_object_payload_for_test(promoted).is_none());
        assert_eq!(state.allocated_object_count_for_test(), 0);
        assert!(objects.kind(promoted).is_err());
    }
}

mod persistent_promotion_commit {
    use super::*;

    #[derive(Default)]
    struct FakeOrdinaryGcPromotionAdapter {
        sources: BTreeMap<u32, OrdinaryGcPromotionSource>,
    }

    impl FakeOrdinaryGcPromotionAdapter {
        fn with_source(mut self, gc_ref: u32, source: OrdinaryGcPromotionSource) -> Self {
            self.sources.insert(gc_ref, source);
            self
        }
    }

    impl OrdinaryGcPromotionAdapter for FakeOrdinaryGcPromotionAdapter {
        fn promotion_source_for_gc_ref(
            &mut self,
            _object_table: &mut ObjectTable,
            gc_ref: u32,
        ) -> Result<Option<OrdinaryGcPromotionSource>> {
            Ok(self.sources.get(&gc_ref).cloned())
        }
    }

    #[test]
    fn root_delta_promotes_known_non_persistent_gc_ref() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x730, vec![ObjectValue::I32(30)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(730));
        state.stage_global(0, GlobalSnapshot::GcRef(0x730)).unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let delta = state.staged_persistent_root_delta(&objects).unwrap();
        let promoted = state.promoted_object_for_test(source).unwrap();

        assert_eq!(
            delta
                .roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0
                })
                .cloned()
                .unwrap(),
            object_set([promoted])
        );
    }

    #[test]
    fn transaction_ref_handle_roots_are_not_promoted_as_ordinary_gc_refs() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let root = objects
            .allocate_persistent_struct_for_gc_ref(0x737, vec![ObjectValue::I32(77)])
            .unwrap();
        let handle = objects.transaction_ref_handle_for_object_id(root).unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(737));

        state
            .stage_global(0, GlobalSnapshot::GcRef(handle))
            .unwrap();
        state
            .stage_table_element_owned(None, 2, 3, TableElementSnapshot::GcRef(handle))
            .unwrap();

        assert!(
            !state
                .promote_persistent_references_before_commit(&mut objects)
                .unwrap()
        );
        let delta = state.staged_persistent_root_delta(&objects).unwrap();

        assert_eq!(
            delta
                .roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0,
                })
                .cloned()
                .unwrap(),
            object_set([root])
        );
        assert_eq!(
            delta
                .roots
                .get(&PersistentRootKey::TableElement(TableElementKey {
                    instance: None,
                    table_index: 2,
                    element_index: 3,
                }))
                .cloned()
                .unwrap(),
            object_set([root])
        );
    }

    #[test]
    fn root_delta_promotes_adapter_struct_gc_ref() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default().with_source(
            0x900,
            OrdinaryGcPromotionSource::Struct {
                type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
                fields: vec![OrdinaryGcPromotionValue::I32(90)],
            },
        );
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(900));
        state.stage_global(0, GlobalSnapshot::GcRef(0x900)).unwrap();

        state
            .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
            .unwrap();
        let promoted = state.promoted_gc_ref_for_test(0x900).unwrap();
        let delta = state.staged_persistent_root_delta(&objects).unwrap();

        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(90)])
        );
        assert_eq!(
            delta
                .roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0
                })
                .cloned()
                .unwrap(),
            object_set([promoted])
        );
    }

    #[test]
    fn live_transaction_ref_promotion_returns_tref_lockable_object() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default().with_source(
            0x902,
            OrdinaryGcPromotionSource::Struct {
                type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
                fields: vec![OrdinaryGcPromotionValue::I64(901)],
            },
        );
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(901));

        let promoted = state
            .promote_gc_ref_for_live_transaction_ref_with_adapter(&mut objects, 0x902, &mut adapter)
            .unwrap()
            .unwrap();
        let handle = objects
            .transaction_ref_handle_for_object_id(promoted)
            .unwrap();

        assert_eq!(state.promoted_gc_ref_for_test(0x902), Some(promoted));
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I64(901)])
        );
        assert!(
            state
                .acquire_tref_read_for_transaction_ref_handle(&mut objects, handle)
                .unwrap()
        );
        assert!(state.owns_object_read(promoted));
        assert!(!state.owns_object_write(promoted));
    }

    #[test]
    fn adapter_promotion_rewrites_nested_refs_and_inline_i31_leaves() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let raw_i31 = ObjectTable::encode_raw_i31_ref(17) as u32;
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default()
            .with_source(
                0x910,
                OrdinaryGcPromotionSource::Struct {
                    type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
                    fields: vec![
                        OrdinaryGcPromotionValue::GcRef(Some(0x912)),
                        OrdinaryGcPromotionValue::GcRef(Some(0x912)),
                        OrdinaryGcPromotionValue::GcRef(Some(raw_i31)),
                    ],
                },
            )
            .with_source(
                0x912,
                OrdinaryGcPromotionSource::Array {
                    type_layout_id: type_layout::TypeLayoutId::DEFAULT_ARRAY,
                    elements: vec![OrdinaryGcPromotionValue::I64(91)],
                },
            );
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(910));
        state.stage_global(0, GlobalSnapshot::GcRef(0x910)).unwrap();

        state
            .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
            .unwrap();

        let promoted_root = state.promoted_gc_ref_for_test(0x910).unwrap();
        let promoted_child = state.promoted_gc_ref_for_test(0x912).unwrap();
        assert_ne!(promoted_root, promoted_child);
        assert_eq!(
            state.staged_object_payload_for_test(promoted_root).unwrap(),
            &ObjectPayload::Struct(vec![
                ObjectValue::Ref(Some(promoted_child)),
                ObjectValue::Ref(Some(promoted_child)),
                ObjectValue::I31(17),
            ])
        );
        assert_eq!(
            state
                .staged_object_payload_for_test(promoted_child)
                .unwrap(),
            &ObjectPayload::Array(vec![ObjectValue::I64(91)])
        );
    }

    #[test]
    fn adapter_promotion_preserves_self_cycles() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default().with_source(
            0x930,
            OrdinaryGcPromotionSource::Struct {
                type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
                fields: vec![OrdinaryGcPromotionValue::GcRef(Some(0x930))],
            },
        );
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(930));
        state.stage_global(0, GlobalSnapshot::GcRef(0x930)).unwrap();

        state
            .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
            .unwrap();

        let promoted = state.promoted_gc_ref_for_test(0x930).unwrap();
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted))])
        );
    }

    #[test]
    fn adapter_promotion_remembers_top_level_durable_leaf_refs() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let func = DurableFuncIdentity {
            module_fingerprint: 0x940,
            function_index: 1,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default()
            .with_source(0x940, OrdinaryGcPromotionSource::FuncRef(func));
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(940));
        state.stage_global(0, GlobalSnapshot::GcRef(0x940)).unwrap();

        state
            .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
            .unwrap();
        let root_delta = state.staged_persistent_root_delta(&objects).unwrap();
        let gc_delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();

        assert_eq!(state.promoted_gc_ref_for_test(0x940), None);
        assert!(
            root_delta
                .roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0,
                })
                .unwrap()
                .is_empty()
        );
        assert!(gc_delta.new_roots.is_empty());
    }

    #[test]
    fn adapter_promotion_rolls_back_on_unsupported_child() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let mut adapter = FakeOrdinaryGcPromotionAdapter::default()
            .with_source(
                0x920,
                OrdinaryGcPromotionSource::Struct {
                    type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
                    fields: vec![OrdinaryGcPromotionValue::GcRef(Some(0x922))],
                },
            )
            .with_source(
                0x922,
                OrdinaryGcPromotionSource::Unsupported("host opaque object"),
            );
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(920));
        state.stage_global(0, GlobalSnapshot::GcRef(0x920)).unwrap();

        let err = state
            .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("ordinary Wasmtime GC reference cannot be promoted: host opaque object"),
            "{err:?}"
        );
        assert_eq!(state.promoted_gc_ref_for_test(0x920), None);
        assert_eq!(state.promoted_gc_ref_for_test(0x922), None);
        assert_eq!(state.allocated_object_count_for_test(), 0);
        assert_eq!(objects.live_count(), 0);
    }

    #[test]
    fn root_delta_ignores_inline_i31_gc_ref_immediate() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let raw_i31 = ObjectTable::encode_raw_i31_ref(19) as u32;
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(732));
        state
            .stage_global(0, GlobalSnapshot::GcRef(raw_i31))
            .unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let delta = state.staged_persistent_root_delta(&objects).unwrap();

        assert!(
            delta
                .roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0
                })
                .cloned()
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[test]
    fn persistent_owner_payload_ref_promotes_child_before_publication() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let child = objects
            .allocate_struct_for_gc_ref(0x731, vec![ObjectValue::I32(31)])
            .unwrap();
        let owner = objects
            .allocate_persistent_struct_for_gc_ref(0x732, vec![ObjectValue::Ref(None)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(731));
        state.acquire_object_write(&mut objects, owner).unwrap();
        state
            .stage_struct_field(&objects, owner, 0, ObjectValue::Ref(Some(child)))
            .unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let promoted_child = state.promoted_object_for_test(child).unwrap();

        assert_eq!(
            state.staged_object_payload_for_test(owner).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_child))])
        );
        assert_eq!(
            state
                .staged_object_payload_for_test(promoted_child)
                .unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(31)])
        );
    }

    #[test]
    fn persistent_gc_delta_uses_promoted_root_after_precommit() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x734, vec![ObjectValue::I32(34)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(734));
        state.stage_global(0, GlobalSnapshot::GcRef(0x734)).unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let promoted = state.promoted_object_for_test(source).unwrap();
        let delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();

        assert_eq!(delta.new_roots, object_set([promoted]));
    }

    #[test]
    fn promotion_registers_source_object_read_for_validation() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x735, vec![ObjectValue::I32(35)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(735));
        state.stage_global(0, GlobalSnapshot::GcRef(0x735)).unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        objects
            .update_payload(source, ObjectPayload::Struct(vec![ObjectValue::I32(36)]))
            .unwrap();

        let error = state.commit_object_payloads(&mut objects).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("optimistic read version changed")
        );
    }
}

mod persistent_promotion_abort {
    use super::*;

    #[test]
    fn abort_frees_promoted_objects_and_clears_promotion_map() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x736, vec![ObjectValue::I32(36)])
            .unwrap();
        let raw_i31 = ObjectTable::encode_raw_i31_ref(37) as u32;
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(736));
        state.stage_global(0, GlobalSnapshot::GcRef(0x736)).unwrap();
        state
            .stage_global(1, GlobalSnapshot::GcRef(raw_i31))
            .unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();

        let promoted_object = state.promoted_object_for_test(source).unwrap();

        assert_eq!(state.allocated_object_count_for_test(), 1);
        assert!(
            state
                .staged_object_payload_for_test(promoted_object)
                .is_some()
        );

        state.abort_allocated_objects(&mut objects).unwrap();

        assert_eq!(state.active_transaction(), None);
        assert_eq!(state.promoted_object_for_test(source), None);
        assert!(
            state
                .staged_object_payload_for_test(promoted_object)
                .is_none()
        );
        assert_eq!(state.allocated_object_count_for_test(), 0);
        assert!(objects.kind(promoted_object).is_err());
    }
}

mod inline_durable_reference_values {
    use super::*;

    #[test]
    fn inline_i31_func_and_extern_values_round_trip_through_struct_payload() {
        let func = DurableFuncIdentity {
            module_fingerprint: 0x10_20_30_40_50_60_70_80,
            function_index: 7,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let extern_ = DurableExternIdentity {
            namespace: 4,
            handle: 0xabc,
            type_layout_id: type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let payload = ObjectPayload::Struct(vec![
            ObjectValue::I31(-7),
            ObjectValue::FuncRef(func),
            ObjectValue::ExternRef(extern_),
        ]);
        let encoded = encode_object_record_for_test(
            10,
            1,
            type_layout::TypeLayoutId::DEFAULT_STRUCT.get(),
            &payload,
        )
        .unwrap();

        let mut heap = object_heap::ObjectHeap::default();
        let handle = heap.install_record_bytes(&encoded).unwrap();

        assert_eq!(heap.payload(handle).unwrap(), &payload);
    }

    #[test]
    fn inline_value_object_kinds_are_rejected_as_standalone_objects() {
        let mut objects = ObjectTable::default();

        for kind in [ObjectKind::I31, ObjectKind::Func, ObjectKind::Extern] {
            let err = objects.allocate(kind).unwrap_err();
            assert!(
                err.to_string()
                    .contains("durable values, not object-table payloads"),
                "{err:?}"
            );
        }
    }
}

fn sample_region_with_two_object_winners() -> crate::runtime::vm::block_region::VMemoryBlockRegion {
    let mut region =
        crate::runtime::vm::block_region::VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream1 = region.alloc_stream(1).unwrap();
    let stream2 = region.alloc_stream(2).unwrap();
    region
        .append_type_layout_metadata(&recovery_test_struct_layout(7))
        .unwrap();
    region
        .append_type_layout_metadata(&recovery_test_array_layout(9))
        .unwrap();

    let first = encoded_object_publication_for_recovery_test(
        41,
        1,
        7,
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
    );
    let second = encoded_object_publication_for_recovery_test(
        42,
        1,
        9,
        ObjectPayload::Array(vec![
            ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
            ObjectValue::I64(9),
        ]),
    );

    append_committed_object_winner(&mut region, 1, stream1, 0, &first);
    append_committed_object_winner(&mut region, 2, stream2, 0, &second);

    region
}

fn recovery_test_struct_layout(layout_id: u32) -> type_layout::PersistentTypeLayout {
    type_layout::PersistentTypeLayout::Struct {
        id: type_layout::TypeLayoutId::new(layout_id).unwrap(),
        fingerprint: 0x5354_5255_4354_2000 | u64::from(layout_id),
        body_size: 40,
        fields: vec![
            type_layout::StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 4,
                kind: type_layout::TraceSlotKind::Scalar,
            },
            type_layout::StructTraceField {
                field_index: 1,
                field_offset: 20,
                value_size: 20,
                kind: type_layout::TraceSlotKind::ObjectRef,
            },
        ],
    }
}

fn recovery_test_array_layout(layout_id: u32) -> type_layout::PersistentTypeLayout {
    type_layout::PersistentTypeLayout::Array {
        id: type_layout::TypeLayoutId::new(layout_id).unwrap(),
        fingerprint: 0x4152_5241_5900_2000 | u64::from(layout_id),
        element_size: 20,
        element_kind: type_layout::TraceSlotKind::ObjectRef,
    }
}

fn encoded_object_publication_for_recovery_test(
    object_id: u64,
    version: u32,
    type_layout_id: u32,
    payload: ObjectPayload,
) -> persist::PendingPublication {
    persist::PendingPublication::persistent_object(
        match payload.kind() {
            ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
            ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
            _ => unreachable!(),
        },
        object_id,
        version,
        type_layout_id,
        encode_object_record_for_test(object_id, version, type_layout_id, &payload).unwrap(),
    )
    .unwrap()
}

fn recovered_object_winner_with_location_for_test(
    object_id: u64,
    version: u32,
    kind: u16,
    type_layout_id: u32,
    data_block: u32,
    data_offset: u32,
    record_bytes: Vec<u8>,
) -> crate::runtime::vm::RecoveredObjectWinner {
    let record_len = u64::try_from(record_bytes.len()).unwrap();
    crate::runtime::vm::RecoveredObjectWinner {
        object_id,
        version,
        kind,
        type_layout_id,
        data_block,
        data_offset,
        record_len,
        record_bytes,
    }
}

fn recovered_object_winner_for_test(
    object_id: u64,
    version: u32,
    kind: u16,
    type_layout_id: u32,
    record_bytes: Vec<u8>,
) -> crate::runtime::vm::RecoveredObjectWinner {
    recovered_object_winner_with_location_for_test(
        object_id,
        version,
        kind,
        type_layout_id,
        1,
        0,
        record_bytes,
    )
}

fn recovered_record_location_for_test(
    winner: &crate::runtime::vm::RecoveredObjectWinner,
) -> object_gc::PersistentRecoveredRecordLocation {
    let durable_record_len = winner
        .record_len
        .checked_add(u64::try_from(mem::size_of::<TxDataRecordHeader>()).unwrap())
        .unwrap();
    object_gc::PersistentRecoveredRecordLocation {
        object_id: ObjectId {
            object_index: winner.object_id,
        },
        version: winner.version,
        data_block: winner.data_block,
        data_offset: winner.data_offset,
        record_len: durable_record_len,
    }
}

fn append_committed_object_winner(
    region: &mut crate::runtime::vm::block_region::VMemoryBlockRegion,
    stream_id: u32,
    stream: crate::runtime::vm::block_region::StreamCursor,
    block_seq: u32,
    publication: &persist::PendingPublication,
) {
    let record = TMemory::encode_publication_data_record(
        publication.logical_id,
        publication.version,
        publication.kind,
        publication.type_layout_id,
        &publication.payload,
    )
    .unwrap();
    let location = region.append_data_record(stream, &record).unwrap();
    let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
    let entry = TMemory::publication_log_entry(
        publication.logical_id,
        publication.version,
        stream_id << 1,
        location.data_block,
        location.data_offset,
        0,
        true,
    )
    .unwrap();
    write_log_entry_for_recovery_test(region, log_block, entry);
}

fn write_log_entry_for_recovery_test(
    region: &mut crate::runtime::vm::block_region::VMemoryBlockRegion,
    start_block: u32,
    entry: crate::runtime::vm::TxLogEntry,
) {
    let mut header = region.log_block_header(start_block).unwrap();
    header.entry_count = 1;
    let block_offset =
        usize::try_from(start_block).unwrap() * crate::runtime::vm::block_region::BLOCK_SIZE;
    region.write(block_offset, &header.as_bytes()).unwrap();
    let offset = block_offset + header.as_bytes().len();
    region
        .write(offset, &encode_tx_log_entry_for_recovery_test(entry))
        .unwrap();
}

fn encode_tx_log_entry_for_recovery_test(entry: crate::runtime::vm::TxLogEntry) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&entry.logical_id.to_le_bytes());
    bytes[8..12].copy_from_slice(&entry.version.to_le_bytes());
    bytes[12..16].copy_from_slice(&entry.tx_meta.to_le_bytes());
    bytes[16..20].copy_from_slice(&entry.data_block.to_le_bytes());
    bytes[20..24].copy_from_slice(&entry.data_offset.to_le_bytes());
    bytes[24..28].copy_from_slice(&entry.crc32.to_le_bytes());
    bytes[28..32].copy_from_slice(&entry.entry_meta.to_le_bytes());
    bytes
}

#[test]
fn transaction_object_trace_descriptor_distinguishes_scalar_arrays() {
    let mut heap = object_heap::ObjectHeap::default();
    let handle = heap
        .allocate_record(
            ObjectId { object_index: 3 },
            7,
            ObjectKind::Array,
            0,
            8,
            &ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)]),
        )
        .unwrap();

    assert_eq!(
        heap.trace_descriptor(handle).unwrap(),
        object_heap::TraceDescriptor::Array(object_heap::TraceArrayDescriptor {
            element_kind: object_heap::TraceValueKind::Scalar,
            length: 2,
        })
    );
}

#[test]
fn object_table_reports_embedded_refs_from_current_record() {
    let mut objects = ObjectTable::default();
    let first = ObjectId { object_index: 8 };
    let second = ObjectId { object_index: 9 };
    let object = objects
        .allocate_struct(vec![
            ObjectValue::I32(1),
            ObjectValue::Ref(Some(first)),
            ObjectValue::Ref(None),
        ])
        .unwrap();

    assert_eq!(objects.trace_object_ids(object).unwrap(), vec![first]);

    objects
        .update_payload(
            object,
            ObjectPayload::Struct(vec![
                ObjectValue::Ref(Some(second)),
                ObjectValue::I64(3),
                ObjectValue::Ref(Some(first)),
            ]),
        )
        .unwrap();

    assert_eq!(
        objects.trace_object_ids(object).unwrap(),
        vec![second, first]
    );
}

#[test]
fn persistent_default_struct_layout_tracing_falls_back_to_payload_refs() {
    let mut objects = ObjectTable::default();
    let first = ObjectId { object_index: 8 };
    let second = ObjectId { object_index: 9 };
    let object = objects
        .allocate_persistent_struct_for_gc_ref(
            0x401,
            vec![
                ObjectValue::I32(1),
                ObjectValue::Ref(Some(first)),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();

    assert_eq!(
        objects.trace_object_ids(object).unwrap(),
        vec![first, second]
    );
}

#[test]
fn persistent_default_array_layout_tracing_falls_back_to_payload_refs() {
    let mut objects = ObjectTable::default();
    let first = ObjectId { object_index: 18 };
    let second = ObjectId { object_index: 19 };
    let object = objects
        .allocate_persistent_array_for_gc_ref(
            0x402,
            vec![
                ObjectValue::Ref(None),
                ObjectValue::Ref(Some(first)),
                ObjectValue::I64(7),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();

    assert_eq!(
        objects.trace_object_ids(object).unwrap(),
        vec![first, second]
    );
}

#[test]
fn object_recovery_reference_graph_uses_layout_metadata_for_persistent_objects() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(type_layout::PersistentTypeLayout::Struct {
            id: type_layout::TypeLayoutId::new(207).unwrap(),
            fingerprint: 0x5354_5255_4354_0207,
            body_size: 40,
            fields: vec![
                type_layout::StructTraceField {
                    field_index: 0,
                    field_offset: 0,
                    value_size: 8,
                    kind: type_layout::TraceSlotKind::Scalar,
                },
                type_layout::StructTraceField {
                    field_index: 1,
                    field_offset: 20,
                    value_size: 20,
                    kind: type_layout::TraceSlotKind::ObjectRef,
                },
            ],
        })
        .unwrap();

    let first = ObjectId { object_index: 8 };
    let second = ObjectId { object_index: 9 };
    let mut rebuilt = ObjectTable::default();
    rebuilt
        .rebuild_from_recovery_for_test(
            &recovered_type_layouts,
            &[recovered_object_winner_for_test(
                41,
                3,
                ObjectKind::Struct as u16,
                207,
                encode_object_record_for_test(
                    41,
                    3,
                    207,
                    &ObjectPayload::Struct(vec![
                        ObjectValue::Ref(Some(first)),
                        ObjectValue::Ref(Some(second)),
                    ]),
                )
                .unwrap(),
            )],
        )
        .unwrap();

    let object = ObjectId { object_index: 41 };
    assert_eq!(
        rebuilt.payload(object).unwrap(),
        ObjectPayload::Struct(vec![
            ObjectValue::Ref(Some(first)),
            ObjectValue::Ref(Some(second)),
        ])
    );
    assert_eq!(rebuilt.trace_object_ids(object).unwrap(), vec![second]);
}

#[test]
fn persistent_object_marker_marks_root_closure_and_reports_unreachable() {
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref(0x501, vec![ObjectValue::I32(3)])
        .unwrap();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(
            0x502,
            vec![ObjectValue::Ref(Some(leaf)), ObjectValue::I64(2)],
        )
        .unwrap();
    let root = objects
        .allocate_persistent_array_for_gc_ref(
            0x503,
            vec![ObjectValue::Ref(Some(child)), ObjectValue::I32(1)],
        )
        .unwrap();
    let unreachable = objects
        .allocate_persistent_struct_for_gc_ref(0x504, vec![ObjectValue::I32(9)])
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();

    assert_eq!(report.reachable, object_set([root, child, leaf]));
    assert_eq!(report.unreachable_persistent, object_set([unreachable]));
    assert!(report.invalid_roots.is_empty());
    assert!(report.dangling_refs.is_empty());
}

#[test]
fn persistent_gc_migrated_linked_list_survives_incremental_collection() {
    let mut objects = ObjectTable::default();
    let list = allocate_persistent_linked_list_for_migrated_gc_test(&mut objects, 50, 0x900);
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x950, vec![ObjectValue::I32(950)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([list]).unwrap();

    state
        .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(7))
        .unwrap();
    let report = state
        .finish_persistent_gc_cycle_and_sweep_for_test(&mut objects)
        .unwrap()
        .expect("maintenance cycle should have started");

    assert_eq!(persistent_linked_list_sum_for_test(&objects, list), 1225);
    assert!(report.mark.reachable.contains(&list));
    assert_eq!(report.mark.reachable.len(), 50);
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(garbage).is_err());
}

#[test]
fn persistent_gc_migrated_deep_struct_chain_survives_maintenance_steps() {
    let mut objects = ObjectTable::default();
    let chain = allocate_persistent_linked_list_for_migrated_gc_test(&mut objects, 16, 0x960);
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x980, vec![ObjectValue::I32(980)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([chain]).unwrap();

    for _ in 0..8 {
        state
            .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(1))
            .unwrap();
    }

    let report = state
        .finish_persistent_gc_cycle_and_sweep_for_test(&mut objects)
        .unwrap()
        .expect("maintenance cycle should have started");

    assert_eq!(persistent_linked_list_sum_for_test(&objects, chain), 120);
    assert_eq!(report.mark.reachable.len(), 16);
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(garbage).is_err());
}

#[test]
fn persistent_gc_migrated_binary_tree_marks_complete_closure() {
    let mut objects = ObjectTable::default();
    let mut next_gc_ref = 0x990;
    let tree =
        allocate_persistent_binary_tree_for_migrated_gc_test(&mut objects, 5, 1, &mut next_gc_ref)
            .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x9c0, vec![ObjectValue::I32(404)])
        .unwrap();

    let mark = PersistentObjectMarker::mark(&objects, [tree]).unwrap();

    assert_eq!(
        persistent_binary_tree_sum_for_test(&objects, Some(tree)),
        496
    );
    assert_eq!(mark.reachable.len(), 31);
    assert!(mark.reachable.contains(&tree));
    assert_eq!(mark.unreachable_persistent, object_set([garbage]));
    assert!(mark.invalid_roots.is_empty());
    assert!(mark.dangling_refs.is_empty());
}

#[test]
fn persistent_gc_migrated_array_refs_trace_live_elements_only() {
    let mut objects = ObjectTable::default();
    let first = objects
        .allocate_persistent_struct_for_gc_ref(0x9d0, vec![ObjectValue::I32(1)])
        .unwrap();
    let second = objects
        .allocate_persistent_struct_for_gc_ref(0x9d1, vec![ObjectValue::I32(2)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x9d2, vec![ObjectValue::I32(3)])
        .unwrap();
    let root = objects
        .allocate_persistent_array_for_gc_ref(
            0x9d3,
            vec![
                ObjectValue::I32(10),
                ObjectValue::Ref(None),
                ObjectValue::Ref(Some(first)),
                ObjectValue::I64(20),
                ObjectValue::Ref(Some(second)),
            ],
        )
        .unwrap();

    let mark = PersistentObjectMarker::mark(&objects, [root]).unwrap();

    assert_eq!(mark.reachable, object_set([root, first, second]));
    assert_eq!(mark.unreachable_persistent, object_set([garbage]));
    assert!(mark.invalid_roots.is_empty());
    assert!(mark.dangling_refs.is_empty());
}

#[test]
fn persistent_gc_migrated_pressure_sweeps_unrooted_objects() {
    let mut objects = ObjectTable::default();
    let mut roots = Vec::new();
    let mut garbage = Vec::new();
    for index in 0..128u32 {
        let object = objects
            .allocate_persistent_struct_for_gc_ref(
                0xa00 + index,
                vec![ObjectValue::I32(i32::try_from(index).unwrap())],
            )
            .unwrap();
        if index % 4 == 0 {
            roots.push(object);
        } else {
            garbage.push(object);
        }
    }

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state
        .install_recovered_persistent_roots(roots.iter().copied())
        .unwrap();

    let report = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();

    assert_eq!(
        report
            .sweep
            .retained_objects
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        roots.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(report.sweep.removed_objects, garbage);
    assert_eq!(objects.live_count(), roots.len());
}

#[test]
fn file_backed_persistent_gc_migrated_graph_recovers_only_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let tx_log_path = dir.path().join("persistent-gc-migrated-graph.bin");
    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
    let mut state = TransactionState::new_for_test_with_durable_log(
        TransactionId::from_raw(0xa80),
        durable_log,
    );
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0xa800,
            0xa80,
            1,
            vec![ObjectValue::I32(7)],
        )
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0xa801,
            0xa80,
            2,
            vec![ObjectValue::Ref(Some(leaf)), ObjectValue::I32(11)],
        )
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0xa802,
            0xa80,
            3,
            vec![ObjectValue::I32(404)],
        )
        .unwrap();

    state.acquire_object_write(&mut objects, leaf).unwrap();
    state
        .stage_struct_field(&objects, leaf, 0, ObjectValue::I32(70))
        .unwrap();
    state.acquire_object_write(&mut objects, root).unwrap();
    state
        .stage_struct_field(&objects, root, 1, ObjectValue::I32(110))
        .unwrap();
    state.acquire_object_write(&mut objects, garbage).unwrap();
    state
        .stage_struct_field(&objects, garbage, 0, ObjectValue::I32(405))
        .unwrap();
    state
        .stage_global(0, GlobalSnapshot::GcRef(0xa801))
        .unwrap();
    commit_active_file_backed_publications_for_test(0xa80, 0xa80, &mut objects, &mut state)
        .unwrap();
    drop(state);

    let (recovered, object_winners) =
        recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(
            &recovered.type_layouts,
            &object_winners,
            &recovered.root_object_ids,
        )
        .unwrap();

    assert_eq!(report.mark.reachable, object_set([root, leaf]));
    assert_eq!(report.mark.unreachable_persistent, object_set([garbage]));
    assert_eq!(
        rebuilt.payload(root).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::Ref(Some(leaf)), ObjectValue::I32(110)])
    );
    assert_eq!(
        rebuilt.payload(leaf).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(70)])
    );
    assert!(rebuilt.payload(garbage).is_err());
}

#[test]
fn persistent_object_marker_budgeted_state_tracks_partial_progress() {
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref(0x505, vec![ObjectValue::I32(5)])
        .unwrap();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x506, vec![ObjectValue::Ref(Some(leaf))])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x507, vec![ObjectValue::Ref(Some(child))])
        .unwrap();
    let unreachable = objects
        .allocate_persistent_struct_for_gc_ref(0x508, vec![ObjectValue::I32(8)])
        .unwrap();

    let mut state = PersistentGcState::new(&objects, [root]).unwrap();

    assert_eq!(
        state
            .mark_step(&objects, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert!(!state.is_complete());

    assert_eq!(
        state
            .mark_step(&objects, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert!(!state.is_complete());

    assert_eq!(
        state
            .mark_step(&objects, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
    assert!(state.is_complete());

    let report = state.into_report(&objects).unwrap();

    assert_eq!(report.reachable, object_set([root, child, leaf]));
    assert_eq!(report.unreachable_persistent, object_set([unreachable]));
    assert!(report.invalid_roots.is_empty());
    assert!(report.dangling_refs.is_empty());
}

#[test]
fn persistent_gc_commit_barrier_enqueues_new_root() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x540, vec![ObjectValue::I32(4)])
        .unwrap();

    let delta = {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(540));
        state.stage_global(0, GlobalSnapshot::GcRef(0x540)).unwrap();
        state.persistent_gc_commit_delta(&objects, &[]).unwrap()
    };

    assert_eq!(
        delta,
        PersistentGcCommitDelta {
            new_roots: object_set([root]),
            edges: BTreeSet::new(),
            invalidates_reachable_cache: true,
        }
    );

    let mut state = PersistentGcState::new(&objects, []).unwrap();
    assert_eq!(
        state
            .observe_commit_delta(&objects, &delta, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
    assert!(state.is_complete());
    assert_eq!(
        state.into_report(&objects).unwrap().reachable,
        object_set([root])
    );
}

#[test]
fn persistent_root_index_tracks_committed_global_root() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x590, vec![ObjectValue::I32(9)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(590));
    state.stage_global(0, GlobalSnapshot::GcRef(0x590)).unwrap();

    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();

    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap();

    assert_eq!(state.persistent_root_ids_for_test(), object_set([root]));
}

#[test]
fn committed_persistent_root_is_visible_to_another_store_using_same_region_runtime() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x597, vec![ObjectValue::I32(16)])
        .unwrap();

    let runtime = TransactionRegionRuntime::new_for_test();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();

    let mut publisher = TransactionState::new_for_test(TransactionId::from_raw(597));
    publisher.shared_region_runtime = Some(runtime.clone());
    publisher
        .stage_global(0, GlobalSnapshot::GcRef(0x597))
        .unwrap();
    let delta = publisher
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    publisher.persistent_root_publications(&delta).unwrap();
    publisher
        .complete_commit_with_persistent_root_delta_for_test(delta)
        .unwrap();

    let observer = TransactionState {
        shared_region_runtime: Some(runtime.clone()),
        ..TransactionState::default()
    };
    assert_eq!(observer.persistent_root_ids_for_test(), object_set([root]));
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([root])
    );
}

#[test]
fn persistent_root_publication_versions_are_shared_across_states_using_same_region_runtime() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x598, vec![ObjectValue::I32(17)])
        .unwrap();
    objects
        .allocate_persistent_struct_for_gc_ref(0x599, vec![ObjectValue::I32(18)])
        .unwrap();

    let runtime = TransactionRegionRuntime::new_for_test();
    let root_key = PersistentRootKey::Global {
        instance: None,
        global_index: 0,
    };
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();

    let mut first = TransactionState::new_for_test(TransactionId::from_raw(598));
    first.shared_region_runtime = Some(runtime.clone());
    first.stage_global(0, GlobalSnapshot::GcRef(0x598)).unwrap();
    let first_delta = first
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    let first_publications = first.persistent_root_publications(&first_delta).unwrap();
    assert_eq!(first_publications[0].version, 1);
    first
        .complete_commit_with_persistent_root_delta_for_test(first_delta)
        .unwrap();

    let mut second = TransactionState::new_for_test(TransactionId::from_raw(599));
    second.shared_region_runtime = Some(runtime.clone());
    second
        .stage_global(0, GlobalSnapshot::GcRef(0x599))
        .unwrap();
    let second_delta = second
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    let second_publications = second.persistent_root_publications(&second_delta).unwrap();
    assert_eq!(second_publications[0].version, 2);
    assert_eq!(
        runtime.persistent_root_version_for_test(root_key).unwrap(),
        Some(2)
    );
}

#[test]
fn shared_region_runtime_reserves_distinct_root_versions_before_either_state_applies() {
    let runtime = TransactionRegionRuntime::new_for_test();
    let root_key = PersistentRootKey::Global {
        instance: None,
        global_index: 0,
    };
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let first_delta = state::PersistentRootDelta {
        roots: BTreeMap::from([(root_key, object_set([ObjectId { object_index: 91 }]))]),
    };
    let second_delta = state::PersistentRootDelta {
        roots: BTreeMap::from([(root_key, object_set([ObjectId { object_index: 92 }]))]),
    };

    let mut first = TransactionState::new_for_test(TransactionId::from_raw(602));
    first.shared_region_runtime = Some(runtime.clone());
    let first_publications = first.persistent_root_publications(&first_delta).unwrap();

    let mut second = TransactionState::new_for_test(TransactionId::from_raw(603));
    second.shared_region_runtime = Some(runtime.clone());
    let second_publications = second.persistent_root_publications(&second_delta).unwrap();

    assert_eq!(first_publications[0].version, 1);
    assert_eq!(second_publications[0].version, 2);
    assert_ne!(
        first_publications[0].version,
        second_publications[0].version
    );
    assert_eq!(
        runtime.persistent_root_version_for_test(root_key).unwrap(),
        Some(2)
    );
}

#[test]
fn shared_region_runtime_applies_committed_roots_before_transaction_authority_is_released() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x59d, vec![ObjectValue::I32(22)])
        .unwrap();

    let runtime = TransactionRegionRuntime::new_for_test();
    let transaction = TransactionId::from_raw(604);
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();

    let mut state = TransactionState::new_for_test(transaction);
    state.shared_region_runtime = Some(runtime.clone());
    state.stage_global(0, GlobalSnapshot::GcRef(0x59d)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.persistent_root_publications(&delta).unwrap();
    state.begin_terminal_commit().unwrap();
    state
        .commit_shared_persistent_root_delta_before_complete_commit(delta)
        .unwrap();

    let observer = TransactionState {
        shared_region_runtime: Some(runtime.clone()),
        ..TransactionState::default()
    };
    assert_eq!(observer.persistent_root_ids_for_test(), object_set([root]));
    assert_eq!(state.active, Some(transaction));
    assert!(state.terminal_commit_active);
    assert!(
        runtime
            .transaction_is_terminal_commit_for_test(transaction)
            .unwrap()
    );

    state.clear_active().unwrap();
}

#[test]
fn shared_root_apply_failure_after_lp_keeps_committed_allocated_object_and_clears_state() {
    clear_current_thread_transaction_for_test();

    let runtime = TransactionRegionRuntime::new_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x59e, vec![ObjectValue::I32(22)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let transaction = TransactionId::from_raw(605);
    let mut state = TransactionState::new_for_test(transaction);
    state.shared_region_runtime = Some(runtime.clone());
    state.record_allocated_object(object).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&objects, object, 0, ObjectValue::I32(23))
        .unwrap();
    state.stage_global(0, GlobalSnapshot::GcRef(0x59e)).unwrap();

    let mut publications = Vec::new();
    state
        .commit_object_payloads_into(&mut objects, &mut publications)
        .unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    publications.extend(state.persistent_root_publications(&delta).unwrap());

    let stream_id = runtime
        .current_thread_log_segment_for_test()
        .unwrap()
        .stream_id();
    let txid = u32::try_from(state.active_transaction_required_raw().unwrap()).unwrap();
    let marker = state
        .publish_object_publications_before_commit(stream_id, txid, &objects, &publications)
        .unwrap()
        .unwrap();
    state.publish_commit_lp(stream_id, txid, marker).unwrap();
    state.begin_terminal_commit().unwrap();

    runtime.fail_apply_persistent_root_delta_once_for_test();
    let error = state
        .complete_commit_with_persistent_root_delta_for_test(delta)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected shared persistent root apply failure"),
        "{error:?}"
    );

    state
        .finish_committed_cleanup_after_durable_commit_error_for_test()
        .unwrap();
    assert_eq!(state.active_transaction(), None);
    assert!(!state.terminal_commit_active);
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(state.allocated_object_count_for_test(), 0);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(23)])
    );
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([])
    );

    clear_current_thread_transaction_for_test();
}

#[test]
fn shared_root_apply_failure_can_retry_with_original_reserved_version() {
    let runtime = TransactionRegionRuntime::new_for_test();
    let root_key = PersistentRootKey::Global {
        instance: None,
        global_index: 0,
    };
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x59f, vec![ObjectValue::I32(24)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(606));
    state.shared_region_runtime = Some(runtime.clone());
    state.stage_global(0, GlobalSnapshot::GcRef(0x59f)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    let publications = state.persistent_root_publications(&delta).unwrap();
    assert_eq!(publications[0].version, 1);

    state.begin_terminal_commit().unwrap();
    runtime.fail_apply_persistent_root_delta_once_for_test();
    let error = state
        .commit_shared_persistent_root_delta_before_complete_commit(delta.clone())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected shared persistent root apply failure"),
        "{error:?}"
    );

    state
        .commit_shared_persistent_root_delta_before_complete_commit(delta)
        .unwrap();
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([root])
    );
    assert_eq!(
        runtime.persistent_root_version_for_test(root_key).unwrap(),
        Some(1)
    );

    state.complete_commit().unwrap();
}

#[test]
fn shared_root_apply_ignores_stale_out_of_order_root_metadata() {
    let runtime = TransactionRegionRuntime::new_for_test();
    let root_key = PersistentRootKey::Global {
        instance: None,
        global_index: 0,
    };
    let first_root = ObjectId { object_index: 93 };
    let second_root = ObjectId { object_index: 94 };
    let first_delta = state::PersistentRootDelta {
        roots: BTreeMap::from([(root_key, object_set([first_root]))]),
    };
    let second_delta = state::PersistentRootDelta {
        roots: BTreeMap::from([(root_key, object_set([second_root]))]),
    };

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();

    let mut first = TransactionState::new_for_test(TransactionId::from_raw(607));
    first.shared_region_runtime = Some(runtime.clone());
    let first_publications = first.persistent_root_publications(&first_delta).unwrap();
    assert_eq!(first_publications[0].version, 1);

    let mut second = TransactionState::new_for_test(TransactionId::from_raw(608));
    second.shared_region_runtime = Some(runtime.clone());
    let second_publications = second.persistent_root_publications(&second_delta).unwrap();
    assert_eq!(second_publications[0].version, 2);

    replace_current_thread_transaction(Some(TransactionId::from_raw(608)));
    second.begin_terminal_commit().unwrap();
    second
        .commit_shared_persistent_root_delta_before_complete_commit(second_delta)
        .unwrap();
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([second_root])
    );
    assert_eq!(
        runtime.persistent_root_version_for_test(root_key).unwrap(),
        Some(2)
    );

    replace_current_thread_transaction(Some(TransactionId::from_raw(607)));
    first.begin_terminal_commit().unwrap();
    first
        .commit_shared_persistent_root_delta_before_complete_commit(first_delta)
        .unwrap();
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([second_root])
    );
    assert_eq!(
        runtime.persistent_root_version_for_test(root_key).unwrap(),
        Some(2)
    );

    replace_current_thread_transaction(Some(TransactionId::from_raw(608)));
    second.complete_commit().unwrap();
    replace_current_thread_transaction(Some(TransactionId::from_raw(607)));
    first.complete_commit().unwrap();
}

#[test]
fn shared_region_runtime_root_ids_change_when_roots_are_removed() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x59a, vec![ObjectValue::I32(19)])
        .unwrap();

    let runtime = TransactionRegionRuntime::new_for_test();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();

    let mut installer = TransactionState::new_for_test(TransactionId::from_raw(600));
    installer.shared_region_runtime = Some(runtime.clone());
    installer
        .stage_global(0, GlobalSnapshot::GcRef(0x59a))
        .unwrap();
    let install_delta = installer
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    installer
        .persistent_root_publications(&install_delta)
        .unwrap();
    installer
        .complete_commit_with_persistent_root_delta_for_test(install_delta)
        .unwrap();
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([root])
    );

    let mut remover = TransactionState::new_for_test(TransactionId::from_raw(601));
    remover.shared_region_runtime = Some(runtime.clone());
    remover.stage_global(0, GlobalSnapshot::I32(0)).unwrap();
    let removal_delta = remover
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    remover
        .persistent_root_publications(&removal_delta)
        .unwrap();
    remover
        .complete_commit_with_persistent_root_delta_for_test(removal_delta)
        .unwrap();

    let observer = TransactionState {
        shared_region_runtime: Some(runtime.clone()),
        ..TransactionState::default()
    };
    assert_eq!(observer.persistent_root_ids_for_test(), object_set([]));
    assert_eq!(
        runtime.persistent_root_ids_for_test().unwrap(),
        object_set([])
    );
}

#[test]
fn shared_region_runtime_keeps_live_bridge_maps_store_local() {
    let runtime = TransactionRegionRuntime::new_for_test();
    let mut first_state = TransactionState::default();
    first_state.shared_region_runtime = Some(runtime.clone());
    let mut second_state = TransactionState::default();
    second_state.shared_region_runtime = Some(runtime);

    let mut first_objects = ObjectTable::default();
    let second_objects = ObjectTable::default();
    let object = ObjectId { object_index: 88 };

    first_objects
        .live_bridge_gc_refs_to_objects
        .insert(0x88, object);
    first_objects
        .object_to_live_bridge_gc_ref
        .insert(object, 0x88);
    first_objects
        .transaction_ref_handles_to_objects
        .insert(0x180, object);
    first_objects
        .objects_to_transaction_ref_handles
        .insert(object, 0x180);

    assert_eq!(
        first_objects.live_bridge_gc_refs_to_objects.get(&0x88),
        Some(&object)
    );
    assert_eq!(
        second_objects.live_bridge_gc_refs_to_objects.get(&0x88),
        None
    );
    assert_eq!(
        first_objects.object_to_live_bridge_gc_ref.get(&object),
        Some(&0x88)
    );
    assert_eq!(
        second_objects.object_to_live_bridge_gc_ref.get(&object),
        None
    );
    assert_eq!(
        first_objects.transaction_ref_handles_to_objects.get(&0x180),
        Some(&object)
    );
    assert_eq!(
        second_objects
            .transaction_ref_handles_to_objects
            .get(&0x180),
        None
    );
    assert_eq!(
        first_objects
            .objects_to_transaction_ref_handles
            .get(&object),
        Some(&0x180)
    );
    assert_eq!(
        second_objects
            .objects_to_transaction_ref_handles
            .get(&object),
        None
    );
}

#[test]
fn persistent_root_index_removes_global_root_on_non_ref_commit() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x591, vec![ObjectValue::I32(10)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(591));
    state.stage_global(0, GlobalSnapshot::GcRef(0x591)).unwrap();
    let install_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(install_delta)
        .unwrap();
    assert_eq!(state.persistent_root_ids_for_test(), object_set([root]));

    state.begin().unwrap();
    state.stage_global(0, GlobalSnapshot::I32(0)).unwrap();
    let removal_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(removal_delta)
        .unwrap();

    assert_eq!(state.persistent_root_ids_for_test(), object_set([]));
}

#[test]
fn persistent_root_index_tracks_table_element_roots() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x592, vec![ObjectValue::I32(11)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(592));
    state
        .stage_table_element_owned(None, 3, 0, TableElementSnapshot::GcRef(0x592))
        .unwrap();
    state
        .stage_table_element_owned(None, 3, 1, TableElementSnapshot::GcRef(0))
        .unwrap();

    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();

    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap();

    assert_eq!(state.persistent_root_ids_for_test(), object_set([root]));
}

#[test]
fn persistent_root_index_preserves_untouched_table_root_on_partial_update() {
    let mut objects = ObjectTable::default();
    let root0 = objects
        .allocate_persistent_struct_for_gc_ref(0x593, vec![ObjectValue::I32(12)])
        .unwrap();
    let root1 = objects
        .allocate_persistent_struct_for_gc_ref(0x594, vec![ObjectValue::I32(13)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(593));
    state
        .stage_table_element_owned(None, 3, 0, TableElementSnapshot::GcRef(0x593))
        .unwrap();
    state
        .stage_table_element_owned(None, 3, 1, TableElementSnapshot::GcRef(0x594))
        .unwrap();
    let install_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(install_delta)
        .unwrap();
    assert_eq!(
        state.persistent_root_ids_for_test(),
        object_set([root0, root1])
    );

    state.begin().unwrap();
    state
        .stage_table_element_owned(None, 3, 0, TableElementSnapshot::GcRef(0))
        .unwrap();
    let partial_update_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(partial_update_delta)
        .unwrap();

    assert_eq!(state.persistent_root_ids_for_test(), object_set([root1]));
}

#[test]
fn persistent_root_index_requires_inactive_state_for_apply() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x595, vec![ObjectValue::I32(14)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(595));
    state.stage_global(0, GlobalSnapshot::GcRef(0x595)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();

    let err = state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "committed persistent root delta can only be applied after complete_commit"
    );
}

#[test]
fn persistent_root_index_reports_version_overflow() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x596, vec![ObjectValue::I32(15)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(596));
    state.stage_global(0, GlobalSnapshot::GcRef(0x596)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state.persistent_root_versions.insert(
        PersistentRootKey::Global {
            instance: None,
            global_index: 0,
        },
        u32::MAX,
    );

    let err = state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap_err();
    assert_eq!(err.to_string(), "persistent root version overflow");
}

#[test]
fn transaction_state_persistent_mark_sweep_uses_committed_roots() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x660, vec![ObjectValue::I32(3)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x661, vec![ObjectValue::Ref(Some(child))])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x662, vec![ObjectValue::I32(4)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x660));
    state.stage_global(0, GlobalSnapshot::GcRef(0x661)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap();

    let report = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();

    assert_eq!(report.mark.reachable, object_set([root, child]));
    assert_eq!(
        report
            .sweep
            .retained_objects
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        object_set([root, child])
    );
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(root).is_ok());
    assert!(objects.live_slot(child).is_ok());
    assert!(objects.live_slot(garbage).is_err());
}

#[test]
fn persistent_gc_does_not_consider_aborted_transaction_local_object_persistent() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x680));
    let persistent_child = objects
        .allocate_persistent_struct_for_gc_ref(0x680, vec![ObjectValue::I32(7)])
        .unwrap();
    let local = objects
        .allocate_struct(vec![ObjectValue::Ref(Some(persistent_child))])
        .unwrap();

    state.abort().unwrap();

    let report = objects
        .persistent_mark_sweep_from_roots_for_test([])
        .unwrap();
    assert!(report.mark.reachable.is_empty());
    assert_eq!(
        report.mark.unreachable_persistent,
        object_set([persistent_child])
    );
    assert_eq!(report.sweep.removed_objects, vec![persistent_child]);
    assert!(objects.live_slot(persistent_child).is_err());
    assert!(objects.live_slot(local).is_ok());
}

#[test]
fn persistent_gc_collects_committed_object_only_after_root_is_removed() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x681, vec![ObjectValue::I32(8)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x681));
    state.stage_global(0, GlobalSnapshot::GcRef(0x681)).unwrap();
    let install = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(install)
        .unwrap();

    let first = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();
    assert_eq!(first.sweep.retained_objects, vec![root]);

    state.begin().unwrap();
    state.stage_global(0, GlobalSnapshot::I32(0)).unwrap();
    let removal = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(removal)
        .unwrap();

    let second = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();
    assert_eq!(second.sweep.removed_objects, vec![root]);
    assert!(objects.live_slot(root).is_err());
}

#[test]
fn transaction_state_persistent_mark_sweep_rejects_active_transaction() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x663, vec![ObjectValue::I32(5)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x663));

    let err = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
}

#[test]
fn transaction_state_persistent_mark_sweep_rejects_suspended_transaction() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let live = objects
        .allocate_persistent_struct_for_gc_ref(0x664, vec![ObjectValue::I32(6)])
        .unwrap();
    let mut state = TransactionState::default();
    let transaction = TransactionId::from_raw(0x664);

    assert_eq!(state.enter_transaction(transaction).unwrap(), None);
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(transaction));

    let err = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
    assert!(objects.live_slot(live).is_ok());
}

#[test]
fn persistent_root_recovery_installs_root_index_for_gc() {
    clear_current_thread_transaction_for_test();
    let mut state = TransactionState::default();

    state
        .install_recovered_persistent_roots([ObjectId { object_index: 41 }])
        .unwrap();

    assert_eq!(
        state.persistent_root_ids_for_test(),
        object_set([ObjectId { object_index: 41 }])
    );
}

#[test]
fn persistent_gc_after_commit_observer_stores_marker_progress() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x560, vec![ObjectValue::I32(6)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x561, vec![ObjectValue::Ref(Some(child))])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(560));
    state.stage_global(0, GlobalSnapshot::GcRef(0x561)).unwrap();
    let delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();
    assert_eq!(delta.new_roots, object_set([root]));

    state.complete_commit().unwrap();

    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &delta)
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(
                &objects,
                &PersistentGcCommitDelta::default(),
            )
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(
                &objects,
                &PersistentGcCommitDelta::default(),
            )
            .unwrap(),
        PersistentGcStepReport::default()
    );
}

#[test]
fn persistent_gc_maintenance_step_advances_read_heavy_cycle() {
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref(0x670, vec![ObjectValue::I32(1)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x671, vec![ObjectValue::Ref(Some(leaf))])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([root]).unwrap();

    let first = state
        .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(1))
        .unwrap();
    let second = state
        .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(1))
        .unwrap();

    assert_eq!(
        first,
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert_eq!(
        second,
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
}

#[test]
fn persistent_gc_finish_cycle_sweeps_unreachable_after_incremental_marking() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x672, vec![ObjectValue::I32(1)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x673, vec![ObjectValue::I32(2)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([root]).unwrap();

    state
        .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(1))
        .unwrap();

    let report = state
        .finish_persistent_gc_cycle_and_sweep_for_test(&mut objects)
        .unwrap()
        .expect("maintenance cycle should exist");

    assert_eq!(report.mark.reachable, object_set([root]));
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(garbage).is_err());
}

#[test]
fn persistent_gc_maintenance_step_rejects_suspended_transaction() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x674, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let transaction = TransactionId::from_raw(0x674);
    state.install_recovered_persistent_roots([root]).unwrap();

    assert_eq!(state.enter_transaction(transaction).unwrap(), None);
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(transaction));

    let err = state
        .persistent_gc_maintenance_step_for_test(&objects, PersistentGcBudget::objects(1))
        .unwrap_err();

    assert!(
        err.to_string().contains(
            "persistent object marker cannot run while a transaction is active or suspended"
        ),
        "{err:?}"
    );
}

#[test]
fn persistent_gc_finish_cycle_rejects_suspended_transaction() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x675, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let transaction = TransactionId::from_raw(0x675);
    state.install_recovered_persistent_roots([root]).unwrap();

    assert_eq!(state.enter_transaction(transaction).unwrap(), None);
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(transaction));

    let err = state
        .finish_persistent_gc_cycle_and_sweep_for_test(&mut objects)
        .unwrap_err();

    assert!(
        err.to_string().contains(
            "persistent object marker cannot run while a transaction is active or suspended"
        ),
        "{err:?}"
    );
}

#[test]
fn persistent_gc_commit_sequence_observes_edges_only_after_commit() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x570, vec![ObjectValue::I32(7)])
        .unwrap();
    let owner = objects
        .allocate_persistent_struct_for_gc_ref(0x571, vec![ObjectValue::Ref(None)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x572, vec![ObjectValue::Ref(Some(owner))])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(570));

    state.stage_global(0, GlobalSnapshot::GcRef(0x572)).unwrap();
    let initial_delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();
    assert_eq!(initial_delta.new_roots, object_set([root]));
    state.complete_commit().unwrap();
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &initial_delta)
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(
                &objects,
                &PersistentGcCommitDelta::default(),
            )
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, owner).unwrap();
    state
        .stage_struct_field(&objects, owner, 0, ObjectValue::Ref(Some(child)))
        .unwrap();
    let mut publications = Vec::new();
    state
        .commit_object_payloads_into(&mut objects, &mut publications)
        .unwrap();
    let edge_delta = state
        .persistent_gc_commit_delta(&objects, &publications)
        .unwrap();
    assert_eq!(
        edge_delta.edges,
        BTreeSet::from([PersistentObjectEdge {
            from: owner,
            to: child,
        }])
    );

    state.complete_commit().unwrap();
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &edge_delta)
            .unwrap(),
        PersistentGcStepReport::default()
    );
}

#[test]
fn persistent_gc_after_commit_observer_rejects_active_transaction_use() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x580, vec![ObjectValue::I32(8)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(580));
    state.stage_global(0, GlobalSnapshot::GcRef(0x580)).unwrap();
    let delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();

    let err = state
        .observe_persistent_gc_commit_delta_after_commit(&objects, &delta)
        .unwrap_err();

    assert_eq!(delta.new_roots, object_set([root]));
    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
}

#[test]
fn persistent_gc_best_effort_observer_does_not_report_post_commit_failure() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x581, vec![ObjectValue::I32(8)])
        .unwrap();
    let _root = objects
        .allocate_persistent_struct_for_gc_ref(0x582, vec![ObjectValue::Ref(Some(child))])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(581));
    state.stage_global(0, GlobalSnapshot::GcRef(0x582)).unwrap();
    let delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();
    state.complete_commit().unwrap();

    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &delta)
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );

    objects.free(child).unwrap();
    let report = state.observe_persistent_gc_commit_delta_after_commit_best_effort(
        &objects,
        &PersistentGcCommitDelta::default(),
    );
    assert_eq!(report, PersistentGcStepReport::default());
    assert!(state.persistent_gc_state.is_none());
}

#[test]
fn persistent_gc_commit_delta_invalidates_cached_reachability_on_root_removal() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x583, vec![ObjectValue::Ref(None)])
        .unwrap();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x584, vec![ObjectValue::I32(8)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(583));
    state.stage_global(0, GlobalSnapshot::GcRef(0x583)).unwrap();
    let initial_delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();
    state.complete_commit().unwrap();
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &initial_delta)
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );

    state.begin().unwrap();
    state.stage_global(0, GlobalSnapshot::I32(0)).unwrap();
    let removal_delta = state.persistent_gc_commit_delta(&objects, &[]).unwrap();
    assert!(removal_delta.invalidates_reachable_cache);
    assert!(removal_delta.new_roots.is_empty());
    state.complete_commit().unwrap();
    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &removal_delta)
            .unwrap(),
        PersistentGcStepReport::default()
    );

    state.begin().unwrap();
    state.acquire_object_write(&mut objects, root).unwrap();
    state
        .stage_struct_field(&objects, root, 0, ObjectValue::Ref(Some(child)))
        .unwrap();
    let mut publications = Vec::new();
    state
        .commit_object_payloads_into(&mut objects, &mut publications)
        .unwrap();
    let edge_delta = state
        .persistent_gc_commit_delta(&objects, &publications)
        .unwrap();
    assert!(edge_delta.invalidates_reachable_cache);
    state.complete_commit().unwrap();

    assert_eq!(
        state
            .observe_persistent_gc_commit_delta_after_commit(&objects, &edge_delta)
            .unwrap(),
        PersistentGcStepReport::default()
    );
}

#[test]
fn persistent_gc_commit_barrier_enqueues_child_when_owner_is_marked() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x541, vec![ObjectValue::I32(1)])
        .unwrap();
    let owner = objects
        .allocate_persistent_struct_for_gc_ref(0x542, vec![ObjectValue::Ref(None)])
        .unwrap();

    let mut state = PersistentGcState::new(&objects, [owner]).unwrap();
    assert_eq!(
        state
            .mark_step(&objects, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
    assert!(state.is_complete());

    let delta = {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut tx = TransactionState::new_for_test(TransactionId::from_raw(541));
        tx.acquire_object_write(&mut objects, owner).unwrap();
        tx.stage_struct_field(&objects, owner, 0, ObjectValue::Ref(Some(child)))
            .unwrap();
        let mut publications = Vec::new();
        tx.commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap();
        tx.persistent_gc_commit_delta(&objects, &publications)
            .unwrap()
    };

    assert_eq!(
        delta,
        PersistentGcCommitDelta {
            new_roots: BTreeSet::new(),
            edges: BTreeSet::from([PersistentObjectEdge {
                from: owner,
                to: child,
            }]),
            invalidates_reachable_cache: true,
        }
    );

    assert_eq!(
        state
            .observe_commit_delta(&objects, &delta, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
    assert!(state.is_complete());
    assert_eq!(
        state.into_report(&objects).unwrap().reachable,
        object_set([owner, child])
    );
}

#[test]
fn persistent_gc_commit_barrier_ignores_child_when_owner_is_unmarked() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x543, vec![ObjectValue::I32(2)])
        .unwrap();
    let owner = objects
        .allocate_persistent_struct_for_gc_ref(0x544, vec![ObjectValue::Ref(None)])
        .unwrap();

    let delta = {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let mut tx = TransactionState::new_for_test(TransactionId::from_raw(542));
        tx.acquire_object_write(&mut objects, owner).unwrap();
        tx.stage_struct_field(&objects, owner, 0, ObjectValue::Ref(Some(child)))
            .unwrap();
        let mut publications = Vec::new();
        tx.commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap();
        tx.persistent_gc_commit_delta(&objects, &publications)
            .unwrap()
    };

    let mut state = PersistentGcState::new(&objects, []).unwrap();
    assert_eq!(
        state
            .observe_commit_delta(&objects, &delta, PersistentGcBudget::objects(1))
            .unwrap(),
        PersistentGcStepReport {
            scanned_objects: 0,
            enqueued_objects: 0,
        }
    );
    assert!(state.is_complete());

    let report = state.into_report(&objects).unwrap();
    assert!(report.reachable.is_empty());
    assert_eq!(report.unreachable_persistent, object_set([owner, child]));
}

#[test]
fn persistent_gc_volatile_sweep_reports_unreachable_objects() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x550, vec![ObjectValue::Ref(None)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x551, vec![ObjectValue::I32(9)])
        .unwrap();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x552, vec![ObjectValue::I32(3)])
        .unwrap();
    objects
        .update_payload(
            root,
            ObjectPayload::Struct(vec![ObjectValue::Ref(Some(child))]),
        )
        .unwrap();

    let mark = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    let report = objects.apply_volatile_persistent_sweep(&mark).unwrap();

    assert_eq!(report.retained_objects, vec![root, child]);
    assert_eq!(report.removed_objects, vec![garbage]);
}

#[test]
fn persistent_gc_volatile_sweep_removes_unreachable_slots_when_requested() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x553, vec![ObjectValue::I32(1)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x554, vec![ObjectValue::I32(2)])
        .unwrap();

    let mark = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    let report = objects.apply_volatile_persistent_sweep(&mark).unwrap();

    assert_eq!(report.retained_objects, vec![root]);
    assert_eq!(report.removed_objects, vec![garbage]);
    assert!(objects.live_slot(garbage).is_err());
    assert_eq!(
        objects.known_object_id_for_live_gc_ref_bridge(0x553),
        Some(root)
    );
    assert_eq!(objects.known_object_id_for_live_gc_ref_bridge(0x554), None);
}

#[test]
fn persistent_gc_volatile_sweep_does_not_persist_dead_state() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x555, vec![ObjectValue::I32(5)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x556, vec![ObjectValue::I32(6)])
        .unwrap();
    let garbage_handle = objects.current_record_handle_for_test(garbage).unwrap();
    let garbage_bytes = objects.heap.record_bytes_for_test(garbage_handle).unwrap();

    let mark = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    objects.apply_volatile_persistent_sweep(&mark).unwrap();

    assert!(objects.live_slot(garbage).is_err());
    assert_eq!(
        objects.heap.record_bytes_for_test(garbage_handle).unwrap(),
        garbage_bytes
    );

    objects.rebuild_volatile_index_from_heap().unwrap();

    assert_eq!(
        objects.current_record_handle_for_test(garbage).unwrap(),
        garbage_handle
    );
    assert_eq!(
        objects.payload(garbage).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(6)])
    );
}

#[test]
fn persistent_gc_volatile_sweep_rejects_invalid_mark_graph_directly() {
    let mut objects = ObjectTable::default();
    let live = objects
        .allocate_persistent_struct_for_gc_ref(0x557, vec![ObjectValue::I32(7)])
        .unwrap();
    let mark = PersistentObjectMarkReport {
        invalid_roots: vec![PersistentRootError {
            root: ObjectId {
                object_index: 77_777,
            },
            kind: PersistentRootErrorKind::Missing,
        }],
        ..Default::default()
    };

    let err = objects.apply_volatile_persistent_sweep(&mark).unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object sweep requires a valid mark graph"),
        "{err:?}"
    );
    assert!(objects.live_slot(live).is_ok());
}

#[test]
fn persistent_mark_sweep_rejects_invalid_root_before_sweep() {
    let mut objects = ObjectTable::default();
    let live = objects
        .allocate_persistent_struct_for_gc_ref(0x650, vec![ObjectValue::I32(1)])
        .unwrap();
    let missing = ObjectId {
        object_index: 99_999,
    };

    let err = objects
        .persistent_mark_sweep_from_roots_for_test([missing])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object sweep requires a valid mark graph"),
        "{err:?}"
    );
    assert!(objects.live_slot(live).is_ok());
}

#[test]
fn persistent_mark_sweep_rejects_dangling_ref_before_sweep() {
    let mut objects = ObjectTable::default();
    let missing = ObjectId {
        object_index: 88_888,
    };
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x651, vec![ObjectValue::Ref(Some(missing))])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x652, vec![ObjectValue::I32(2)])
        .unwrap();

    let err = objects
        .persistent_mark_sweep_from_roots_for_test([root])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object sweep requires a valid mark graph"),
        "{err:?}"
    );
    assert!(objects.live_slot(root).is_ok());
    assert!(objects.live_slot(garbage).is_ok());
}

#[test]
fn persistent_object_marker_budgeted_state_rejects_zero_budget_with_pending_work() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x509, vec![ObjectValue::I32(9)])
        .unwrap();

    let mut state = PersistentGcState::new(&objects, [root]).unwrap();

    let err = state
        .mark_step(&objects, PersistentGcBudget::objects(0))
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker budget must scan at least one object"),
        "{err:?}"
    );
}

#[test]
fn persistent_object_marker_budgeted_state_rejects_incomplete_report() {
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref(0x50a, vec![ObjectValue::I32(10)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x50b, vec![ObjectValue::Ref(Some(leaf))])
        .unwrap();

    let state = PersistentGcState::new(&objects, [root]).unwrap();

    let err = state.into_report(&objects).unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot report with pending work"),
        "{err:?}"
    );
}

#[test]
fn persistent_object_marker_budgeted_state_rejects_active_transaction_use() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x50c, vec![ObjectValue::I32(12)])
        .unwrap();
    let mut state = PersistentGcState::new(&objects, [root]).unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let _state = TransactionState::new_for_test(TransactionId::from_raw(532));

    let err = state
        .mark_step(&objects, PersistentGcBudget::objects(1))
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
}

#[test]
fn persistent_object_marker_preserves_lifo_dangling_ref_order() {
    let mut objects = ObjectTable::default();
    let left_missing = ObjectId { object_index: 201 };
    let right_missing = ObjectId { object_index: 202 };
    let left = objects
        .allocate_persistent_struct_for_gc_ref(0x50d, vec![ObjectValue::Ref(Some(left_missing))])
        .unwrap();
    let right = objects
        .allocate_persistent_struct_for_gc_ref(0x50e, vec![ObjectValue::Ref(Some(right_missing))])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(
            0x50f,
            vec![ObjectValue::Ref(Some(left)), ObjectValue::Ref(Some(right))],
        )
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();

    assert_eq!(
        report.dangling_refs,
        vec![
            DanglingObjectRef {
                from: right,
                to: right_missing,
                kind: DanglingObjectRefKind::Missing,
            },
            DanglingObjectRef {
                from: left,
                to: left_missing,
                kind: DanglingObjectRefKind::Missing,
            },
        ]
    );
}

#[test]
fn persistent_object_marker_handles_cycles_and_shared_children() {
    let mut objects = ObjectTable::default();
    let shared = objects
        .allocate_persistent_struct_for_gc_ref(0x511, vec![ObjectValue::I32(7)])
        .unwrap();
    let left = objects
        .allocate_persistent_struct_for_gc_ref(
            0x512,
            vec![ObjectValue::Ref(Some(shared)), ObjectValue::Ref(None)],
        )
        .unwrap();
    let right = objects
        .allocate_persistent_struct_for_gc_ref(
            0x513,
            vec![ObjectValue::Ref(Some(shared)), ObjectValue::Ref(Some(left))],
        )
        .unwrap();
    objects
        .update_payload(
            left,
            ObjectPayload::Struct(vec![
                ObjectValue::Ref(Some(shared)),
                ObjectValue::Ref(Some(right)),
            ]),
        )
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [left]).unwrap();

    assert_eq!(report.reachable, object_set([left, right, shared]));
    assert!(report.unreachable_persistent.is_empty());
    assert!(report.invalid_roots.is_empty());
    assert!(report.dangling_refs.is_empty());
}

#[test]
fn persistent_object_marker_reports_invalid_roots() {
    let mut objects = ObjectTable::default();
    let volatile = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
    let missing = ObjectId { object_index: 999 };

    let report = PersistentObjectMarker::mark(&objects, [volatile, missing]).unwrap();

    assert!(report.reachable.is_empty());
    assert!(report.unreachable_persistent.is_empty());
    assert_eq!(
        report.invalid_roots,
        vec![
            PersistentRootError {
                root: volatile,
                kind: PersistentRootErrorKind::NotPersistent,
            },
            PersistentRootError {
                root: missing,
                kind: PersistentRootErrorKind::Missing,
            },
        ]
    );
    assert!(report.dangling_refs.is_empty());
}

#[test]
fn persistent_object_marker_reports_dangling_child_refs() {
    let mut objects = ObjectTable::default();
    let missing = ObjectId { object_index: 99 };
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x521, vec![ObjectValue::Ref(Some(missing))])
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();

    assert_eq!(report.reachable, object_set([root]));
    assert!(report.unreachable_persistent.is_empty());
    assert_eq!(
        report.dangling_refs,
        vec![DanglingObjectRef {
            from: root,
            to: missing,
            kind: DanglingObjectRefKind::Missing,
        }]
    );
    assert!(report.invalid_roots.is_empty());
}

#[test]
fn persistent_object_marker_rejects_layout_tracing_errors() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(type_layout::PersistentTypeLayout::Struct {
            id: type_layout::TypeLayoutId::new(307).unwrap(),
            fingerprint: 0x5354_5255_4354_0307,
            body_size: 40,
            fields: vec![type_layout::StructTraceField {
                field_index: 0,
                field_offset: 20,
                value_size: 20,
                kind: type_layout::TraceSlotKind::ObjectRef,
            }],
        })
        .unwrap();
    let mut rebuilt = ObjectTable::default();
    rebuilt
        .rebuild_from_recovery_for_test(
            &recovered_type_layouts,
            &[recovered_object_winner_for_test(
                41,
                3,
                ObjectKind::Struct as u16,
                307,
                encode_object_record_for_test(
                    41,
                    3,
                    307,
                    &ObjectPayload::Struct(vec![ObjectValue::I32(1)]),
                )
                .unwrap(),
            )],
        )
        .unwrap();

    let err = PersistentObjectMarker::mark(&rebuilt, [ObjectId { object_index: 41 }]).unwrap_err();

    let error = format!("{err:?}");
    assert!(
        error.contains("persistent struct trace field range exceeds payload length"),
        "{error}"
    );
}

#[test]
fn persistent_object_marker_rejects_active_transaction() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x531, vec![ObjectValue::I32(1)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let _state = TransactionState::new_for_test(TransactionId::from_raw(531));

    let err = PersistentObjectMarker::mark(&objects, [object]).unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
}

#[test]
fn persistent_object_marker_marks_recovered_root_ids() {
    let region = sample_region_with_two_object_winners_and_global_root();
    let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
    let winners = recovered.committed_object_winners().unwrap();
    let mut rebuilt = ObjectTable::default();
    rebuilt
        .rebuild_from_recovery_for_test(&recovered.type_layouts, &winners)
        .unwrap();
    let roots = recovered
        .root_object_ids
        .iter()
        .map(|&object_index| ObjectId { object_index });

    let report = PersistentObjectMarker::mark(&rebuilt, roots).unwrap();

    assert_eq!(
        report.reachable,
        object_set([ObjectId { object_index: 42 }, ObjectId { object_index: 41 }])
    );
    assert!(report.unreachable_persistent.is_empty());
    assert!(report.invalid_roots.is_empty());
    assert!(report.dangling_refs.is_empty());
}

mod persistent_gc_recovery_locations {
    use super::*;

    #[test]
    fn persistent_gc_recovery_winner_records_data_location() {
        let region = sample_region_with_two_object_winners();
        let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
        let winners = recovered.committed_object_winners().unwrap();
        let winner = winners
            .into_iter()
            .find(|winner| winner.object_id == 41)
            .unwrap();

        assert!(winner.data_block > 0);
        assert!(winner.record_len > 0);
    }

    #[test]
    fn persistent_gc_recovery_filter_reports_reachable_record_locations() {
        let mut recovered_type_layouts = TypeLayoutRegistry::default();
        recovered_type_layouts
            .insert(recovery_test_struct_layout(7))
            .unwrap();

        let winner41 = recovered_object_winner_with_location_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            11,
            101,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        );
        let winner42 = recovered_object_winner_with_location_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            12,
            202,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        );
        let winner43 = recovered_object_winner_with_location_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            7,
            13,
            303,
            encode_object_record_for_test(
                43,
                5,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        );

        let report = ObjectTable::persistent_recovery_gc_report(
            &recovered_type_layouts,
            &[winner41.clone(), winner42.clone(), winner43],
            &[41],
        )
        .unwrap();

        assert_eq!(
            report.reachable_record_locations,
            vec![
                recovered_record_location_for_test(&winner41),
                recovered_record_location_for_test(&winner42),
            ]
        );
    }

    #[test]
    fn persistent_gc_recovery_filter_reports_unreachable_record_locations() {
        let mut recovered_type_layouts = TypeLayoutRegistry::default();
        recovered_type_layouts
            .insert(recovery_test_struct_layout(7))
            .unwrap();

        let winner41 = recovered_object_winner_with_location_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            11,
            101,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        );
        let winner42 = recovered_object_winner_with_location_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            12,
            202,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        );
        let winner43 = recovered_object_winner_with_location_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            7,
            13,
            303,
            encode_object_record_for_test(
                43,
                5,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        );

        let report = ObjectTable::persistent_recovery_gc_report(
            &recovered_type_layouts,
            &[winner41, winner42, winner43.clone()],
            &[41],
        )
        .unwrap();

        assert_eq!(
            report.unreachable_record_locations,
            vec![recovered_record_location_for_test(&winner43)]
        );
    }
}

#[test]
fn pre_gc_object_recovery_closure_handles_cycles_nested_reachability_and_unreachable_winners() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(recovery_test_struct_layout(7))
        .unwrap();

    let winners = vec![
        recovered_object_winner_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(2),
                    ObjectValue::Ref(Some(ObjectId { object_index: 43 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                43,
                5,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(3),
                    ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            44,
            6,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                44,
                6,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(4), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
    ];

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(&recovered_type_layouts, &winners, &[41])
        .unwrap();

    assert_eq!(
        report.mark.reachable,
        object_set([
            ObjectId { object_index: 41 },
            ObjectId { object_index: 42 },
            ObjectId { object_index: 43 },
        ])
    );
    assert_eq!(
        report.mark.unreachable_persistent,
        object_set([ObjectId { object_index: 44 }])
    );
    assert_eq!(report.installed_winners, vec![41, 42, 43]);
    assert_eq!(report.skipped_unreachable_winners, vec![44]);
    assert_eq!(
        rebuilt
            .trace_object_ids(ObjectId { object_index: 43 })
            .unwrap(),
        vec![ObjectId { object_index: 41 }]
    );
    assert!(rebuilt.payload(ObjectId { object_index: 44 }).is_err());
    assert!(rebuilt.payload(ObjectId { object_index: 45 }).is_err());
    assert_eq!(rebuilt.live_count(), 3);
}

#[test]
fn persistent_gc_recovery_filter_installs_only_reachable_winners() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(recovery_test_struct_layout(7))
        .unwrap();

    let winners = vec![
        recovered_object_winner_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                43,
                5,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
    ];

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(&recovered_type_layouts, &winners, &[41])
        .unwrap();

    assert_eq!(report.installed_winners, vec![41, 42]);
    assert_eq!(
        rebuilt.payload(ObjectId { object_index: 41 }).unwrap(),
        ObjectPayload::Struct(vec![
            ObjectValue::I32(1),
            ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
        ])
    );
    assert_eq!(
        rebuilt.payload(ObjectId { object_index: 42 }).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)])
    );
    assert!(rebuilt.payload(ObjectId { object_index: 43 }).is_err());
}

#[test]
fn persistent_gc_recovery_filter_keeps_reachable_child_not_explicit_root() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(recovery_test_struct_layout(7))
        .unwrap();

    let winners = vec![
        recovered_object_winner_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
    ];

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(&recovered_type_layouts, &winners, &[41])
        .unwrap();

    assert_eq!(
        report.mark.reachable,
        object_set([ObjectId { object_index: 41 }, ObjectId { object_index: 42 }])
    );
    assert!(report.mark.invalid_roots.is_empty());
    assert!(report.mark.dangling_refs.is_empty());
    assert_eq!(
        rebuilt
            .trace_object_ids(ObjectId { object_index: 41 })
            .unwrap(),
        vec![ObjectId { object_index: 42 }]
    );
}

#[test]
fn persistent_gc_recovery_filter_reports_unreachable_winners() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(recovery_test_struct_layout(7))
        .unwrap();

    let winners = vec![
        recovered_object_winner_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(1),
                    ObjectValue::Ref(Some(ObjectId { object_index: 42 })),
                ]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            42,
            4,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                42,
                4,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                43,
                5,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
    ];

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(&recovered_type_layouts, &winners, &[41])
        .unwrap();

    assert_eq!(
        report.mark.reachable,
        object_set([ObjectId { object_index: 41 }, ObjectId { object_index: 42 }])
    );
    assert_eq!(
        report.mark.unreachable_persistent,
        object_set([ObjectId { object_index: 43 }])
    );
    assert_eq!(report.installed_winners, vec![41, 42]);
    assert_eq!(report.skipped_unreachable_winners, vec![43]);
}

#[test]
fn persistent_gc_recovery_filter_rejects_missing_type_layout() {
    let winners = vec![recovered_object_winner_for_test(
        41,
        3,
        ObjectKind::Struct as u16,
        999,
        encode_object_record_for_test(
            41,
            3,
            999,
            &ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
        )
        .unwrap(),
    )];
    let mut rebuilt = ObjectTable::default();

    let err = rebuilt
        .rebuild_reachable_from_recovery_for_test(&TypeLayoutRegistry::default(), &winners, &[41])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("unknown persistent type layout id: 999"),
        "{err:?}"
    );
}

#[test]
fn persistent_gc_recovery_filter_skips_unreachable_winner_with_missing_type_layout() {
    let mut recovered_type_layouts = TypeLayoutRegistry::default();
    recovered_type_layouts
        .insert(recovery_test_struct_layout(7))
        .unwrap();

    let winners = vec![
        recovered_object_winner_for_test(
            41,
            3,
            ObjectKind::Struct as u16,
            7,
            encode_object_record_for_test(
                41,
                3,
                7,
                &ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
        recovered_object_winner_for_test(
            43,
            5,
            ObjectKind::Struct as u16,
            999,
            encode_object_record_for_test(
                43,
                5,
                999,
                &ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap(),
        ),
    ];

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(&recovered_type_layouts, &winners, &[41])
        .unwrap();

    assert_eq!(report.installed_winners, vec![41]);
    assert_eq!(report.skipped_unreachable_winners, vec![43]);
    assert_eq!(
        report.mark.reachable,
        object_set([ObjectId { object_index: 41 }])
    );
    assert_eq!(
        report.mark.unreachable_persistent,
        object_set([ObjectId { object_index: 43 }])
    );
    assert_eq!(rebuilt.live_count(), 1);
    assert_eq!(
        rebuilt.payload(ObjectId { object_index: 41 }).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
    );
    assert!(rebuilt.payload(ObjectId { object_index: 43 }).is_err());
}

#[test]
fn persistent_gc_recovery_filter_preserves_destination_table_on_error() {
    let mut objects = ObjectTable::default();
    let sentinel = objects
        .allocate_persistent_struct_for_gc_ref(0x601, vec![ObjectValue::I32(9)])
        .unwrap();
    let sentinel_payload = objects.payload(sentinel).unwrap();
    let sentinel_handle = objects.current_record_handle_for_test(sentinel).unwrap();
    let sentinel_live_count = objects.live_count();

    let winners = vec![recovered_object_winner_for_test(
        41,
        3,
        ObjectKind::Struct as u16,
        999,
        encode_object_record_for_test(
            41,
            3,
            999,
            &ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
        )
        .unwrap(),
    )];

    let err = objects
        .rebuild_reachable_from_recovery_for_test(&TypeLayoutRegistry::default(), &winners, &[41])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("unknown persistent type layout id: 999"),
        "{err:?}"
    );
    assert_eq!(objects.live_count(), sentinel_live_count);
    assert_eq!(objects.payload(sentinel).unwrap(), sentinel_payload);
    assert_eq!(
        objects.current_record_handle_for_test(sentinel).unwrap(),
        sentinel_handle
    );
}

fn sample_region_with_two_object_winners_and_global_root()
-> crate::runtime::vm::block_region::VMemoryBlockRegion {
    let mut region =
        crate::runtime::vm::block_region::VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream1 = region.alloc_stream(1).unwrap();
    let stream2 = region.alloc_stream(2).unwrap();
    region
        .append_type_layout_metadata(&recovery_test_struct_layout(7))
        .unwrap();

    let target = encoded_object_publication_for_recovery_test(
        41,
        1,
        7,
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
    );
    let root = encoded_object_publication_for_recovery_test(
        42,
        1,
        7,
        ObjectPayload::Struct(vec![
            ObjectValue::I32(2),
            ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
        ]),
    );

    append_committed_object_winner(&mut region, 1, stream1, 0, &target);
    append_committed_object_winner(&mut region, 2, stream2, 0, &root);

    let stream = region.alloc_stream(3).unwrap();
    append_committed_root_update_for_marker_test(
        &mut region,
        3,
        stream,
        0,
        ((crate::runtime::vm::PackedGranuleDomain::TGlobal as u64) << 60) | 1,
        1,
        &[Some(42)],
    );
    region
}

fn append_committed_root_update_for_marker_test(
    region: &mut crate::runtime::vm::block_region::VMemoryBlockRegion,
    stream_id: u32,
    stream: crate::runtime::vm::block_region::StreamCursor,
    block_seq: u32,
    logical_id: u64,
    version: u32,
    object_ids: &[Option<u64>],
) {
    let payload = encode_root_object_refs_for_marker_test(object_ids);
    let record = TMemory::encode_publication_data_record(
        logical_id,
        version,
        crate::runtime::vm::PackedGranuleDomain::TGlobal as u16,
        0,
        &payload,
    )
    .unwrap();
    let location = region.append_data_record(stream, &record).unwrap();
    let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
    let entry = TMemory::publication_log_entry(
        logical_id,
        version,
        stream_id << 1,
        location.data_block,
        location.data_offset,
        0,
        true,
    )
    .unwrap();
    write_log_entry_for_recovery_test(region, log_block, entry);
}

fn encode_root_object_refs_for_marker_test(object_ids: &[Option<u64>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for object_id in object_ids {
        let raw = PersistentObjectRefRaw::from_optional_object_index(*object_id)
            .unwrap()
            .as_raw();
        bytes.extend_from_slice(&raw.to_le_bytes());
    }
    bytes
}

fn allocate_persistent_linked_list_for_migrated_gc_test(
    objects: &mut ObjectTable,
    len: usize,
    gc_ref_base: u32,
) -> ObjectId {
    let mut head = None;
    for index in 0..len {
        let gc_ref = gc_ref_base
            .checked_add(u32::try_from(index).unwrap())
            .unwrap();
        let node = objects
            .allocate_persistent_struct_for_gc_ref(
                gc_ref,
                vec![
                    ObjectValue::Ref(head),
                    ObjectValue::I32(i32::try_from(index).unwrap()),
                ],
            )
            .unwrap();
        head = Some(node);
    }
    head.expect("linked-list test needs at least one node")
}

fn persistent_linked_list_sum_for_test(objects: &ObjectTable, head: ObjectId) -> i32 {
    let mut sum = 0;
    let mut current = Some(head);
    while let Some(object_id) = current {
        let payload = objects.payload(object_id).unwrap();
        let ObjectPayload::Struct(fields) = payload else {
            panic!("linked-list node must be a struct");
        };
        let [ObjectValue::Ref(next), ObjectValue::I32(value)] = fields.as_slice() else {
            panic!("linked-list node must have next and value fields");
        };
        sum += *value;
        current = *next;
    }
    sum
}

fn allocate_persistent_binary_tree_for_migrated_gc_test(
    objects: &mut ObjectTable,
    depth: u32,
    value: i32,
    next_gc_ref: &mut u32,
) -> Option<ObjectId> {
    if depth == 0 {
        return None;
    }
    let left = allocate_persistent_binary_tree_for_migrated_gc_test(
        objects,
        depth - 1,
        value * 2,
        next_gc_ref,
    );
    let right = allocate_persistent_binary_tree_for_migrated_gc_test(
        objects,
        depth - 1,
        value * 2 + 1,
        next_gc_ref,
    );
    let gc_ref = *next_gc_ref;
    *next_gc_ref = gc_ref.checked_add(1).unwrap();
    Some(
        objects
            .allocate_persistent_struct_for_gc_ref(
                gc_ref,
                vec![
                    ObjectValue::Ref(left),
                    ObjectValue::Ref(right),
                    ObjectValue::I32(value),
                ],
            )
            .unwrap(),
    )
}

fn persistent_binary_tree_sum_for_test(objects: &ObjectTable, root: Option<ObjectId>) -> i32 {
    let Some(object_id) = root else {
        return 0;
    };
    let payload = objects.payload(object_id).unwrap();
    let ObjectPayload::Struct(fields) = payload else {
        panic!("binary-tree node must be a struct");
    };
    let [
        ObjectValue::Ref(left),
        ObjectValue::Ref(right),
        ObjectValue::I32(value),
    ] = fields.as_slice()
    else {
        panic!("binary-tree node must have left, right, and value fields");
    };
    value
        + persistent_binary_tree_sum_for_test(objects, *left)
        + persistent_binary_tree_sum_for_test(objects, *right)
}

fn object_set<const N: usize>(objects: [ObjectId; N]) -> BTreeSet<ObjectId> {
    objects.into_iter().collect()
}

#[test]
fn lock_based_allows_shared_reads_and_writer_upgrade() {
    let mut locks = LockBased::default();
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    locks.record_read_for_test(first, granule, 5).unwrap();
    locks.record_read_for_test(second, granule, 5).unwrap();
    locks.acquire_write_for_test(second, granule, 5).unwrap();
}

#[test]
fn lock_based_writer_excludes_other_transactions() {
    let mut locks = LockBased::default();
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    locks.acquire_write_for_test(first, granule, 3).unwrap();

    let read_error = locks
        .record_read_result_for_test(second, granule, 3)
        .unwrap_err();
    assert_eq!(read_error, LockBasedConflictKindForTest::ReadOwnedByOther);

    let write_error = locks
        .acquire_write_result_for_test(second, granule, 3)
        .unwrap_err();
    assert_eq!(write_error, LockBasedConflictKindForTest::WriteOwnedByOther);
}

#[test]
fn lock_based_conflicts_are_released_on_abort() {
    let mut locks = LockBased::default();
    let first = TransactionId(1);
    let second = TransactionId(2);

    locks.acquire_memory_granule_write(first, 0, 0).unwrap();
    assert!(locks.acquire_memory_granule_read(second, 0, 0).is_err());

    locks.release_transaction(first);

    locks.acquire_memory_granule_read(second, 0, 0).unwrap();
}

#[test]
fn lock_based_supports_upgrade_and_release() {
    let mut locks = LockBased::default();
    let first = TransactionId(1);
    let second = TransactionId(2);

    locks.acquire_memory_granule_read(first, 0, 7).unwrap();
    locks.acquire_memory_granule_write(first, 0, 7).unwrap();
    locks.release_transaction(first);

    locks.acquire_memory_granule_write(second, 0, 7).unwrap();
}

#[test]
fn lock_based_write_conflict_aborts_current_transaction() {
    let mut locks = LockBased::default();
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let conflicted = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };
    let independent = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 1,
    };

    locks.acquire_write_for_test(first, conflicted, 9).unwrap();
    locks
        .acquire_write_for_test(second, independent, 3)
        .unwrap();

    let error = locks
        .acquire_write_result_for_test(second, conflicted, 9)
        .unwrap_err();
    assert_eq!(error, LockBasedConflictKindForTest::WriteOwnedByOther);
    assert_eq!(locks.owner_for_test(independent), Some(second));

    locks.abort_for_test(second);

    assert_eq!(locks.owner_for_test(conflicted), Some(first));
    assert_eq!(locks.owner_for_test(independent), None);
}

#[test]
fn lock_based_validates_optimistic_reads_at_commit() {
    let mut locks = LockBased::default();
    let reader = TransactionId::from_raw(1);
    let writer = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };

    locks.record_read_for_test(reader, granule, 7).unwrap();
    locks.acquire_write_for_test(writer, granule, 7).unwrap();
    locks.abort_for_test(writer);

    let error = locks
        .validate_read_result_for_test(reader, granule, 8)
        .unwrap_err();
    assert_eq!(error, LockBasedConflictKindForTest::ReadVersionMismatch);
}

#[test]
fn lock_based_abort_releases_owned_granules() {
    let mut locks = LockBased::default();
    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };

    locks
        .acquire_write_for_test(transaction, granule, 4)
        .unwrap();
    assert_eq!(locks.owner_for_test(granule), Some(transaction));

    locks.abort_for_test(transaction);

    assert_eq!(locks.owner_for_test(granule), None);
}

#[test]
fn fixture_executor_begins_and_fails_transaction() {
    let mut state = TransactionState::default();
    let operators = [
        wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 0,
            operator: wasmtime_environ::TransactionOperator::TTry,
            bytes_read: 2,
        },
        wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 2,
            operator: wasmtime_environ::TransactionOperator::TFail,
            bytes_read: 2,
        },
    ];

    execute_research_transaction_fixture(&mut state, &operators).unwrap();

    assert_eq!(state.active_transaction(), None);
    assert!(state.structured_failure_pending());
    state.clear_structured_failure();
    assert!(!state.structured_failure_pending());
}

#[test]
fn fixture_executor_ttry_end_consumes_pending_failure() {
    let mut state = TransactionState::default();
    let operators = [
        wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 0,
            operator: wasmtime_environ::TransactionOperator::TTry,
            bytes_read: 2,
        },
        wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 2,
            operator: wasmtime_environ::TransactionOperator::TFail,
            bytes_read: 2,
        },
        wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 4,
            operator: wasmtime_environ::TransactionOperator::TTryEnd,
            bytes_read: 2,
        },
    ];

    execute_research_transaction_fixture(&mut state, &operators).unwrap();

    assert_eq!(state.active_transaction(), None);
    assert!(!state.structured_failure_pending());
}

#[test]
fn fixture_executor_commits_open_transaction_at_end() {
    let mut state = TransactionState::default();
    let operators = [wasmtime_environ::ResearchTransactionModuleOperator {
        function_index: 0,
        body_offset: 0,
        operator: wasmtime_environ::TransactionOperator::TTry,
        bytes_read: 2,
    }];

    execute_research_transaction_fixture(&mut state, &operators).unwrap();

    assert_eq!(state.active_transaction(), None);
}

#[test]
fn module_compilation_accepts_lifecycle_transaction_opcodes() {
    let engine = crate::Engine::default();
    let wasm = [
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x0a, 0x08, 0x01, 0x06, 0x00, // code section/function body
        0xfa, 0x04, // ttry
        0xfa, 0x0f, // tfail
        0x0b, // end
    ];

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_accepts_transaction_data_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_memory_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section
        0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
        0x41, 0x00, // i32.const 0
        0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_rejects_transaction_load_on_ordinary_memory() {
    let engine = crate::Engine::default();
    let wasm = [
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // ordinary memory section
        0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
        0x41, 0x00, // i32.const 0
        0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
        0x0b, // end
    ];

    let error = crate::Module::new(&engine, wasm).unwrap_err();
    let error = format!("{error:?}");
    assert!(
        error.contains("transactional memory operator requires tmemory"),
        "{error}"
    );
}

#[test]
fn module_compilation_accepts_transaction_store_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_memory_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section
        0x0a, 0x0c, 0x01, 0x0a, 0x00, // code section/function body
        0x41, 0x00, // i32.const 0
        0x41, 0x2a, // i32.const 42
        0xfa, 0x36, 0x02, 0x00, // i32.tstore align=2 offset=0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_accepts_transaction_global_get_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_global_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // global section
        0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
        0xfa, 0x23, 0x00, // tglobal.get 0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_rejects_transaction_global_get_on_ordinary_global() {
    let engine = crate::Engine::default();
    let wasm = [
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // ordinary global section
        0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
        0xfa, 0x23, 0x00, // tglobal.get 0
        0x0b, // end
    ];

    let error = crate::Module::new(&engine, wasm).unwrap_err();
    let error = format!("{error:?}");
    assert!(
        error.contains("transactional global operator requires tglobal"),
        "{error}"
    );
}

#[test]
fn module_compilation_accepts_transaction_global_set_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_global_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x06, 0x06, 0x01, 0x7f, 0x01, 0x41, 0x00, 0x0b, // global section
        0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
        0x41, 0x2a, // i32.const 42
        0xfa, 0x24, 0x00, // tglobal.set 0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_accepts_transaction_memory_size_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_memory_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section
        0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
        0xfa, 0x3f, 0x00, // tmemory.size 0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_accepts_transaction_memory_grow_helper_lowering() {
    let engine = crate::Engine::default();
    let wasm = with_transaction_memory_metadata(&[
        0x00, 0x61, 0x73, 0x6d, // magic
        0x01, 0x00, 0x00, 0x00, // version
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section
        0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
        0x41, 0x01, // i32.const 1
        0xfa, 0x40, 0x00, // tmemory.grow 0
        0x0b, // end
    ]);

    crate::Module::new(&engine, wasm).unwrap();
}

#[test]
fn module_compilation_accepts_transaction_object_helper_lowering() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    crate::Module::new(
        &engine,
        wat::parse_str(
            r#"
                (module
                  (type $s (tstruct (field (mut i32))))
                  (type $ps (tstruct (field (mut i8))))
                  (type $a (tarray (mut i32)))
                  (type $pa (tarray (mut i8)))
                  (type $ra (tarray (mut (tref $s))))
                  (data $d "\00\01\02\03")
                  (elem $e (tref $s)
                    (tstruct.new $s (i32.const 1))
                    (tstruct.new $s (i32.const 2)))
                  (tfunc (export "object-smoke") (result i32)
                    (local $sref (tref $s))
                    (local $psref (tref $ps))
                    (local $aref (tref $a))
                    (local $aref2 (tref $a))
                    (local $paref (tref $pa))
                    (local $raref (tref $ra))
                    (local.set $sref
                      (tstruct.new $s (i32.const 41)))
                    (drop (tstruct.new_default $s))
                    (local.set $psref
                      (tstruct.new $ps (i32.const -1)))
                    (drop (tstruct.get_s $ps 0 (tref.cast_read (local.get $psref))))
                    (drop (tstruct.get_u $ps 0 (tref.cast_read (local.get $psref))))
                    (tstruct.set $s 0 (tref.cast_write (local.get $sref)) (i32.const 42))
                    (local.set $aref
                      (tarray.new $a (i32.const 7) (i32.const 4)))
                    (local.set $aref2
                      (tarray.new_default $a (i32.const 4)))
                    (drop (tarray.new_fixed $a 2 (i32.const 1) (i32.const 2)))
                    (local.set $paref
                      (tarray.new_data $pa $d (i32.const 0) (i32.const 4)))
                    (local.set $raref
                      (tarray.new_elem $ra $e (i32.const 0) (i32.const 2)))
                    (tarray.set $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 1)
                      (i31.get_s (tref.ti31 (i32.const 13))))
                    (drop (tarray.len (local.get $aref)))
                    (drop (tarray.get_s $pa (tref.cast_read (local.get $paref)) (i32.const 0)))
                    (drop (tarray.get_u $pa (tref.cast_read (local.get $paref)) (i32.const 0)))
                    (tarray.fill $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 2)
                      (i32.const 5)
                      (i32.const 1))
                    (tarray.copy $a $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 3)
                      (tref.cast_read (local.get $aref2))
                      (i32.const 0)
                      (i32.const 1))
                    (tarray.init_data $pa $d
                      (tref.cast_write (local.get $paref))
                      (i32.const 1)
                      (i32.const 0)
                      (i32.const 1))
                    (tarray.init_elem $ra $e
                      (tref.cast_write (local.get $raref))
                      (i32.const 0)
                      (i32.const 0)
                      (i32.const 1))
                    (drop (ti31.get_u (tref.ti31 (i32.const 13))))
                    (drop (tany.convert_textern
                      (textern.convert_tany (tref.ti31 (i32.const 7)))))
                    (i32.add
                      (tstruct.get $s 0 (tref.cast_read (local.get $sref)))
                      (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))))
                "#,
        )
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn transaction_i31_reference_fields_round_trip_as_scalars() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (tstruct (field (mut (ref null i31)))))
              (tfunc (export "x") (result i32)
                (local $s (tref $s))
                (local.set $s (tstruct.new $s (tref.ti31 (i32.const 13))))
                (i31.get_s (tstruct.get $s 0 (tref.cast_read (local.get $s))))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let x = instance.get_typed_func::<(), i32>(&mut store, "x").unwrap();

    assert_eq!(x.call(&mut store, ()).unwrap(), 13);
}

#[test]
fn transaction_object_tstruct_executes_through_object_table() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (tfunc (export "create_set_get") (result i32)
                (local $sref (tref $s))
                (local.set $sref
                  (tstruct.new $s (i32.const 41)))
                (tstruct.set $s 0 (tref.cast_write (local.get $sref)) (i32.const 42))
                (tstruct.get $s 0 (tref.cast_read (local.get $sref))))
              (tfunc (export "i31s") (result i32)
                (ti31.get_s (tref.ti31 (i32.const 0x7fffffff))))
              (tfunc (export "i31u") (result i32)
                (ti31.get_u (tref.ti31 (i32.const 0x7fffffff)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create_set_get = instance
        .get_typed_func::<(), i32>(&mut store, "create_set_get")
        .unwrap();
    let i31s = instance
        .get_typed_func::<(), i32>(&mut store, "i31s")
        .unwrap();
    let i31u = instance
        .get_typed_func::<(), i32>(&mut store, "i31u")
        .unwrap();

    assert_eq!(create_set_get.call(&mut store, ()).unwrap(), 42);
    assert_eq!(i31s.call(&mut store, ()).unwrap(), -1);
    assert_eq!(i31u.call(&mut store, ()).unwrap(), 0x7fffffff);
    assert_eq!(store.transaction_object_table().live_count(), 1);
    assert_eq!(
        store
            .transaction_object_table()
            .payload(ObjectId { object_index: 0 })
            .unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(42)])
    );
}

#[test]
fn transaction_object_tstruct_get_ref_roundtrips_into_object_operation() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $node (tstruct
                (field (mut i32))
                (field (mut (tref null $node)))))
              (tfunc (export "read_child") (result i32)
                (local $child (tref $node))
                (local $parent (tref $node))
                (local $copy (tref $node))
                (local $roundtrip (tref null $node))
                (local.set $child
                  (tstruct.new $node (i32.const 7) (tref.null $node)))
                (local.set $parent
                  (tstruct.new $node (i32.const 0) (local.get $child)))
                (local.set $copy
                  (tstruct.new $node (i32.const 0) (tref.null $node)))
                (local.set $roundtrip
                  (tstruct.get $node 1 (tref.cast_read (local.get $parent))))
                (tstruct.set $node 1
                  (tref.cast_write (local.get $copy))
                  (local.get $roundtrip))
                (local.set $roundtrip
                  (tstruct.get $node 1 (tref.cast_read (local.get $copy))))
                (tstruct.get $node 0 (tref.cast_read (local.get $roundtrip)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let read_child = instance
        .get_typed_func::<(), i32>(&mut store, "read_child")
        .unwrap();

    assert_eq!(read_child.call(&mut store, ()).unwrap(), 7);
    assert_eq!(store.transaction_object_table().live_count(), 3);
}

#[test]
fn transaction_object_tstruct_tfail_frees_new_object_record() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (struct (field (mut i32))))
              (tfunc (export "create_fail")
                (drop (tstruct.new $s (i32.const 41)))
                (tfail)))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create_fail = instance
        .get_typed_func::<(), ()>(&mut store, "create_fail")
        .unwrap();

    create_fail.call(&mut store, ()).unwrap();

    assert_eq!(store.transaction_object_table().live_count(), 0);
}

#[test]
fn transaction_object_tstruct_trap_frees_new_object_record() {
    clear_current_thread_transaction_for_test();
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (struct (field (mut i32))))
              (tfunc (export "create_trap")
                (drop (tstruct.new $s (i32.const 41)))
                (unreachable))
              (tfunc (export "create_ok") (result i32)
                (local $s (tref $s))
                (local.set $s (tstruct.new $s (i32.const 7)))
                (tstruct.get $s 0 (tref.cast_read (local.get $s)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create_trap = instance
        .get_typed_func::<(), ()>(&mut store, "create_trap")
        .unwrap();
    let create_ok = instance
        .get_typed_func::<(), i32>(&mut store, "create_ok")
        .unwrap();

    assert!(create_trap.call(&mut store, ()).is_err());
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(store.transaction_object_table().live_count(), 0);

    assert_eq!(create_ok.call(&mut store, ()).unwrap(), 7);
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(store.transaction_object_table().live_count(), 1);
}

#[test]
fn transaction_object_existing_staged_write_rolls_back_on_tfail_and_trap() {
    clear_current_thread_transaction_for_test();
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (struct (field (mut i32))))
              (global $slot (mut (ref null $s)) (ref.null $s))
              (tfunc (export "new") (result i32)
                (global.set $slot (tstruct.new $s (i32.const 1)))
                (tstruct.get $s 0 (tref.cast_read (global.get $slot))))
              (tfunc (export "read") (result i32)
                (tstruct.get $s 0 (tref.cast_read (global.get $slot))))
              (tfunc (export "write_fail")
                (tstruct.set $s 0 (tref.cast_write (global.get $slot)) (i32.const 99))
                (tfail))
              (tfunc (export "write_trap")
                (tstruct.set $s 0 (tref.cast_write (global.get $slot)) (i32.const 77))
                (unreachable))
              (tfunc (export "write_ok") (param i32)
                (tstruct.set $s 0 (tref.cast_write (global.get $slot)) (local.get 0))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let new = instance
        .get_typed_func::<(), i32>(&mut store, "new")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();
    let write_fail = instance
        .get_typed_func::<(), ()>(&mut store, "write_fail")
        .unwrap();
    let write_trap = instance
        .get_typed_func::<(), ()>(&mut store, "write_trap")
        .unwrap();
    let write_ok = instance
        .get_typed_func::<i32, ()>(&mut store, "write_ok")
        .unwrap();

    assert_eq!(new.call(&mut store, ()).unwrap(), 1);
    assert_eq!(store.transaction_object_table().live_count(), 1);

    assert_eq!(read.call(&mut store, ()).unwrap(), 1);

    write_fail.call(&mut store, ()).unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(read.call(&mut store, ()).unwrap(), 1);

    write_trap.call(&mut store, ()).unwrap_err();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(read.call(&mut store, ()).unwrap(), 1);

    write_ok.call(&mut store, 5).unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(read.call(&mut store, ()).unwrap(), 5);
    assert_eq!(store.transaction_object_table().live_count(), 1);
}

#[test]
fn transaction_object_tarray_executes_through_object_table() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $a (tarray (mut i32)))
              (tfunc (export "create_set_get") (result i32)
                (local $aref (tref $a))
                (local.set $aref
                  (tarray.new $a (i32.const 7) (i32.const 3)))
                (tarray.set $a (tref.cast_write (local.get $aref)) (i32.const 1) (i32.const 42))
                (i32.add
                  (tarray.len (local.get $aref))
                  (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create_set_get = instance
        .get_typed_func::<(), i32>(&mut store, "create_set_get")
        .unwrap();

    assert_eq!(create_set_get.call(&mut store, ()).unwrap(), 45);
    assert_eq!(store.transaction_object_table().live_count(), 1);
    assert_eq!(
        store
            .transaction_object_table()
            .payload(ObjectId { object_index: 0 })
            .unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::I32(7),
            ObjectValue::I32(42),
            ObjectValue::I32(7)
        ])
    );
}

#[test]
fn transaction_object_constructors_return_transaction_ref_handles() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (type $a (tarray (mut (tref null $s))))
              (tfunc (export "exercise") (result i32)
                (local $sref (tref $s))
                (local $aref (tref $a))
                (local $roundtrip (tref null $s))
                (local.set $sref
                  (tstruct.new $s (i32.const 41)))
                (tstruct.set $s 0 (tref.cast_write (local.get $sref)) (i32.const 42))
                (local.set $aref
                  (tarray.new $a (local.get $sref) (i32.const 2)))
                (tarray.set $a
                  (tref.cast_write (local.get $aref))
                  (i32.const 1)
                  (tref.null $s))
                (local.set $roundtrip
                  (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 0)))
                (i32.add
                  (tstruct.get $s 0 (tref.cast_read (local.get $roundtrip)))
                  (tarray.len (local.get $aref)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let exercise = instance
        .get_typed_func::<(), i32>(&mut store, "exercise")
        .unwrap();

    assert_eq!(exercise.call(&mut store, ()).unwrap(), 44);

    let objects = store.transaction_object_table();
    let live_ids = objects.live_object_ids_for_test();
    let struct_id = live_ids
        .iter()
        .copied()
        .find(|object_id| {
            objects.payload(*object_id).unwrap()
                == ObjectPayload::Struct(vec![ObjectValue::I32(42)])
        })
        .unwrap();
    let array_id = live_ids
        .iter()
        .copied()
        .find(|object_id| {
            objects.payload(*object_id).unwrap()
                == ObjectPayload::Array(vec![
                    ObjectValue::Ref(Some(struct_id)),
                    ObjectValue::Ref(None),
                ])
        })
        .unwrap();
    let struct_handle = *objects
        .objects_to_transaction_ref_handles
        .get(&struct_id)
        .unwrap();
    let array_handle = *objects
        .objects_to_transaction_ref_handles
        .get(&array_id)
        .unwrap();

    assert_eq!(objects.live_count(), 2);
    assert_eq!(live_ids.len(), 2);
    assert!(objects.live_bridge_gc_refs_to_objects.is_empty());
    assert!(objects.object_to_live_bridge_gc_ref.is_empty());
    assert_eq!(objects.transaction_ref_handles_to_objects.len(), 2);
    assert_eq!(objects.objects_to_transaction_ref_handles.len(), 2);
    assert_eq!(
        objects
            .transaction_ref_handles_to_objects
            .get(&struct_handle),
        Some(&struct_id)
    );
    assert_eq!(
        objects
            .transaction_ref_handles_to_objects
            .get(&array_handle),
        Some(&array_id)
    );
    assert_eq!(
        objects.known_object_id_for_live_gc_ref_bridge(struct_handle),
        None
    );
    assert_eq!(
        objects.known_object_id_for_live_gc_ref_bridge(array_handle),
        None
    );
    assert_eq!(
        objects.payload(struct_id).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(42)])
    );
    assert_eq!(
        objects.payload(array_id).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::Ref(Some(struct_id)),
            ObjectValue::Ref(None)
        ])
    );
}

#[test]
fn transaction_object_tarray_default_and_fixed_constructors_create_object_records() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $a (tarray (mut f32)))
              (tfunc (export "default_get") (result f32)
                (local $aref (tref $a))
                (local.set $aref (tarray.new_default $a (i32.const 2)))
                (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))
              (tfunc (export "fixed_get") (result f32)
                (local $aref (tref $a))
                (local.set $aref
                  (tarray.new_fixed $a 3
                    (f32.const 1)
                    (f32.const 2)
                    (f32.const 3)))
                (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 2))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let default_get = instance
        .get_typed_func::<(), f32>(&mut store, "default_get")
        .unwrap();
    let fixed_get = instance
        .get_typed_func::<(), f32>(&mut store, "fixed_get")
        .unwrap();

    assert_eq!(default_get.call(&mut store, ()).unwrap().to_bits(), 0);
    assert_eq!(
        fixed_get.call(&mut store, ()).unwrap().to_bits(),
        3.0f32.to_bits()
    );
    assert_eq!(store.transaction_object_table().live_count(), 2);
    assert_eq!(
        store
            .transaction_object_table()
            .payload(ObjectId { object_index: 0 })
            .unwrap(),
        ObjectPayload::Array(vec![ObjectValue::F32(0), ObjectValue::F32(0)])
    );
    assert_eq!(
        store
            .transaction_object_table()
            .payload(ObjectId { object_index: 1 })
            .unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::F32(1.0f32.to_bits()),
            ObjectValue::F32(2.0f32.to_bits()),
            ObjectValue::F32(3.0f32.to_bits())
        ])
    );
}

#[test]
fn transaction_object_static_initializers_return_transaction_ref_handles() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (tstruct (field i32)))
              (type $a (tarray (mut (tref null $s))))
              (global $aref (mut (tref null $a))
                (tarray.new_fixed $a 2
                  (tstruct.new $s (i32.const 41))
                  (tref.null $s)))
              (tfunc (export "read") (result i32)
                (local $roundtrip (tref null $s))
                (local.set $roundtrip
                  (tarray.get $a (tref.cast_read (global.get $aref)) (i32.const 0)))
                (i32.add
                  (tstruct.get $s 0 (tref.cast_read (local.get $roundtrip)))
                  (tarray.len (global.get $aref)))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 43);

    let objects = store.transaction_object_table();
    assert_eq!(objects.live_count(), 2);
    assert!(objects.live_bridge_gc_refs_to_objects.is_empty());
    assert_eq!(objects.transaction_ref_handles_to_objects.len(), 2);
    let live_ids = objects.live_object_ids_for_test();
    let struct_id = live_ids
        .iter()
        .copied()
        .find(|object_id| {
            objects.payload(*object_id).unwrap()
                == ObjectPayload::Struct(vec![ObjectValue::I32(41)])
        })
        .unwrap();
    let array_id = live_ids
        .iter()
        .copied()
        .find(|object_id| {
            objects.payload(*object_id).unwrap()
                == ObjectPayload::Array(vec![
                    ObjectValue::Ref(Some(struct_id)),
                    ObjectValue::Ref(None),
                ])
        })
        .unwrap();
    assert_eq!(
        objects.payload(struct_id).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(41)])
    );
    assert_eq!(
        objects.payload(array_id).unwrap(),
        ObjectPayload::Array(vec![
            ObjectValue::Ref(Some(struct_id)),
            ObjectValue::Ref(None)
        ])
    );
}

#[test]
fn exported_tfunc_returning_tarray_ref_enters_transaction() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $a (array (mut f32)))
              (global $slot (mut (ref null $a)) (ref.null $a))
              (tfunc (export "new") (result i32)
                (global.set $slot (tarray.new_default $a (i32.const 2)))
                (tarray.len (global.get $slot)))
              (tfunc (export "read") (result f32)
                (tarray.get $a (tref.cast_read (global.get $slot)) (i32.const 1))))
            "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let new = instance
        .get_typed_func::<(), i32>(&mut store, "new")
        .unwrap();
    let read = instance
        .get_typed_func::<(), f32>(&mut store, "read")
        .unwrap();

    assert_eq!(new.call(&mut store, ()).unwrap(), 2);
    assert_eq!(read.call(&mut store, ()).unwrap().to_bits(), 0);
    assert_eq!(store.transaction_object_table().live_count(), 1);
}
