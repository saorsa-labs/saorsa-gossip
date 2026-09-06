use super::*;

fn instance() -> (PlumtreePubSub<RecordingTransport>, Arc<RecordingTransport>) {
    let peer = test_peer_id(1);
    let transport = RecordingTransport::new(peer);
    let pubsub =
        PlumtreePubSub::new_with_task_control(peer, transport.clone(), test_signing_key(), false);
    (pubsub, transport)
}

#[tokio::test]
async fn configured_bounds_apply_to_creation_refresh_and_grafts() {
    for degree in [0, 1, 2, 8, 16] {
        let (pubsub, _) = instance();
        pubsub.set_eager_degree_ceiling(degree).await;
        let maximum = if degree == 0 { 12 } else { degree };
        let minimum = 6.min(maximum);
        let peers: Vec<_> = (10..40).map(test_peer_id).collect();
        let ready_topic = TopicId::new([1; 32]);
        let async_topic = TopicId::new([2; 32]);
        let _ready = pubsub.subscribe_ready(ready_topic).await;
        let _async = pubsub.subscribe(async_topic);
        tokio::task::yield_now().await;
        // Exercise both subscription creation paths as well as membership creation.
        for topic in [ready_topic, async_topic, TopicId::new([3; 32])] {
            pubsub.initialize_topic_peers(topic, peers.clone()).await;
            pubsub.set_topic_peers(topic, peers.clone()).await;
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).unwrap();
            assert_eq!(state.max_eager_degree, maximum);
            assert_eq!(state.eager_peers.len(), minimum);
            for peer in &peers {
                state.graft_peer_at(*peer, Instant::now());
                assert!(state.eager_peers.len() <= maximum);
            }
            assert_eq!(state.eager_peers.len(), maximum);
            assert!(state.eager_peers.is_disjoint(&state.lazy_peers));
        }
    }
}

#[tokio::test]
async fn reconfiguration_rebalances_existing_topics_and_is_instance_local() {
    let (leaf, _) = instance();
    let (full, _) = instance();
    let topic = TopicId::new([4; 32]);
    let peers: Vec<_> = (10..30).map(test_peer_id).collect();
    leaf.initialize_topic_peers(topic, peers.clone()).await;
    full.initialize_topic_peers(topic, peers.clone()).await;
    leaf.set_eager_degree_ceiling(2).await;
    assert_eq!(
        leaf.topics.read_topic(&topic).await[&topic]
            .eager_peers
            .len(),
        2
    );
    {
        let topics = full.topics.read_topic(&topic).await;
        assert_eq!(topics[&topic].eager_peers.len(), 6);
        assert_eq!(topics[&topic].max_eager_degree, 12);
    }
    leaf.set_eager_degree_ceiling(0).await;
    let topics = leaf.topics.read_topic(&topic).await;
    assert_eq!(topics[&topic].eager_peers.len(), 6);
    assert_eq!(topics[&topic].max_eager_degree, 12);
}

