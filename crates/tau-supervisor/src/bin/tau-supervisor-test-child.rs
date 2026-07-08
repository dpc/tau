//! Integration-test-only child binary coupled to `tests/supervisor.rs`.

use std::error::Error;
use std::io::{BufReader, BufWriter, Write};

use tau_proto::{
    CborValue, ClientKind, Event, EventDelivery, HarnessInputMessage, HarnessOutputMessage, Hello,
    PROTOCOL_VERSION, PeerInputReader, PeerOutputWriter, Ready, Subscribe, ToolRegister,
    ToolResult, ToolSpec, ToolStarted,
};

const EXIT_IMMEDIATELY_ARG: &str = "--exit-immediately";
const PARTIAL_FRAME_ARG: &str = "--partial-frame";
const FLOOD_ARG: &str = "--flood";
const REPORT_SECRET_ENV_ARG: &str = "--report-secret-env";
const SLEEP_ARG: &str = "--sleep";
const REPORT_CWD_ARG: &str = "--report-cwd";
const STDERR_MARKER_ARG: &str = "--stderr-marker";

fn write_ready(message: impl Into<String>) -> Result<(), Box<dyn Error>> {
    let stdout = std::io::stdout();
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));
    writer.write_message(&HarnessInputMessage::Ready(Ready {
        message: Some(message.into()),
    }))?;
    writer.flush()?;
    Ok(())
}

fn write_partial_frame() -> Result<(), Box<dyn Error>> {
    std::io::stdout().write_all(&[0x81])?;
    Ok(())
}

fn write_flood_messages(message_count: usize) -> Result<(), Box<dyn Error>> {
    let stdout = std::io::stdout();
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));
    for index in 0..message_count {
        writer.write_message(&HarnessInputMessage::Ready(Ready {
            message: Some(index.to_string()),
        }))?;
    }
    writer.flush()?;
    Ok(())
}

fn report_secret_env() -> Result<(), Box<dyn Error>> {
    let secret_visible =
        std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("TAU_SECRET_"));
    write_ready(if secret_visible { "present" } else { "absent" })
}

fn run_test_mode(
    first_arg: Option<String>,
    mut remaining_args: impl Iterator<Item = String>,
) -> Result<bool, Box<dyn Error>> {
    match first_arg.as_deref() {
        Some(EXIT_IMMEDIATELY_ARG) => Ok(true),
        Some(PARTIAL_FRAME_ARG) => {
            write_partial_frame()?;
            Ok(true)
        }
        Some(FLOOD_ARG) => {
            let message_count = remaining_args
                .next()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing flood count")
                })?
                .parse::<usize>()?;
            write_flood_messages(message_count)?;
            Ok(true)
        }
        Some(REPORT_SECRET_ENV_ARG) => {
            report_secret_env()?;
            Ok(true)
        }
        Some(SLEEP_ARG) => {
            std::thread::sleep(std::time::Duration::from_secs(60));
            Ok(true)
        }
        Some(REPORT_CWD_ARG) => {
            write_ready(std::env::current_dir()?.display().to_string())?;
            Ok(true)
        }
        Some(STDERR_MARKER_ARG) => {
            eprintln!("tau-supervisor-stderr-marker");
            write_ready("stderr-written")?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn write_startup_messages(writer: &mut PeerOutputWriter<impl Write>) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test-child".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_message(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: Vec::new(),
        live_selectors: vec![tau_proto::EventSelector::Exact(
            tau_proto::EventName::TOOL_STARTED,
        )],
    }))?;
    writer.write_message(&HarnessInputMessage::emit(Event::ToolRegister(
        ToolRegister {
            tool: ToolSpec {
                name: tau_proto::ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo test payloads".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        },
    )))?;
    writer.write_message(&HarnessInputMessage::Ready(Ready {
        message: Some("ready".to_owned()),
    }))?;
    writer.flush()?;
    Ok(())
}

fn write_echo_result(
    writer: &mut PeerOutputWriter<impl Write>,
    invoke: ToolStarted,
) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::emit(Event::ToolResult(ToolResult {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: match invoke.arguments {
            CborValue::Null => CborValue::Text("null".to_owned()),
            value => value,
        },
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    })))?;
    writer.flush()?;
    Ok(())
}

fn handle_delivery(
    writer: &mut PeerOutputWriter<impl Write>,
    delivery: EventDelivery,
) -> Result<(), Box<dyn Error>> {
    // Tool invocations are execution triggers; replay-marked frames re-send
    // history and must not re-run them.
    if delivery.is_replay() {
        return Ok(());
    }
    let Event::ToolStarted(invoke) = delivery.into_event() else {
        return Ok(());
    };
    if invoke.tool_name != tau_proto::ToolName::new("echo") {
        return Ok(());
    }
    write_echo_result(writer, invoke)
}

fn run_protocol_mode() -> Result<(), Box<dyn Error>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = PeerInputReader::new(BufReader::new(stdin.lock()));
    let mut writer = PeerOutputWriter::new(BufWriter::new(stdout.lock()));

    write_startup_messages(&mut writer)?;

    loop {
        let Some(message) = reader.read_message()? else {
            return Ok(());
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => handle_delivery(&mut writer, delivery)?,
            HarnessOutputMessage::Disconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    if run_test_mode(args.next(), args)? {
        return Ok(());
    }
    run_protocol_mode()
}
