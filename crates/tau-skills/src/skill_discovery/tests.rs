//! Exact retained-scope timestamp and metadata serialization.

use std::path::Path;
use std::time::Duration;

use super::*;

/// Raw scope serialization preserves every ordering input, including
/// absent, negative and sub-microsecond times rather than rounded wire
/// timestamps.
#[test]
fn retained_discovery_roundtrip_preserves_exact_timestamps_and_policies() {
    let parsed = crate::load_skill_from_content(
        "---\nname: retained\ndescription: metadata\nadvertise: false\nuser-invocable: false\n---\n",
        Path::new("/sample/retained.md"),
    ).0.expect("skill");
    for modified in [
        None,
        Some(SystemTime::UNIX_EPOCH - Duration::new(1, 17)),
        Some(SystemTime::UNIX_EPOCH + Duration::new(1, 19)),
    ] {
        let scope = SkillDiscovery {
            entries: vec![DiscoveryEntry::Candidate(SampledSkill {
                skill: parsed.clone(),
                modified,
                source_precedence: Some(7),
            })],
        };
        let encoded = serde_yaml_ng::to_string(&scope).expect("serialize");
        let restored: SkillDiscovery = serde_yaml_ng::from_str(&encoded).expect("restore");
        let DiscoveryEntry::Candidate(candidate) = &restored.entries[0] else {
            panic!("candidate");
        };
        assert_eq!(candidate.skill, parsed);
        assert_eq!(candidate.modified, modified);
        assert_eq!(candidate.source_precedence, Some(7));
    }
}
