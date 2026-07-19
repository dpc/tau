use super::*;

fn facts_budget(max_record_bytes: u64, remaining_bytes: u64) -> AgentCreationFactsBudget {
    AgentCreationFactsBudget {
        max_record_bytes,
        remaining_bytes,
    }
}

/// Ensures the writer rejects a record length that the matching loader would
/// reject, before opening or mutating the journal.
#[test]
fn write_record_limit_matches_read_limit() {
    let error = validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES + 1)
        .expect_err("oversized record must be rejected");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
}

/// The roster enrichment path reads immutable creation fields and the latest
/// in-memory display projection without scanning transcript history.
#[test]
fn creation_facts_read_valid_first_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: Some(AgentId::parse("parent").expect("parent id")),
                role: "engineer".to_owned(),
                display_name: Some("Initial".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "Latest".to_owned(),
            }),
            UnixMicros::new(43),
        )
        .expect("name appends");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("within budget");

    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            started_at: Some(started_at),
            role,
            display_name: Some(display_name),
            ..
        } if started_at == UnixMicros::new(42)
            && role == "engineer"
            && display_name == "Latest"
    ));
}

/// Cold roster enrichment accepts a journal-bound checkpoint but suppresses a
/// structurally valid checkpoint whose boundary witness no longer matches.
#[test]
fn creation_facts_require_journal_bound_cold_checkpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                Event::AgentStarted(tau_proto::AgentStarted {
                    agent_id: agent_id.clone(),
                    parent_agent: None,
                    role: "engineer".to_owned(),
                    display_name: Some("Cold".to_owned()),
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
                UnixMicros::new(42),
            )
            .expect("creation appends");
    }

    let store = AgentStore::open_lazy(temp.path()).expect("cold store opens");
    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("fresh checkpoint");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: Some(display_name),
            ..
        } if display_name == "Cold"
    ));

    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("invalid checkpoint stays categorical");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: None,
            ..
        }
    ));
}

/// Missing journals remain categorical rows rather than becoming I/O errors.
#[test]
fn creation_facts_report_missing_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("missing").expect("agent id");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("missing does not consume budget");

    assert_eq!(facts, AgentCreationFacts::Missing);
    assert_eq!(facts.bytes_read(), 0);
}

/// Aggregate roster enrichment fails before allocating a first record that does
/// not fit the caller's remaining budget.
#[test]
fn creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    let result = store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1));

    assert_eq!(result, Err(AgentCreationFactsBudgetExceeded));
}

/// In-memory ephemeral creation projections consume the same aggregate budget
/// even though no journal read is needed.
#[test]
fn ephemeral_creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("ephemeral").expect("agent id");
    store
        .mark_agent_ephemeral(agent_id.as_str())
        .expect("mark ephemeral");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: true,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    assert_eq!(
        store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1)),
        Err(AgentCreationFactsBudgetExceeded)
    );
}

/// Truncated durable records charge their advertised bounded length so repeated
/// malformed members cannot bypass the aggregate roster budget.
#[test]
fn truncated_creation_records_consume_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let record_length = 256 * 1024_u64;
    let mut remaining = 4 * 1024 * 1024_u64;
    for index in 0..17 {
        let agent_id = AgentId::parse(format!("truncated-{index}")).expect("agent id");
        let dir = store.agent_dir(agent_id.as_str());
        fs::create_dir_all(&dir).expect("agent dir");
        let mut bytes = record_length.to_le_bytes().to_vec();
        bytes.resize(8 + record_length as usize - 1, 0);
        fs::write(dir.join("events.cbor"), bytes).expect("truncated journal");

        let facts = store.agent_creation_facts(&agent_id, facts_budget(record_length, remaining));
        if index < 16 {
            let facts = facts.expect("record fits remaining budget");
            assert_eq!(
                facts,
                AgentCreationFacts::Unreadable {
                    bytes_read: record_length
                }
            );
            remaining = remaining.saturating_sub(facts.bytes_read());
        } else {
            assert_eq!(facts, Err(AgentCreationFactsBudgetExceeded));
        }
    }
}
