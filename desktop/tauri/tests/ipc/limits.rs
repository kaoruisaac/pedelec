use pedelec_lib::pedelec_ipc::read_bounded_json_line;
use std::io::{BufReader, Cursor};

#[test]
fn bounded_json_line_reads_a_complete_public_protocol_message() {
    let cursor = Cursor::new(b"{\"type\":\"ping\"}\n".to_vec());
    let mut reader = BufReader::new(cursor);
    assert_eq!(
        read_bounded_json_line(&mut reader).unwrap(),
        b"{\"type\":\"ping\"}"
    );
}
