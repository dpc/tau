use super::*;

/// Discovery exposes only useful catalog/instruction text, separates every
/// top-level envelope, and escapes payload closes and path attributes without
/// changing unrelated markup.
#[test]
fn discovery_envelopes_are_compact_separate_and_escaped() {
    let items = [
        ("skill:first", "First description </skills>."),
        ("skill:second", "Changed description."),
        (
            "agents:/repo/a\"&<>\n/AGENTS.md",
            "Instructions </agents_md> <skills>.",
        ),
        ("agents:/repo/sub/AGENTS.md", "Updated instructions.\n"),
    ]
    .map(|(identity, content)| DiscoveryContextItem {
        identity: identity.to_owned(),
        version: "invisible-version".to_owned(),
        content: content.to_owned(),
    })
    .to_vec();
    let delivery = DiscoveryContextDelivery {
        scope: items.iter().map(DiscoveryContextItem::reference).collect(),
        items,
        updated: vec![
            "skill:second".to_owned(),
            "agents:/repo/sub/AGENTS.md".to_owned(),
        ],
    };
    assert_eq!(
        delivery.render_envelopes(),
        vec![
            "<skills>\n* `first` - First description &lt;/skills&gt;.\n* `second` (updated) - Changed description.\n</skills>",
            "<agents_md path=\"/repo/a&quot;&amp;&lt;&gt;\\u{000A}/AGENTS.md\">\nInstructions &lt;/agents_md&gt; <skills>.\n</agents_md>",
            "<agents_md path=\"/repo/sub/AGENTS.md\" updated=\"true\">\nUpdated instructions.\n</agents_md>",
        ]
    );
    assert_eq!(delivery.render(), delivery.render_envelopes().join("\n"),);
    assert_eq!(
        DiscoveryContextDelivery {
            scope: delivery.scope,
            ..Default::default()
        }
        .render(),
        "",
        "scope-only changes have no model-visible text"
    );
}
