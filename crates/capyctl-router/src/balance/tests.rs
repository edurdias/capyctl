use super::*;
use capyctl_controller::load_table::LoadView;

fn instance(index: u32, generation: i64, host: &str) -> ServingInstance {
    ServingInstance {
        instance_index: index,
        generation,
        host_id: Some(host.into()),
        remote_host: Some(host.into()),
        launch_command_id: Some(format!("launch-{generation}")),
        dispatch_open: true,
        host_live: true,
        host_unresponsive: false,
        engine_exited: false,
        load: None,
        group: false,
    }
}

fn sample(of: &ServingInstance, running: u32, waiting: u32, kv_ppm: u32) -> LoadView {
    LoadView {
        deployment_id: "d".into(),
        generation: of.generation,
        host_id: of.remote_host.clone().unwrap(),
        owned_handle: of.launch_command_id.clone().unwrap(),
        sampled_at_ms: 0,
        age_ms: 400,
        fresh: true,
        ingress_in_flight: 0,
        engine: Some(EngineGauges {
            running,
            waiting,
            kv_usage_ppm: kv_ppm,
        }),
        max_running: None,
    }
}

fn chosen(ranking: &Ranking) -> Vec<i64> {
    ranking.order.iter().map(|c| c.generation).collect()
}

// T17, ADR 0013 §10: score = max(router in-flight, running + waiting) plus a
// KV penalty above 80 %; the penalty is bounded by its weight.
#[test]
fn the_score_takes_the_larger_of_router_and_engine_load_plus_kv_pressure() {
    let gauges = |running, waiting, kv| EngineGauges {
        running,
        waiting,
        kv_usage_ppm: kv,
    };
    assert_eq!(score(3, None), 3);
    assert_eq!(score(3, Some(gauges(1, 1, 0))), 3);
    assert_eq!(score(1, Some(gauges(4, 5, 0))), 9);
    assert_eq!(kv_penalty(800_000), 0);
    assert_eq!(kv_penalty(800_001), 1);
    assert_eq!(kv_penalty(900_000), 4);
    assert_eq!(kv_penalty(1_000_000), KV_PRESSURE_WEIGHT);
    assert_eq!(kv_penalty(u32::MAX), KV_PRESSURE_WEIGHT);
    assert_eq!(score(2, Some(gauges(0, 0, 950_000))), 2 + 6);
    assert_eq!(
        score(u64::MAX, Some(gauges(u32::MAX, u32::MAX, 1_000_000))),
        u64::MAX
    );
}

// T17: equal scores are ordered by index and rotated by the counter, so ties
// spread deterministically and the same inputs always give the same order.
#[test]
fn ties_rotate_deterministically() {
    let instances = [
        instance(0, 5, "a"),
        instance(1, 6, "b"),
        instance(2, 7, "a"),
    ];
    let idle = |_| 0;
    assert_eq!(chosen(&rank(&instances, idle, 0)), vec![5, 6, 7]);
    assert_eq!(chosen(&rank(&instances, idle, 1)), vec![6, 7, 5]);
    assert_eq!(chosen(&rank(&instances, idle, 2)), vec![7, 5, 6]);
    assert_eq!(chosen(&rank(&instances, idle, 3)), vec![5, 6, 7]);
    // Only equal scores rotate; a lower score always leads.
    let busy = |generation| if generation == 5 { 2 } else { 0 };
    assert_eq!(chosen(&rank(&instances, busy, 1)), vec![7, 6, 5]);
}

// T17 (long-prompt skew): an instance whose engine reports queued work is
// passed over while the other is lighter, even at equal router in-flight.
#[test]
fn engine_load_shifts_the_choice() {
    let mut a = instance(0, 5, "a");
    let b = instance(1, 6, "b");
    a.load = Some(sample(&a, 6, 2, 0));
    let ranking = rank(&[a.clone(), b.clone()], |_| 1, 0);
    assert_eq!(chosen(&ranking), vec![6, 5]);
    assert_eq!(ranking.order[1].score, 8);
    assert_eq!(ranking.order[1].engine.unwrap().running, 6);
    // KV pressure alone breaks an otherwise equal choice.
    let mut hot = instance(0, 5, "a");
    hot.load = Some(sample(&hot, 0, 0, 990_000));
    assert_eq!(chosen(&rank(&[hot, b], |_| 0, 0)), vec![6, 5]);
}

