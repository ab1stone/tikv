// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    fs,
    fs::File,
    io,
    io::prelude::*,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    thread,
    time::Duration,
};

use engine_traits::{RaftEngineDebug, RaftEngineReadOnly};
use kvproto::raft_serverpb::RaftMessage;
use raft::eraftpb::MessageType;
use test_raftstore::*;
use test_raftstore_macro::test_case;
use tikv_util::{HandyRwLock, config::*, time::Instant};

#[test]
fn test_overlap_cleanup() {
    let mut cluster = new_node_cluster(0, 3);
    // Disable raft log gc in this test case.
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(60);

    let gen_snapshot_fp = "region_gen_snap";

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();

    let region_id = cluster.run_conf_change();
    pd_client.must_add_peer(region_id, new_peer(2, 2));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");

    cluster.must_transfer_leader(region_id, new_peer(2, 2));
    // This will only pause the bootstrapped region, so the split region
    // can still work as expected.
    fail::cfg(gen_snapshot_fp, "pause").unwrap();
    pd_client.must_add_peer(region_id, new_peer(3, 3));
    cluster.must_put(b"k3", b"v3");
    assert_snapshot(&cluster.get_snap_dir(2), region_id, true);
    let region1 = cluster.get_region(b"k1");
    cluster.must_split(&region1, b"k2");
    // Wait till the snapshot of split region is applied, whose range is ["", "k2").
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");
    // Resume the fail point and pause it again. So only the paused snapshot is
    // generated. And the paused snapshot's range is ["", ""), hence overlap.
    fail::cfg(gen_snapshot_fp, "pause").unwrap();
    // Overlap snapshot should be deleted.
    assert_snapshot(&cluster.get_snap_dir(3), region_id, false);
    fail::remove(gen_snapshot_fp);
}

// When resolving remote address, all messages will be dropped and
// report unreachable. However unreachable won't reset follower's
// progress if it's in Snapshot state. So trying to send a snapshot
// when the address is being resolved will leave follower's progress
// stay in Snapshot forever.
#[test]
fn test_server_snapshot_on_resolve_failure() {
    let mut cluster = new_server_cluster(1, 2);
    configure_for_snapshot(&mut cluster.cfg);

    let on_send_store_fp = "transport_on_send_snapshot";

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();
    cluster.run_conf_change();

    cluster.must_put(b"k1", b"v1");

    let ready_notify = Arc::default();
    let (notify_tx, notify_rx) = mpsc::channel();
    cluster.sim.write().unwrap().add_send_filter(
        1,
        Box::new(MessageTypeNotifier::new(
            MessageType::MsgSnapshot,
            notify_tx,
            Arc::clone(&ready_notify),
        )),
    );

    // "return(2)" those failure occurs if TiKV resolves or sends to store 2.
    fail::cfg(on_send_store_fp, "return(2)").unwrap();
    pd_client.add_peer(1, new_learner_peer(2, 2));

    // We are ready to recv notify.
    ready_notify.store(true, Ordering::SeqCst);
    notify_rx.recv_timeout(Duration::from_secs(3)).unwrap();

    let engine2 = cluster.get_engine(2);
    must_get_none(&engine2, b"k1");

    // If snapshot status is reported correctly, sending snapshot should be retried.
    notify_rx.recv_timeout(Duration::from_secs(3)).unwrap();
}

#[test]
fn test_generate_snapshot() {
    let mut cluster = new_server_cluster(1, 5);
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::millis(20);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(8);
    cluster.cfg.raft_store.merge_max_log_gap = 3;
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run();
    cluster.must_transfer_leader(1, new_peer(1, 1));
    cluster.stop_node(4);
    cluster.stop_node(5);
    (0..10).for_each(|_| cluster.must_put(b"k2", b"v2"));
    // Sleep for a while to ensure all logs are compacted.
    thread::sleep(Duration::from_millis(100));

    fail::cfg("snapshot_delete_after_send", "pause").unwrap();

    // Let store 4 inform leader to generate a snapshot.
    cluster.run_node(4).unwrap();
    must_get_equal(&cluster.get_engine(4), b"k2", b"v2");

    fail::cfg("snapshot_enter_do_build", "pause").unwrap();
    cluster.run_node(5).unwrap();
    thread::sleep(Duration::from_millis(100));

    fail::cfg("snapshot_delete_after_send", "off").unwrap();
    must_empty_dir(cluster.get_snap_dir(1));

    // The task is droped so that we can't get the snapshot on store 5.
    fail::cfg("snapshot_enter_do_build", "pause").unwrap();
    must_get_none(&cluster.get_engine(5), b"k2");

    fail::cfg("snapshot_enter_do_build", "off").unwrap();
    must_get_equal(&cluster.get_engine(5), b"k2", b"v2");

    fail::remove("snapshot_enter_do_build");
    fail::remove("snapshot_delete_after_send");
}

fn must_empty_dir(path: String) {
    for _ in 0..500 {
        thread::sleep(Duration::from_millis(10));
        if fs::read_dir(&path).unwrap().count() == 0 {
            return;
        }
    }

    let entries = fs::read_dir(&path)
        .and_then(|dir| dir.collect::<io::Result<Vec<_>>>())
        .unwrap();
    if !entries.is_empty() {
        panic!(
            "the directory {:?} should be empty, but has entries: {:?}",
            path, entries
        );
    }
}