#[tokio::test]
async fn eager_forwarding_and_cached_iwant_preserve_ceiling() {
    let (pubsub, transport) = instance();
    pubsub.set_eager_degree_ceiling(2).await;
    let topic = TopicId::new([5; 32]);
    let peers: Vec<_> = (10..30).map(test_peer_id).collect();
    pubsub.initialize_topic_peers(topic, peers).await;
    let counts = pubsub
        .publish_local_with_fanout(topic, Bytes::from_static(b"local"))
        .await
        .unwrap();
    assert_eq!(counts.attempted, 2);
    assert_eq!(
        transport
            .send_counts
            .lock()
            .unwrap()
            .values()
            .sum::<usize>(),
        2
    );

    for id in 40..50 {
        let from = test_peer_id(id);
        let msg_id = [id; 32];
        let message =
            signed_eager_message(&pubsub.signing_key, topic, msg_id, Bytes::from(vec![id]));
        transport.send_counts.lock().unwrap().clear();
        pubsub.handle_eager(from, topic, message).await.unwrap();
        // Inbound forwarding returns before its detached send tasks complete.
        // Wait for their permits to release before observing wire fanout.
        time::timeout(Duration::from_secs(5), async {
            loop {
                if pubsub
                    .outbound_budgets
                    .peers_guard()
                    .values()
                    .all(|entry| entry.data_slots.is_empty())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("forward sends complete");
        assert_eq!(
            transport
                .send_counts
                .lock()
                .unwrap()
                .values()
                .sum::<usize>(),
            2
        );
        let requester = test_peer_id(id + 50);
        pubsub
            .handle_iwant(requester, topic, vec![msg_id])
            .await
            .unwrap();
        assert_eq!(
            transport.send_count_to(requester),
            1,
            "lazy repair remains available"
        );
        let topics = pubsub.topics.read_topic(&topic).await;
        assert_eq!(topics[&topic].eager_peers.len(), 2);
        assert!(topics[&topic].lazy_peers.contains(&requester));
    }
    // An inbound-only topic also inherits the instance ceiling at creation.
    let inbound = TopicId::new([6; 32]);
    for id in 60..70 {
        let message = signed_eager_message(
            &pubsub.signing_key,
            inbound,
            [id; 32],
            Bytes::from(vec![id]),
        );
        pubsub
            .handle_eager(test_peer_id(id), inbound, message)
            .await
            .unwrap();
    }
    assert_eq!(
        pubsub.topics.read_topic(&inbound).await[&inbound]
            .eager_peers
            .len(),
        2
    );
}

#[test]
fn cooling_rescue_replaces_at_capacity() {
    let mut state = TopicState::new();
    state.max_eager_degree = 1;
    let eager = test_peer_id(2);
    let lazy = test_peer_id(3);
    let now = Instant::now();
    state.eager_peers.insert(eager);
    state.lazy_peers.insert(lazy);
    for (peer, seconds) in [(eager, 60), (lazy, 30)] {
        let mut cooling = PeerCoolingState::new(now);
        cooling.suppressed_until = Some(now + Duration::from_secs(seconds));
        state.peer_cooling.insert(peer, cooling);
    }
    let (tx, _rx) = mpsc::unbounded_channel();
    state.subscribers.push(tx);
    assert_eq!(
        state.rescue_suppressed_eager_peer_if_needed_at(now),
        Some(lazy)
    );
    assert_eq!(state.eager_peers, HashSet::from([lazy]));
    assert!(state.lazy_peers.contains(&eager));
}

#[test]
fn opportunistic_replacement_works_below_stock_minimum() {
    let mut state = TopicState::new();
    state.max_eager_degree = 1;
    let now = Instant::now();
    let eager = test_peer_id(2);
    let lazy = test_peer_id(3);
    state.eager_peers.insert(eager);
    state.lazy_peers.insert(lazy);
    let mut bad = PeerScore::new_at(now);
    for _ in 0..PEER_TIMEOUT_THRESHOLD {
        bad.record_outbound_send_timeout_at(now);
    }
    bad.record_cooling_event_at(now);
    state.peer_scores.insert(eager, bad);
    let mut good = PeerScore::new_at(now);
    good.record_delivery();
    for _ in 0..6 {
        good.record_outbound_send_success_at(now, SendAttemptKind::Normal);
    }
    state.peer_scores.insert(lazy, good);
    assert_eq!(state.maintain_degree_at(now), (1, 1));
    assert_eq!(state.eager_peers, HashSet::from([lazy]));
}

#[tokio::test(start_paused = true)]
async fn background_maintainer_uses_updated_ceiling() {
    let (pubsub, _) = instance();
    let topic = TopicId::new([7; 32]);
    pubsub
        .initialize_topic_peers(topic, (10..30).map(test_peer_id).collect())
        .await;
    pubsub.spawn_degree_maintainer();
    tokio::task::yield_now().await;
    pubsub.set_eager_degree_ceiling(2).await;
    for _ in 0..2 {
        // Force a promotion on each actual timer tick, including the 30 s repeat.
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).unwrap();
            for peer in state.eager_peers.clone() {
                state.prune_peer(peer);
            }
        }
        time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            pubsub.topics.read_topic(&topic).await[&topic]
                .eager_peers
                .len(),
            2
        );
    }
}