// T18 T34, ADR 0013 §10: a stale sample, one from another host, another
// generation or another launch, or a failed scrape is not used; the score
// falls back to router in-flight.
#[test]
fn unusable_samples_fall_back_to_router_in_flight() {
    let base = instance(0, 5, "a");
    let mut variants = Vec::new();
    let mut stale = sample(&base, 9, 0, 0);
    stale.fresh = false;
    variants.push(stale);
    let mut other_host = sample(&base, 9, 0, 0);
    other_host.host_id = "b".into();
    variants.push(other_host);
    let mut other_generation = sample(&base, 9, 0, 0);
    other_generation.generation = 4;
    variants.push(other_generation);
    let mut other_launch = sample(&base, 9, 0, 0);
    other_launch.owned_handle = "launch-old".into();
    variants.push(other_launch);
    let mut failed = sample(&base, 9, 0, 0);
    failed.engine = None;
    variants.push(failed);
    for view in variants {
        let mut a = base.clone();
        a.load = Some(view.clone());
        assert!(usable_load(&a).is_none(), "{view:?}");
        let ranking = rank(&[a], |_| 2, 0);
        assert_eq!(ranking.order[0].score, 2, "{view:?}");
        assert!(ranking.order[0].engine.is_none());
    }
    // SPEC §§10, 17 (D9): an embedded instance's sample is the role's own,
    // taken under the host it is placed on; one under another host is not
    // used, and neither is one for an instance with no placed host.
    let mut embedded = base.clone();
    embedded.remote_host = None;
    embedded.load = Some(sample(&base, 9, 0, 0));
    assert_eq!(usable_load(&embedded).map(|(g, _)| g.running), Some(9));
    let mut elsewhere = sample(&base, 9, 0, 0);
    elsewhere.host_id = "b".into();
    embedded.load = Some(elsewhere);
    assert!(usable_load(&embedded).is_none());
    embedded.host_id = None;
    embedded.load = Some(sample(&base, 9, 0, 0));
    assert!(usable_load(&embedded).is_none());
}

// T38 T33: a closed gate or a lost host session is never a candidate.
#[test]
fn closed_or_disconnected_instances_are_skipped_with_a_reason() {
    let mut closed = instance(0, 5, "a");
    closed.dispatch_open = false;
    let mut lost = instance(1, 6, "b");
    lost.host_live = false;
    let open = instance(2, 7, "a");
    let ranking = rank(&[closed, lost, open], |_| 0, 0);
    assert_eq!(chosen(&ranking), vec![7]);
    assert_eq!(
        ranking
            .skipped
            .iter()
            .map(|s| (s.generation, s.reason))
            .collect::<Vec<_>>(),
        vec![(5, "dispatch_closed"), (6, "host_session_lost")]
    );
}

// T38 T33 (owner decision 2026-09-23): a host whose heartbeats went silent is
// skipped as unresponsive, even though its dispatch is also suspended and its
// session no longer counts as current.
#[test]
fn an_unresponsive_host_is_skipped_as_such() {
    let mut frozen = instance(0, 5, "a");
    frozen.host_unresponsive = true;
    frozen.dispatch_open = false;
    frozen.host_live = false;
    let open = instance(1, 6, "b");
    let ranking = rank(&[frozen, open], |_| 0, 0);
    assert_eq!(chosen(&ranking), vec![6]);
    assert_eq!(
        ranking
            .skipped
            .iter()
            .map(|s| (s.generation, s.reason))
            .collect::<Vec<_>>(),
        vec![(5, "host_unresponsive")]
    );
}

// T32 T38 (W13): an instance whose engine exited is skipped as such while its
// cleanup settles; its sibling keeps serving.
#[test]
fn an_exited_engine_is_skipped_as_such() {
    let mut exited = instance(0, 5, "a");
    exited.engine_exited = true;
    exited.dispatch_open = false;
    let open = instance(1, 6, "b");
    let ranking = rank(&[exited, open], |_| 0, 0);
    assert_eq!(chosen(&ranking), vec![6]);
    assert_eq!(
        ranking
            .skipped
            .iter()
            .map(|s| (s.generation, s.reason))
            .collect::<Vec<_>>(),
        vec![(5, "engine_exited")]
    );
}

// T17: the choice and its count are taken together, so back-to-back choices
// on idle instances alternate rather than herd, and counts return to zero.
#[test]
fn choices_count_immediately_and_release_to_zero() {
    let counts = InstanceCounts::default();
    let instances = [instance(0, 5, "a"), instance(1, 6, "b")];
    let picks: Vec<i64> = (0..6)
        .map(|_| counts.choose("d", &instances).order[0].generation)
        .collect();
    assert_eq!(picks.iter().filter(|g| **g == 5).count(), 3);
    assert_eq!(counts.current("d", 5), 3);
    assert_eq!(counts.current("d", 6), 3);
    for _ in 0..3 {
        counts.release("d", 5);
        counts.release("d", 6);
    }
    assert_eq!((counts.current("d", 5), counts.current("d", 6)), (0, 0));
    assert!(counts.lock().counts.is_empty());
    counts.release("d", 5);
    assert_eq!(counts.current("d", 5), 0);
}