fn assert_snapshot(snap_dir: &str, region_id: u64, exist: bool) {
    let region_id = format!("{}", region_id);
    let timer = Instant::now();
    loop {
        for p in fs::read_dir(snap_dir).unwrap() {
            let name = p.unwrap().file_name().into_string().unwrap();
            let mut parts = name.split('_');
            parts.next();
            if parts.next().unwrap() == region_id && exist {
                return;
            }
        }
        if !exist {
            return;
        }

        if timer.saturating_elapsed() < Duration::from_secs(6) {
            thread::sleep(Duration::from_millis(20));
        } else {
            panic!(
                "assert snapshot [exist: {}, region: {}] fail",
                exist, region_id
            );
        }
    }
}

// A peer on store 3 is isolated and is applying snapshot. (add failpoint so
// it's always pending) Then two conf change happens, this peer is removed and a
// new peer is added on store 3. Then isolation clear, this peer will be
// destroyed because of a bigger peer id in msg. In previous implementation,
// peer fsm can be destroyed synchronously because snapshot state is pending and
// can be canceled, but panic may happen if the applyfsm runs very slow.
#[test]
fn test_destroy_peer_on_pending_snapshot() {
    let mut cluster = new_server_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    fail::cfg_callback("engine_rocks_raft_engine_clean_seek", move || {
        if std::thread::current().name().unwrap().contains("raftstore") {
            panic!("seek should not happen in raftstore threads");
        }
    })
    .unwrap();
    let r1 = cluster.run_conf_change();
    pd_client.must_add_peer(r1, new_peer(2, 2));
    pd_client.must_add_peer(r1, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    // Ensure peer 3 is initialized.
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    cluster.must_transfer_leader(1, new_peer(1, 1));

    cluster.add_send_filter(IsolationFilterFactory::new(3));

    for i in 0..20 {
        cluster.must_put(format!("k1{}", i).as_bytes(), b"v1");
    }

    let apply_snapshot_fp = "apply_pending_snapshot";
    fail::cfg(apply_snapshot_fp, "return()").unwrap();

    cluster.clear_send_filters();
    // Wait for leader send snapshot.
    sleep_ms(100);

    cluster.add_send_filter(IsolationFilterFactory::new(3));
    // Don't send check stale msg to PD
    let peer_check_stale_state_fp = "peer_check_stale_state";
    fail::cfg(peer_check_stale_state_fp, "return()").unwrap();

    pd_client.must_remove_peer(r1, new_peer(3, 3));
    pd_client.must_add_peer(r1, new_peer(3, 4));

    let before_handle_normal_3_fp = "before_handle_normal_3";
    fail::cfg(before_handle_normal_3_fp, "pause").unwrap();

    cluster.clear_send_filters();
    // Wait for leader send msg to peer 3.
    // Then destroy peer 3 and create peer 4.
    sleep_ms(100);

    fail::remove(apply_snapshot_fp);
    fail::remove(before_handle_normal_3_fp);

    cluster.must_put(b"k120", b"v1");
    // After peer 4 has applied snapshot, data should be got.
    must_get_equal(&cluster.get_engine(3), b"k120", b"v1");
}

// The peer 3 in store 3 is isolated for a while and then recovered.
// During its applying snapshot, however the peer is destroyed and thus applying
// snapshot is canceled. And when it's destroyed (destroy is not finished
// either), the machine restarted. After the restart, the snapshot should be
// applied successfully.println! And new data should be written to store 3
// successfully.
#[test]
fn test_destroy_peer_on_pending_snapshot_and_restart() {
    let mut cluster = new_server_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let r1 = cluster.run_conf_change();
    pd_client.must_add_peer(r1, new_peer(2, 2));
    pd_client.must_add_peer(r1, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    // Ensure peer 3 is initialized.
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    cluster.must_transfer_leader(1, new_peer(1, 1));
    let destroy_peer_fp = "destroy_peer_after_pending_move";
    fail::cfg(destroy_peer_fp, "return(true)").unwrap();

    cluster.add_send_filter(IsolationFilterFactory::new(3));

    for i in 0..20 {
        cluster.must_put(format!("k1{}", i).as_bytes(), b"v1");
    }

    // skip applying snapshot into RocksDB to keep peer status is Applying
    let apply_snapshot_fp = "apply_pending_snapshot";
    fail::cfg(apply_snapshot_fp, "return()").unwrap();

    cluster.clear_send_filters();
    // Wait for leader send snapshot.
    sleep_ms(100);

    // Don't send check stale msg to PD
    let peer_check_stale_state_fp = "peer_check_stale_state";
    fail::cfg(peer_check_stale_state_fp, "return()").unwrap();

    pd_client.must_remove_peer(r1, new_peer(3, 3));
    // Without it, pd_client.must_remove_peer does not trigger destroy_peer!
    pd_client.must_add_peer(r1, new_peer(3, 4));

    let before_handle_normal_3_fp = "before_handle_normal_3";
    // to pause ApplyTaskRes::Destroy so that peer gc could finish
    fail::cfg(before_handle_normal_3_fp, "pause").unwrap();
    // Wait for leader send msg to peer 3.
    // Then destroy peer 3
    sleep_ms(100);

    fail::remove(before_handle_normal_3_fp); // allow destroy run

    // restart node 3
    cluster.stop_node(3);
    fail::remove(apply_snapshot_fp);
    fail::remove(peer_check_stale_state_fp);
    fail::remove(destroy_peer_fp);
    cluster.run_node(3).unwrap();
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");
    // After peer 3 has applied snapshot, data should be got.
    must_get_equal(&cluster.get_engine(3), b"k119", b"v1");
    // In the end the snapshot file should be gc-ed anyway, either by new peer or by
    // store
    let now = Instant::now();
    loop {
        let mut snap_files = vec![];
        let snap_dir = cluster.get_snap_dir(3);
        // snapfiles should be gc.
        snap_files.extend(fs::read_dir(snap_dir).unwrap().map(|p| p.unwrap().path()));
        if snap_files.is_empty() {
            break;
        }
        if now.saturating_elapsed() > Duration::from_secs(5) {
            panic!("snap files are not gc-ed");
        }
        sleep_ms(20);
    }

    cluster.must_put(b"k120", b"v1");
    // new data should be replicated to peer 4 in store 3
    must_get_equal(&cluster.get_engine(3), b"k120", b"v1");
}

#[test]
fn test_shutdown_when_snap_gc() {
    let mut cluster = new_node_cluster(0, 2);
    // So that batch system can handle a snap_gc event before shutting down.
    cluster.cfg.raft_store.store_batch_system.max_batch_size = Some(1);
    cluster.cfg.raft_store.snap_mgr_gc_tick_interval = ReadableDuration::millis(20);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();

    // Only save a snapshot on peer 2, but do not apply it really.
    fail::cfg("skip_schedule_applying_snapshot", "return").unwrap();
    pd_client.must_add_peer(r1, new_learner_peer(2, 2));

    // Snapshot directory on store 2 shouldn't be empty.
    let snap_dir = &cluster.get_snap_dir(2);
    for i in 0..=100 {
        if i == 100 {
            panic!("store 2 snap dir must not be empty");
        }
        let dir = fs::read_dir(snap_dir).unwrap();
        if dir.count() > 0 {
            break;
        }
        sleep_ms(10);
    }

    fail::cfg("peer_2_handle_snap_mgr_gc", "pause").unwrap();
    std::thread::spawn(|| {
        // Sleep a while to wait snap_gc event to reach batch system.
        sleep_ms(500);
        fail::cfg("peer_2_handle_snap_mgr_gc", "off").unwrap();
    });

    sleep_ms(100);
    cluster.stop_node(2);

    let snap_dir = cluster.get_snap_dir(2);
    let dir = fs::read_dir(snap_dir).unwrap();
    if dir.count() == 0 {
        panic!("store 2 snap dir must not be empty");
    }
}

// Test if a peer handle the old snapshot properly.
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_receive_old_snapshot() {
    let mut cluster = new_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    cluster.cfg.raft_store.right_derive_when_split = true;

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();

    // Bypass the snapshot gc because the snapshot may be used twice.
    let peer_2_handle_snap_mgr_gc_fp = "peer_2_handle_snap_mgr_gc";
    fail::cfg(peer_2_handle_snap_mgr_gc_fp, "return()").unwrap();

    pd_client.must_add_peer(r1, new_peer(2, 2));
    pd_client.must_add_peer(r1, new_peer(3, 3));

    cluster.must_transfer_leader(r1, new_peer(1, 1));

    cluster.must_put(b"k00", b"v1");
    // Ensure peer 2 is initialized.
    must_get_equal(&cluster.get_engine(2), b"k00", b"v1");

    cluster.add_send_filter(IsolationFilterFactory::new(2));

    for i in 0..20 {
        cluster.must_put(format!("k{}", i).as_bytes(), b"v1");
    }

    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let recv_filter = Box::new(
        RegionPacketFilter::new(r1, 2)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgSnapshot)
            .reserve_dropped(Arc::clone(&dropped_msgs)),
    );
    cluster.add_recv_filter_on_node(2, recv_filter);
    cluster.clear_send_filters();

    for _ in 0..20 {
        let guard = dropped_msgs.lock().unwrap();
        if !guard.is_empty() {
            break;
        }
        drop(guard);
        sleep_ms(10);
    }
    let msgs: Vec<_> = {
        let mut guard = dropped_msgs.lock().unwrap();
        if guard.is_empty() {
            drop(guard);
            panic!("do not receive snapshot msg in 200ms");
        }
        std::mem::take(guard.as_mut())
    };

    cluster.clear_recv_filter_on_node(2);

    for i in 20..40 {
        cluster.must_put(format!("k{}", i).as_bytes(), b"v1");
    }
    must_get_equal(&cluster.get_engine(2), b"k39", b"v1");

    let router = cluster.get_router(2).unwrap();
    // Send the old snapshot
    for raft_msg in msgs {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(raft_msg.into()).unwrap();
    }

    cluster.must_put(b"k40", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k40", b"v1");

    pd_client.must_remove_peer(r1, new_peer(2, 2));

    must_get_none(&cluster.get_engine(2), b"k40");

    let region = cluster.get_region(b"k1");
    cluster.must_split(&region, b"k5");

    let left = cluster.get_region(b"k1");
    pd_client.must_add_peer(left.get_id(), new_peer(2, 4));

    cluster.must_put(b"k11", b"v1");
    // If peer 2 handles previous old snapshot properly and does not leave over
    // metadata in `pending_snapshot_regions`, peer 4 should be created
    // normally.
    must_get_equal(&cluster.get_engine(2), b"k11", b"v1");

    fail::remove(peer_2_handle_snap_mgr_gc_fp);
}

/// Test if snapshot can be genereated when there is a ready with no newly
/// committed entries.
/// The failpoint `before_no_ready_gen_snap_task` is used for skipping
/// the code path that snapshot is generated when there is no ready.
#[test]
fn test_gen_snapshot_with_no_committed_entries_ready() {
    let mut cluster = new_node_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let on_raft_gc_log_tick_fp = "on_raft_gc_log_tick";
    fail::cfg(on_raft_gc_log_tick_fp, "return()").unwrap();

    let before_no_ready_gen_snap_task_fp = "before_no_ready_gen_snap_task";
    fail::cfg(before_no_ready_gen_snap_task_fp, "return()").unwrap();

    cluster.run();

    cluster.add_send_filter(IsolationFilterFactory::new(3));

    for i in 1..10 {
        cluster.must_put(format!("k{}", i).as_bytes(), b"v1");
    }

    fail::remove(on_raft_gc_log_tick_fp);
    sleep_ms(100);

    cluster.clear_send_filters();
    // Snapshot should be generated and sent after leader 1 receives the heartbeat
    // response from peer 3.
    must_get_equal(&cluster.get_engine(3), b"k9", b"v1");
}

// Test snapshot generating can be canceled by Raft log GC correctly. It does
// 1. pause snapshot generating with a failpoint, and then add a new peer;
// 2. append more Raft logs to the region to trigger raft log compactions;
// 3. disable the failpoint to continue snapshot generating;
// 4. the generated snapshot should have a larger index than the latest
// `truncated_idx`.
#[test]
fn test_cancel_snapshot_generating() {
    let mut cluster = new_node_cluster(0, 5);
    cluster.cfg.raft_store.snap_mgr_gc_tick_interval = ReadableDuration(Duration::from_secs(100));
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::millis(10);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(10);
    cluster.cfg.raft_store.merge_max_log_gap = 5;

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let rid = cluster.run_conf_change();
    let snap_dir = cluster.get_snap_dir(1);

    pd_client.must_add_peer(rid, new_peer(2, 2));
    cluster.must_put(b"k0", b"v0");
    must_get_equal(&cluster.get_engine(2), b"k0", b"v0");
    pd_client.must_add_peer(rid, new_peer(3, 3));
    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Remove snapshot files generated for initial configuration changes.
    for entry in fs::read_dir(&snap_dir).unwrap() {
        let entry = entry.unwrap();
        fs::remove_file(entry.path()).unwrap();
    }

    fail::cfg("before_region_gen_snap", "pause").unwrap();
    pd_client.must_add_peer(rid, new_learner_peer(4, 4));

    // Snapshot generatings will be canceled by raft log GC.
    let mut truncated_idx = cluster.truncated_state(rid, 1).get_index();
    truncated_idx += 20;
    (0..20).for_each(|_| cluster.must_put(b"kk", b"vv"));
    cluster.wait_log_truncated(rid, 1, truncated_idx);

    fail::cfg("before_region_gen_snap", "off").unwrap();
    // Wait for all snapshot generating tasks are consumed.
    thread::sleep(Duration::from_millis(100));

    // New generated snapshot files should have a larger index than truncated index.
    for entry in fs::read_dir(&snap_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let file_name = path.file_name().unwrap().to_str().unwrap();
        if !file_name.ends_with(".meta") {
            continue;
        }
        let parts: Vec<_> = file_name[0..file_name.len() - 5].split('_').collect();
        let snap_index = parts[3].parse::<u64>().unwrap();
        assert!(snap_index > truncated_idx);
    }
}

#[test]
fn test_snapshot_gc_after_failed() {
    let mut cluster = new_server_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    cluster.cfg.raft_store.snap_gc_timeout = ReadableDuration::millis(300);

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    pd_client.must_add_peer(r1, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    pd_client.must_add_peer(r1, new_peer(3, 3));
    let snap_dir = cluster.get_snap_dir(3);
    fail::cfg("get_snapshot_for_gc", "return(0)").unwrap();
    for idx in 1..3 {
        // idx 1 will fail in fail_point("get_snapshot_for_gc"), but idx 2 will succeed
        for suffix in &[".meta", "_default.sst"] {
            let f = format!("gen_{}_{}_{}{}", 2, 6, idx, suffix);
            let mut snap_file_path = PathBuf::from(&snap_dir);
            snap_file_path.push(&f);
            let snap_file_path = snap_file_path.as_path();
            let mut file = match File::create(snap_file_path) {
                Err(why) => panic!("couldn't create {:?}: {}", snap_file_path, why),
                Ok(file) => file,
            };

            // write any data, in fact we don't check snapshot file corrupted or not in GC;
            if let Err(why) = file.write_all(b"some bytes") {
                panic!("couldn't write to {:?}: {}", snap_file_path, why)
            }
        }
    }
    let now = Instant::now();
    loop {
        let snap_keys = cluster.get_snap_mgr(3).list_idle_snap().unwrap();
        if snap_keys.is_empty() {
            panic!("no snapshot file is found");
        }

        let mut found_unexpected_file = false;
        let mut found_expected_file = false;
        for (snap_key, _is_sending) in snap_keys {
            if snap_key.region_id == 2 && snap_key.idx == 1 {
                found_expected_file = true;
            }
            if snap_key.idx == 2 && snap_key.region_id == 2 {
                if now.saturating_elapsed() > Duration::from_secs(10) {
                    panic!("unexpected snapshot file found. {:?}", snap_key);
                }
                found_unexpected_file = true;
                break;
            }
        }

        if !found_expected_file {
            panic!("The expected snapshot file is not found");
        }

        if !found_unexpected_file {
            break;
        }

        sleep_ms(400);
    }
    fail::cfg("get_snapshot_for_gc", "off").unwrap();
    cluster.sim.wl().clear_recv_filters(3);
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_sending_fail_with_net_error() {
    let mut cluster = new_cluster(1, 2);
    configure_for_snapshot(&mut cluster.cfg);
    cluster.cfg.raft_store.snap_gc_timeout = ReadableDuration::millis(300);

    let pd_client = cluster.pd_client.clone();
    // Disable default max peer number check.
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    let (send_tx, send_rx) = mpsc::sync_channel(1);
    // only send one MessageType::MsgSnapshot message
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(r1, 1)
            .allow(1)
            .direction(Direction::Send)
            .msg_type(MessageType::MsgSnapshot)
            .set_msg_callback(Arc::new(move |m: &RaftMessage| {
                if m.get_message().get_msg_type() == MessageType::MsgSnapshot {
                    let _ = send_tx.send(());
                }
            })),
    ));

    // peer2 will interrupt in receiving snapshot
    fail::cfg("receiving_snapshot_net_error", "return()").unwrap();
    pd_client.must_add_peer(r1, new_learner_peer(2, 2));

    // ready to send notify.
    send_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    // need to wait receiver handle the snapshot request
    sleep_ms(100);

    // peer2 can't receive any snapshot, so it doesn't have any key values.
    // but the receiving_count should be zero if receiving snapshot failed.
    let engine2 = cluster.get_engine(2);
    must_get_none(&engine2, b"k1");
    assert_eq!(cluster.get_snap_mgr(2).stats().receiving_count, 0);
}

/// Logs scan are now moved to raftlog gc threads. The case is to test if logs
/// are still cleaned up when there is stale logs before first index during
/// applying snapshot. It's expected to schedule a gc task after applying
/// snapshot.
#[test]
fn test_snapshot_clean_up_logs_with_unfinished_log_gc() {
    let mut cluster = new_node_cluster(0, 3);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(15);
    cluster.cfg.raft_store.raft_log_gc_threshold = 15;
    // Speed up log gc.
    cluster.cfg.raft_store.raft_log_compact_sync_interval = ReadableDuration::millis(1);
    let pd_client = cluster.pd_client.clone();

    // Disable default max peer number check.
    pd_client.disable_default_operator();
    cluster.run();
    // Simulate raft log gc tasks are lost during shutdown.
    let fp = "worker_gc_raft_log";
    fail::cfg(fp, "return").unwrap();

    let state = cluster.truncated_state(1, 3);
    for i in 0..30 {
        let b = format!("k{}", i).into_bytes();
        cluster.must_put(&b, &b);
    }
    must_get_equal(&cluster.get_engine(3), b"k29", b"k29");
    cluster.wait_log_truncated(1, 3, state.get_index() + 1);
    cluster.stop_node(3);
    let truncated_index = cluster.truncated_state(1, 3).get_index();
    let raft_engine = cluster.engines[&3].raft.clone();
    // Make sure there are stale logs.
    raft_engine.get_entry(1, truncated_index).unwrap().unwrap();

    let last_index = cluster.raft_local_state(1, 3).get_last_index();
    for i in 30..60 {
        let b = format!("k{}", i).into_bytes();
        cluster.must_put(&b, &b);
    }
    cluster.wait_log_truncated(1, 2, last_index + 1);

    fail::remove(fp);
    // So peer (3, 3) will accept a snapshot. And all stale logs before first
    // index should be cleaned up.
    cluster.run_node(3).unwrap();
    must_get_equal(&cluster.get_engine(3), b"k59", b"k59");
    cluster.must_put(b"k60", b"v60");
    must_get_equal(&cluster.get_engine(3), b"k60", b"v60");

    let truncated_index = cluster.truncated_state(1, 3).get_index();
    let mut dest = vec![];
    raft_engine.get_all_entries_to(1, &mut dest).unwrap();
    // Only previous log should be cleaned up.
    assert!(dest[0].get_index() > truncated_index, "{:?}", dest);
}

/// Redo snapshot apply after restart when kvdb state is updated but raftdb
/// state is not.
#[test]
fn test_snapshot_recover_from_raft_write_failure() {
    let mut cluster = new_server_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    // Avoid triggering snapshot at final step.
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(10);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let r1 = cluster.run_conf_change();
    pd_client.must_add_peer(r1, new_peer(2, 2));
    pd_client.must_add_peer(r1, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    cluster.must_transfer_leader(r1, new_peer(3, 3));

    cluster.add_send_filter(IsolationFilterFactory::new(1));

    for i in 0..20 {
        cluster.must_put(format!("k1{}", i).as_bytes(), b"v1");
    }

    // Raft writes are dropped.
    let raft_before_save_on_store_1_fp = "raft_before_save_on_store_1";
    fail::cfg(raft_before_save_on_store_1_fp, "return").unwrap();
    // Skip applying snapshot into RocksDB to keep peer status in Applying.
    let apply_snapshot_fp = "apply_pending_snapshot";
    fail::cfg(apply_snapshot_fp, "return()").unwrap();

    cluster.clear_send_filters();
    // Wait for leader send snapshot.
    sleep_ms(100);

    cluster.stop_node(1);
    fail::remove(raft_before_save_on_store_1_fp);
    fail::remove(apply_snapshot_fp);
    cluster.run_node(1).unwrap();
    // Snapshot is applied.
    must_get_equal(&cluster.get_engine(1), b"k119", b"v1");
    let mut ents = Vec::new();
    cluster
        .get_raft_engine(1)
        .get_all_entries_to(1, &mut ents)
        .unwrap();
    // Raft logs are cleared.
    assert!(ents.is_empty());

    // Final step: append some more entries to make sure raftdb is healthy.
    for i in 20..25 {
        cluster.must_put(format!("k1{}", i).as_bytes(), b"v1");
    }
}

/// Test whether applying snapshot is resumed properly when last_index before
/// applying snapshot is larger than the snapshot index and applying is aborted
/// between kv write and raft write.
#[test]
fn test_snapshot_recover_from_raft_write_failure_with_uncommitted_log() {
    let mut cluster = new_server_cluster(0, 3);
    configure_for_snapshot(&mut cluster.cfg);
    // Avoid triggering snapshot at final step.
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(10);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    // We use three peers([1, 2, 3]) for this test.
    cluster.run();

    sleep_ms(500);

    // Guarantee peer 1 is leader.
    cluster.must_transfer_leader(1, new_peer(1, 1));

    cluster.must_put(b"k1", b"v1");
    for i in 1..4 {
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
    }

    // Guarantee that peer 2 and 3 won't receive any entries,
    // so these entries cannot be committed.
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(1, 1)
            .msg_type(MessageType::MsgAppend)
            .direction(Direction::Send),
    ));

    // Peer 1 appends entries which is never committed.
    for i in 1..20 {
        let region = cluster.get_region(b"");
        let reqs = vec![new_put_cmd(format!("k2{}", i).as_bytes(), b"v2")];
        let mut put = new_request(
            region.get_id(),
            region.get_region_epoch().clone(),
            reqs,
            false,
        );
        put.mut_header().set_peer(new_peer(1, 1));
        let _ = cluster.call_command_on_node(1, put, Duration::from_secs(1));
    }

    for i in 1..4 {
        must_get_none(&cluster.get_engine(i), b"k210");
    }
    // Now peer 1 should have much longer log than peer 2 and 3.

    // Hack: down peer 1 in order to change leader to peer 3.
    cluster.stop_node(1);
    sleep_ms(100);
    cluster.clear_send_filters();
    sleep_ms(100);
    cluster.must_transfer_leader(1, new_peer(3, 3));

    for i in 0..20 {
        cluster.must_put(format!("k3{}", i).as_bytes(), b"v3");
    }

    // Peer 1 back to cluster
    cluster.add_send_filter(IsolationFilterFactory::new(1));
    sleep_ms(100);
    cluster.run_node(1).unwrap();
    sleep_ms(100);
    must_get_none(&cluster.get_engine(1), b"k319");
    must_get_equal(&cluster.get_engine(2), b"k319", b"v3");
    must_get_equal(&cluster.get_engine(3), b"k319", b"v3");

    // Raft writes are dropped.
    let raft_before_save_on_store_1_fp = "raft_before_save_on_store_1";
    fail::cfg(raft_before_save_on_store_1_fp, "return").unwrap();
    // Skip applying snapshot into RocksDB to keep peer status in Applying.
    let apply_snapshot_fp = "apply_pending_snapshot";
    fail::cfg(apply_snapshot_fp, "return()").unwrap();
    cluster.clear_send_filters();
    // Wait for leader send snapshot.
    sleep_ms(100);

    cluster.stop_node(1);
    fail::remove(raft_before_save_on_store_1_fp);
    fail::remove(apply_snapshot_fp);
    // Recover from applying state and validate states,
    // may fail in this step due to invalid states.
    cluster.run_node(1).unwrap();
    // Snapshot is applied.
    must_get_equal(&cluster.get_engine(1), b"k319", b"v3");
    let mut ents = Vec::new();
    cluster
        .get_raft_engine(1)
        .get_all_entries_to(1, &mut ents)
        .unwrap();
    // Raft logs are cleared.
    assert!(ents.is_empty());

    // Final step: append some more entries to make sure raftdb is healthy.
    for i in 20..25 {
        cluster.must_put(format!("k1{}", i).as_bytes(), b"v1");
    }
}

#[test]
fn test_snapshot_complete_recover_raft_tick() {
    // https://github.com/tikv/tikv/issues/14548 gives the description of what the following tests.
    let mut cluster = test_raftstore_v2::new_node_cluster(0, 3);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(50);
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::millis(10);

    cluster.run();

    let region = cluster.get_region(b"k");
    cluster.must_transfer_leader(region.get_id(), new_peer(1, 1));
    for i in 0..200 {
        let k = format!("k{:04}", i);
        cluster.must_put(k.as_bytes(), b"val");
    }

    cluster.stop_node(2);
    for i in 200..300 {
        let k = format!("k{:04}", i);
        cluster.must_put(k.as_bytes(), b"val");
    }

    fail::cfg("APPLY_COMMITTED_ENTRIES", "pause").unwrap();
    fail::cfg("RESET_APPLY_INDEX_WHEN_RESTART", "return").unwrap();
    cluster.run_node(2).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    fail::remove("APPLY_COMMITTED_ENTRIES");
    cluster.stop_node(1);

    cluster.must_put(b"k0500", b"val");
    assert_eq!(cluster.must_get(b"k0500").unwrap(), b"val".to_vec());
}

#[test]
fn test_snapshot_send_failed() {
    let mut cluster = test_raftstore_v2::new_server_cluster(1, 2);
    configure_for_snapshot(&mut cluster.cfg);
    cluster.cfg.raft_store.snap_gc_timeout = ReadableDuration::millis(300);
    cluster.cfg.raft_store.snap_mgr_gc_tick_interval = ReadableDuration::millis(100);
    cluster.cfg.raft_store.pd_heartbeat_tick_interval = ReadableDuration::millis(100);
    let pd_client = cluster.pd_client.clone();
    // Disable default max peer number check.
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"zk1", b"v1");
    let (send_tx, send_rx) = mpsc::sync_channel(1);
    // only send one MessageType::MsgSnapshot message
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(r1, 1)
            .allow(1)
            .direction(Direction::Send)
            .msg_type(MessageType::MsgSnapshot)
            .set_msg_callback(Arc::new(move |m: &RaftMessage| {
                if m.get_message().get_msg_type() == MessageType::MsgSnapshot {
                    let _ = send_tx.try_send(());
                }
            })),
    ));
    // peer2 will interrupt in receiving snapshot
    fail::cfg("receiving_snapshot_net_error", "return()").unwrap();
    pd_client.must_add_peer(r1, new_learner_peer(2, 2));

    // ready to send notify.
    send_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    // need to wait receiver handle the snapshot request
    sleep_ms(100);

    // peer2 can't receive any snapshot, so it doesn't have any key valuse.
    // but the receiving_count should be zero if receiving snapshot is failed.
    let engine2 = cluster.get_engine(2);
    must_get_none(&engine2, b"zk1");
    assert_eq!(cluster.get_snap_mgr(2).stats().receiving_count, 0);
    let mgr = cluster.get_snap_mgr(1);
    assert!(!mgr.list_snapshot().unwrap().is_empty());

    // clear fail point and wait snapshot finish.
    fail::remove("receiving_snapshot_net_error");
    cluster.clear_send_filters();
    let (sender, receiver) = mpsc::channel();
    let sync_sender = Mutex::new(sender);
    fail::cfg_callback("receiving_snapshot_net_error", move || {
        let sender = sync_sender.lock().unwrap();
        sender.send(true).unwrap();
    })
    .unwrap();
    receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    must_get_equal(&engine2, b"zk1", b"v1");

    // remove peer and check snapshot should be deleted.
    pd_client.must_remove_peer(r1, new_peer(2, 2));
    sleep_ms(100);
    assert!(mgr.list_snapshot().unwrap().is_empty());
}

