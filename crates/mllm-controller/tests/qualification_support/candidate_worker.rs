use super::*;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

#[tokio::test]
async fn candidate_probe_samples_clock_after_terminal_and_rejects_clock_failure() {
    for fail in [false, true] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.initialized(&fake).await;
        let CandidateDispatchResult::New(dispatch) = f
            .store
            .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
            .unwrap()
        else {
            panic!("new probe required")
        };
        let sampled = Arc::new(AtomicI64::new(0));
        let clock_sampled = sampled.clone();
        let result = mllm_controller::qualification::collect_probe_with_clock(
            &fake,
            *dispatch,
            &move || {
                clock_sampled.fetch_add(1, Ordering::SeqCst);
                if fail {
                    Err(mllm_store::lifecycle::LifecycleError::Invalid)
                } else {
                    Ok(1350)
                }
            },
        )
        .await;
        assert_eq!(sampled.load(Ordering::SeqCst), 1);
        if fail {
            assert!(result.is_err());
            assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
        } else {
            let result = result.unwrap();
            assert_eq!(result.observed_at_ms, 1350);
            f.store
                .record_candidate_result(&f.session, &collector, &result, 1350)
                .unwrap();
            assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
        }
    }
}

#[tokio::test]
async fn candidate_probe_send_revalidates_time_current_lease_and_session() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.initialized(&fake).await;
    let CandidateDispatchResult::New(dispatch) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!("new probe required")
    };
    assert!(f
        .store
        .revalidate_candidate_probe_send(&f.session, &dispatch, 1200)
        .is_ok());
    f.sql.execute_batch("INSERT INTO request_leases SELECT id || '-unknown',deployment_id,revision,generation,session_id,'uncertain' FROM request_leases").unwrap();
    assert!(f.store.revalidate_candidate_probe_send(&f.session,&dispatch,1200).is_err(), "probe must reject unrelated unproven work before send");
    f.sql.execute("DELETE FROM request_leases WHERE id LIKE '%-unknown'", []).unwrap();
    for now in [-1, 1199, 400000] {
        assert!(f
            .store
            .revalidate_candidate_probe_send(&f.session, &dispatch, now)
            .is_err());
    }
    f.sql
        .execute("UPDATE request_leases SET disposition='uncertain'", [])
        .unwrap();
    assert!(f
        .store
        .revalidate_candidate_probe_send(&f.session, &dispatch, 1200)
        .is_err());
    let current = f.store.begin_coordinator_session().unwrap();
    assert!(f
        .store
        .revalidate_candidate_probe_send(&f.session, &dispatch, 1200)
        .is_err());
    assert!(f
        .store
        .revalidate_candidate_probe_send(&current, &dispatch, 1200)
        .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
}
