use tau_proto::{AgentPromptId, ToolCallId};

use super::PromptRuntimeState;

fn prompt_id(index: usize) -> AgentPromptId {
    AgentPromptId::parse(format!("prompt-{index}")).expect("test prompt id")
}

/// Prompt snapshots must survive every removal before the last actual call,
/// retire on that last call, and clear all reverse links without
/// cross-prompt scans.
#[test]
fn many_prompt_snapshots_follow_exact_call_membership() {
    const PROMPTS: usize = 64;
    const CALLS_PER_PROMPT: usize = 64;
    let mut state = PromptRuntimeState::default();
    let mut calls = Vec::new();
    for prompt_index in 0..PROMPTS {
        let prompt_id = prompt_id(prompt_index);
        state.tool_specs.insert(prompt_id.clone(), Vec::new());
        state
            .tool_invocation_policies
            .insert(prompt_id.clone(), Default::default());
        for call_index in 0..CALLS_PER_PROMPT {
            let call_id: ToolCallId = format!("call-{prompt_index:02}-{call_index:02}").into();
            state.record_tool_call_prompt(call_id.clone(), prompt_id.clone());
            calls.push((prompt_id.clone(), call_id));
        }
    }

    calls.sort_by_key(|(_, call_id)| {
        call_id
            .as_str()
            .bytes()
            .fold(0_u64, |hash, byte| hash.wrapping_mul(131) + u64::from(byte))
    });
    let mut remaining = vec![CALLS_PER_PROMPT; PROMPTS];
    for (prompt_id, call_id) in calls {
        let prompt_index = prompt_id
            .as_str()
            .strip_prefix("prompt-")
            .expect("known prefix")
            .parse::<usize>()
            .expect("numeric suffix");
        state.remove_tool_call_prompt(&call_id);
        remaining[prompt_index] -= 1;
        assert_eq!(
            state.tool_specs.contains_key(&prompt_id),
            remaining[prompt_index] != 0
        );
        assert_eq!(
            state.tool_invocation_policies.contains_key(&prompt_id),
            remaining[prompt_index] != 0
        );
    }
    assert!(state.tool_call_prompts.is_empty());
    assert!(state.tool_calls_by_prompt.is_empty());
    assert_eq!(state.tool_call_index_work(), PROMPTS * CALLS_PER_PROMPT * 2);

    let first = prompt_id(100);
    let second = prompt_id(101);
    let replaced: ToolCallId = "defensive-replacement".into();
    let first_survivor: ToolCallId = "first-survivor".into();
    let second_survivor: ToolCallId = "second-survivor".into();
    for prompt_id in [&first, &second] {
        state.tool_specs.insert(prompt_id.clone(), Vec::new());
        state
            .tool_invocation_policies
            .insert(prompt_id.clone(), Default::default());
    }
    state.record_tool_call_prompt(replaced.clone(), first.clone());
    state.record_tool_call_prompt(first_survivor.clone(), first.clone());
    state.record_tool_call_prompt(second_survivor.clone(), second.clone());
    state.record_tool_call_prompt(replaced.clone(), second.clone());
    assert!(state.tool_specs.contains_key(&first));
    assert!(state.tool_specs.contains_key(&second));
    assert_eq!(state.tool_call_prompt(&first_survivor), Some(&first));
    assert_eq!(state.tool_call_prompt(&replaced), Some(&second));

    for index in 0..128 {
        state.record_tool_call_prompt(format!("clear-{index}").into(), second.clone());
    }
    let work_before_clear = state.tool_call_index_work();
    state.clear_prompt_tool_snapshot(&second);
    assert_eq!(state.tool_call_index_work() - work_before_clear, 130);
    assert_eq!(state.tool_call_prompt(&first_survivor), Some(&first));
    assert!(state.tool_specs.contains_key(&first));
    assert!(state.tool_invocation_policies.contains_key(&first));
    assert!(!state.tool_specs.contains_key(&second));
    assert!(!state.tool_invocation_policies.contains_key(&second));

    state.clear_prompt_tool_snapshot(&first);
    assert!(state.tool_call_prompts.is_empty());
    assert!(state.tool_calls_by_prompt.is_empty());
}