#[test]
/// Test a corrupted snapshot can be detected and retry to generate a new one.
fn test_retry_corrupted_snapshot() {
    let mut cluster = new_node_cluster(0, 3);
    let pd_client = cluster.pd_client.clone();
    pd_client.disable_default_operator();

    let r = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    must_get_none(&cluster.get_engine(3), b"k1");
    pd_client.must_add_peer(r, new_peer(2, 2));
    fail::cfg("inject_sst_file_corruption", "return").unwrap();
    pd_client.must_add_peer(r, new_peer(3, 3));

    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");
}

#[test]
fn test_send_snapshot_timeout() {
    let mut cluster = new_server_cluster(1, 5);
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::millis(20);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(8);
    cluster.cfg.raft_store.merge_max_log_gap = 3;
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run();
    cluster.must_transfer_leader(1, new_peer(1, 1));
    cluster.stop_node(4);
    cluster.stop_node(5);
    (0..10).for_each(|_| cluster.must_put(b"k2", b"v2"));
    // Sleep for a while to ensure all logs are compacted.
    thread::sleep(Duration::from_millis(100));

    fail::cfg("snap_send_duration_timeout", "return(100)").unwrap();

    // Let store 4 inform leader to generate a snapshot.
    cluster.run_node(4).unwrap();
    must_get_equal(&cluster.get_engine(4), b"k2", b"v2");

    // add a delay to let send snapshot fail due to timeout.
    fail::cfg("snap_send_timer_delay", "return(1000)").unwrap();
    cluster.run_node(5).unwrap();
    thread::sleep(Duration::from_millis(150));
    must_get_none(&cluster.get_engine(5), b"k2");

    // only delay once, the snapshot should success after retry.
    fail::cfg("snap_send_timer_delay", "1*return(1000)").unwrap();
    thread::sleep(Duration::from_millis(500));
    must_get_equal(&cluster.get_engine(5), b"k2", b"v2");

    fail::remove("snap_send_timer_delay");
    fail::remove("snap_send_duration_timeout");
}

