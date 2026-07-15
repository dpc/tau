use tau_core::SessionStore;
use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionAgentUnloaded, SessionId};

fn main() {
    let root = std::env::args().nth(1).expect("fixture path");
    let mut store = SessionStore::open(root).expect("open fixture store");
    let agent_id = AgentId::parse("main").expect("valid fixture agent id");
    for session_index in 0..100 {
        let session_id = format!("benchmark-session-{session_index:04}");
        for event_index in 0..100 {
            let event = if event_index % 2 == 0 {
                Event::SessionAgentLoaded(SessionAgentLoaded {
                    session_id: SessionId::from(session_id.clone()),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                })
            } else {
                Event::SessionAgentUnloaded(SessionAgentUnloaded {
                    session_id: SessionId::from(session_id.clone()),
                    agent_id: agent_id.clone(),
                })
            };
            store
                .append_session_event(&session_id, None, event)
                .expect("append fixture event");
        }
    }
}
