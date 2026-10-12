use discord_media::{
    maybe_run_av1_worker, Av1IpcError, Av1Supervisor, FLAG_ABORT, FLAG_BAD_ID, FLAG_HANG,
};

fn main() {
    if maybe_run_av1_worker() {
        return;
    }
    parent_survives_abort();
    parent_keeps_separate_streams();
    parent_rejects_a_bad_response();
    parent_survives_a_hang();
    parent_refuses_a_delta_before_a_sequence_header();
    eprintln!("av1 process tests passed");
}

fn parent_survives_abort() {
    let mut worker = Av1Supervisor::spawn_with(std::time::Duration::from_secs(2)).expect("worker");
    let err = worker
        .decode_flags(1, &[0x0a], FLAG_ABORT)
        .expect_err("abort");
    eprintln!("abort contained: {err}");
    worker.decode(1, &[]).expect("worker restarted after abort");
}

fn parent_keeps_separate_streams() {
    let mut worker = Av1Supervisor::spawn_with(std::time::Duration::from_secs(2)).expect("worker");
    assert!(worker.decode(11, &[]).unwrap().is_none());
    assert!(worker.decode(22, &[]).unwrap().is_none());
    worker.reset_stream(11).unwrap();
    assert!(worker.decode(22, &[]).unwrap().is_none());
}

fn parent_rejects_a_bad_response() {
    let mut worker = Av1Supervisor::spawn_with(std::time::Duration::from_secs(2)).expect("worker");
    let err = worker
        .decode_flags(1, &[], FLAG_BAD_ID)
        .expect_err("bad id");
    eprintln!("bad response contained: {err}");
    worker
        .decode(1, &[])
        .expect("worker restarted after a bad response");
}

fn parent_refuses_a_delta_before_a_sequence_header() {
    let mut worker = Av1Supervisor::spawn_with(std::time::Duration::from_secs(2)).expect("worker");
    let err = worker
        .decode(1, &[0x30, 0xd1, 0x7b, 0x7e])
        .expect_err("delta without a sequence header");
    match err {
        Av1IpcError::Decode(text) => assert!(text.contains("sequence header"), "{text}"),
        other => panic!("worker aborted instead of refusing the frame: {other}"),
    }
    worker.decode(1, &[]).expect("worker stayed up");
}

fn parent_survives_a_hang() {
    let mut worker =
        Av1Supervisor::spawn_with(std::time::Duration::from_millis(400)).expect("worker");
    let started = std::time::Instant::now();
    let err = worker.decode_flags(1, &[], FLAG_HANG).expect_err("hang");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "hang was not killed"
    );
    eprintln!("hang contained: {err}");
    worker
        .decode(1, &[])
        .expect("worker restarted after a hang");
}