// Test that snapshots that failed to be sent are cleaned up.
#[test]
fn test_snapshot_cleanup_on_send_error() {
    let mut cluster = new_server_cluster(0, 3);
    let pd_client = cluster.pd_client.clone();
    pd_client.disable_default_operator();

    let r = cluster.run_conf_change();

    pd_client.must_add_peer(r, new_peer(2, 2));
    cluster.must_put(b"k1", b"v1");

    // Simulate an error on the gprc stream so that the leader fails to send
    // the first snapshot to peer 3.
    fail::cfg("snap_send_error", "return").unwrap();
    pd_client.must_add_peer(r, new_peer(3, 3));

    // Snapshot files are identified by a snap key which consists of region,
    // term, and applied index. By performing a new put here, we advance the
    // applied index on the leader and make it generate a new snapshot file.
    cluster.must_put(b"k2", b"v2");

    // After removing the failpoint, the new snapshot will be sent to peer 3,
    // but we want to make sure that the old snapshot gets cleaned up as well.
    fail::remove("snap_send_error");
    cluster.must_put(b"k3", b"v3");

    must_get_equal(&cluster.get_engine(3), b"k3", b"v3");

    // Check that all snapshots on the leader have been deleted.
    must_empty_dir(cluster.get_snap_dir(1));
}

