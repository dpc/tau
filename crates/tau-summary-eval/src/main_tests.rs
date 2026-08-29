use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;

use tempfile::tempdir;

use super::{Cli, Command, MAXIMUM_INPUT_BYTES, read_bounded, run};

const CORPUS: &[u8] = include_bytes!("../fixtures/corpus-v1.json");
const CANDIDATES: &[u8] = include_bytes!("../fixtures/offline-candidates-v1.json");
const EXPECTED_RESULT: &[u8] = include_bytes!("../fixtures/offline-result-v1.json");

/// Stream reads accept the exact limit but stop and reject after one extra
/// byte.
#[test]
fn bounded_reader_enforces_limit_during_the_read() {
    let exact =
        read_bounded(Cursor::new(vec![b'x'; MAXIMUM_INPUT_BYTES])).expect("exact limit accepted");
    assert_eq!(exact.len(), MAXIMUM_INPUT_BYTES);

    let error = read_bounded(Cursor::new(vec![b'x'; MAXIMUM_INPUT_BYTES + 1]))
        .expect_err("one byte over must fail");
    assert!(error.contains("4 MiB"));
}

/// The CLI pins complete v1 output, creates it privately, and refuses
/// overwrite.
#[test]
fn score_cli_emits_exact_private_v1_record_once() {
    let directory = tempdir().expect("temporary directory");
    let corpus = directory.path().join("corpus.json");
    let candidates = directory.path().join("candidates.json");
    let output = directory.path().join("result.json");
    fs::write(&corpus, CORPUS).expect("write corpus");
    fs::write(&candidates, CANDIDATES).expect("write candidates");

    let command = || Cli {
        command: Command::Score {
            corpus: corpus.clone(),
            candidates: candidates.clone(),
            output: output.clone(),
        },
    };
    run(command()).expect("first score");

    assert_eq!(fs::read(&output).expect("read result"), EXPECTED_RESULT);
    let mode = fs::metadata(&output)
        .expect("result metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        run(command())
            .expect_err("overwrite must fail")
            .contains("cannot create")
    );
}
