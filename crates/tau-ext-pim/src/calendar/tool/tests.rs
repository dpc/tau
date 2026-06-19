use super::*;

#[test]
fn calendar_schema_hides_timezone_and_has_command_conditionals() {
    // Weak local models need command-specific split-tool schemas rather than
    // prose-only guidance. Keep timezone out of model-visible args, but keep
    // range starts optional because the runtime supplies a bounded default.
    let schemas = calendar_tool_specs();
    let list_events_schema = schemas
        .iter()
        .find(|spec| spec.name.as_str() == "calendar_search")
        .and_then(|spec| spec.parameters.as_ref())
        .expect("search parameters");
    let properties = list_events_schema
        .pointer("/properties")
        .and_then(serde_json::Value::as_object)
        .expect("list events properties");

    assert!(!properties.contains_key("timezone"));
    assert!(
        list_events_schema
            .pointer("/required")
            .is_some_and(|required| {
                required
                    .as_array()
                    .is_some_and(|required| required.is_empty())
            })
    );

    let search_title_description = list_events_schema
        .pointer("/properties/title/description")
        .and_then(serde_json::Value::as_str)
        .expect("search title description");
    assert!(search_title_description.contains("substring filter"));
    assert!(!search_title_description.contains("calendar_create"));

    let create_schema = schemas
        .iter()
        .find(|spec| spec.name.as_str() == "calendar_create")
        .and_then(|spec| spec.parameters.as_ref())
        .expect("create parameters");
    let create_title_description = create_schema
        .pointer("/properties/title/description")
        .and_then(serde_json::Value::as_str)
        .expect("create title description");
    assert_eq!(create_title_description, "Event title.");

    let free_busy_schema = schemas
        .iter()
        .find(|spec| spec.name.as_str() == "calendar_free_busy")
        .and_then(|spec| spec.parameters.as_ref())
        .expect("free busy parameters");
    assert!(
        free_busy_schema
            .pointer("/required")
            .is_some_and(|required| {
                required
                    .as_array()
                    .is_some_and(|required| required.is_empty())
            })
    );

    let update_schema = schemas
        .iter()
        .find(|spec| spec.name.as_str() == "calendar_update")
        .and_then(|spec| spec.parameters.as_ref())
        .expect("update parameters");
    assert_eq!(
        update_schema.pointer("/required").expect("required"),
        &serde_json::json!(["event_id", "field", "new_value"])
    );
    assert!(update_schema.pointer("/dependentRequired/end").is_none());
    assert_eq!(
        update_schema
            .pointer("/properties/field/enum")
            .expect("fields"),
        &serde_json::json!(["title", "description", "location", "start", "attendees"])
    );
    assert!(update_schema.pointer("/anyOf").is_none());
}

#[test]
fn calendar_tool_examples_validate_and_legacy_examples_parse() {
    // Examples are provider-owned repair hints. Validate them against the
    // registered schemas and ensure legacy envelope examples use runtime args,
    // not only split-tool adapter args.
    for spec in calendar_tool_specs()
        .into_iter()
        .chain([calendar_tool_spec()])
    {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }

    for example in calendar_tool_spec().examples {
        let invocation: ToolInvocation = example.arguments.deserialized().unwrap_or_else(|error| {
            panic!("legacy example `{}` did not parse: {error}", example.id)
        });
        let args = invocation
            .args
            .unwrap_or_else(|| CborValue::Map(Vec::new()));
        match invocation.command {
            CalendarCommand::ListCalendars => {
                let _: ListCalendarsArgs = args.deserialized().expect("list args");
            }
            CalendarCommand::ListEvents | CalendarCommand::FreeBusy => {
                let _: CalendarRangeArgs = args.deserialized().expect("range args");
            }
            CalendarCommand::ReadEvent => {
                let _: ReadEventArgs = args.deserialized().expect("read args");
            }
            CalendarCommand::CreateEvent => {
                let _: CreateEventArgs = args.deserialized().expect("create args");
            }
            CalendarCommand::UpdateEvent => {
                let _: UpdateEventArgs = args.deserialized().expect("update args");
            }
            CalendarCommand::DeleteEvent => {
                let _: DeleteEventArgs = args.deserialized().expect("delete args");
            }
            CalendarCommand::RespondInvite => {
                let _: RespondInviteArgs = args.deserialized().expect("respond args");
            }
        }
    }
}