#[test]
fn test_snapshot_receiver_busy() {
    let mut cluster = new_server_cluster(0, 2);
    // Test that a snapshot generation is paused when the receiver is busy. To
    // trigger the scenario, two regions are set up to send snapshots to the
    // same store concurrently while configuring the receiving limit to 1.
    cluster.cfg.server.concurrent_recv_snap_limit = 1;

    cluster.cfg.rocksdb.titan.enabled = Some(true);
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(60);

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();

    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");

    // Do a split to create the second region.
    let region = cluster.get_region(b"k1");
    cluster.must_split(&region, b"k2");

    let r2 = cluster.get_region(b"k1").id;

    // When a snapshot receiver is busy, we want the snapshot generation to
    // pause and wait until the receiver becomes available. For the two regions
    // in this test, there should only be two snapshot generations in total.
    fail::cfg("before_region_gen_snap", "2*print()->panic()").unwrap();

    // Pause the failpoint to stall the first snapshot send task.
    fail::cfg("receiving_snapshot_net_error", "pause").unwrap();
    pd_client.must_add_peer(r1, new_peer(2, 2));

    // Wait for the first snapshot to be received and paused.
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    fail::cfg_callback("receiving_snapshot_callback", move || {
        let _ = tx.lock().unwrap().send(());
    })
    .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Unblock the first snapshot task when the precheck fails on the second
    // snapshot task.
    fail::cfg_callback("snap_gen_precheck_failed", || {
        fail::remove("receiving_snapshot_net_error");
    })
    .unwrap();

    // Add the second region to store 2. The second region's snapshot precheck
    // will fail because the first snapshot task is still in progress. The
    // precheck failure will remove the receiving_snapshot_net_error failpoint,
    // unblocking the first snapshot task and eventually the second one.
    pd_client.must_add_peer(r2, new_peer(2, 1002));

    // Ensure that both regions work.
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k3", b"v3");

    cluster.must_put(b"k11", b"v11");
    must_get_equal(&cluster.get_engine(2), b"k11", b"v11");

    cluster.must_put(b"k4", b"v4");
    must_get_equal(&cluster.get_engine(2), b"k4", b"v4");

    fail::remove("before_region_gen_snap");
    fail::remove("receiving_snapshot_callback");
    fail::remove("snap_gen_precheck_failed");
}

#[test]
fn test_snapshot_receiver_not_busy_after_precheck_is_complete() {
    let mut cluster = new_server_cluster(0, 2);
    // Test that a snapshot generation is paused when the receiver is busy. To
    // trigger the scenario, two regions are set up to send snapshots to the
    // same store concurrently while configuring the receiving limit to 1.
    cluster.cfg.server.concurrent_recv_snap_limit = 1;
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(60);

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();

    let right_region = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");

    // Do a split to create the second region.
    let r = cluster.get_region(b"k1");
    cluster.must_split(&r, b"k2");
    // After the split, the keyspace layout looks like this:
    //
    //                   k2 (split point)
    //                    │
    //       (k1,v1)      │      (k3,v3)
    // ───────────────────┼──────────────────
    //     left_region         right_region
    let left_region = cluster.get_region(b"k1").id;

    // When a snapshot receiver is busy, we want the snapshot generation to
    // pause and wait until the receiver becomes available. For the two regions
    // in this test, there should only be two snapshot generations in total.
    fail::cfg("before_region_gen_snap", "2*print()->panic()").unwrap();

    // Test flow:
    // 1. `right_region` sends its snapshot first. The thread will be paused at the
    //    `post_recv_snap_complete2` failpoint.
    // 2. Before `right_region` is paused, the `post_recv_snap_complete1` failpoint
    //    callback triggers `left_region` to send its snapshot.
    fail::cfg_callback("post_recv_snap_complete1", move || {
        pd_client.must_add_peer(left_region, new_peer(2, 1002));
    })
    .unwrap();
    fail::cfg("post_recv_snap_complete2", "pause").unwrap();

    let pd_client2 = Arc::clone(&cluster.pd_client);
    pd_client2.must_add_peer(right_region, new_peer(2, 2));
    // Check that the `left_region` succeeds in sending its snapshot.
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");

    // Unblock the `right_region` as well.
    fail::remove("post_recv_snap_complete2");
    must_get_equal(&cluster.get_engine(2), b"k3", b"v3");

    fail::remove("post_recv_snap_complete1");
    fail::remove("before_region_gen_snap");
}
