use super::*;

const AUTHORITY: &str = "127.0.0.1:8765";

fn assert_error(reply: Reply, status: u16) {
  assert_eq!(reply.status, status);
  assert_eq!(reply.content_type, "application/json; charset=utf-8");
  let body: Value = serde_json::from_slice(&reply.body).unwrap();
  assert!(
    body["error"]
      .as_str()
      .is_some_and(|value| !value.is_empty())
  );
}

#[test]
fn local_download_streams_large_file_and_head_never_reads_body() {
  struct Sink {
    bytes: u64,
    largest_write: usize,
    first: Vec<u8>,
  }
  impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
      if self.first.is_empty() {
        self.first = bytes.to_vec();
      }
      self.bytes += bytes.len() as u64;
      self.largest_write = self.largest_write.max(bytes.len());
      Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  let file = tempfile::tempfile().unwrap();
  let size = 32 * 1024 * 1024;
  file.set_len(size).unwrap();
  let mut sink = Sink {
    bytes: 0,
    largest_write: 0,
    first: Vec::new(),
  };
  write_download(
    &mut sink,
    super::super::artifacts::Download::Local {
      file,
      size,
      filename: "model\"é.pt".into(),
    },
    false,
  )
  .unwrap();
  assert!(sink.bytes > size);
  assert!(
    sink.largest_write <= 64 * 1024,
    "checkpoint body must be copied in bounded chunks"
  );
  let file = tempfile::tempfile().unwrap();
  file.set_len(16 * 1024 * 1024 * 1024).unwrap();
  let mut headers = Vec::new();
  write_download(
    &mut headers,
    super::super::artifacts::Download::Local {
      file,
      size: 16 * 1024 * 1024 * 1024,
      filename: "model.pt".into(),
    },
    true,
  )
  .unwrap();
  let headers = String::from_utf8(headers).unwrap();
  assert!(headers.ends_with("\r\n\r\n"));
  assert!(headers.contains("Content-Length: 17179869184\r\n"));
  assert!(headers.contains("Referrer-Policy: no-referrer\r\n"));
  assert!(headers.contains("Content-Disposition: attachment;"));
}

#[test]
fn query_decoding_preserves_repeated_metric_and_run_order() {
  let query =
    Query::parse("run_id=run-two&metric=train%2Floss&run_id=run-one&metric=eval+accuracy")
      .unwrap_or_else(|_| panic!("valid query"));
  assert_eq!(
    query.runs(2).unwrap_or_else(|_| panic!("valid IDs")),
    ["run-two", "run-one"]
  );
  assert_eq!(query.many("metric"), ["train/loss", "eval accuracy"]);
  assert_eq!(
    query
      .number("limit", 100, 1, 1000)
      .unwrap_or_else(|_| panic!("default limit")),
    100
  );
}

#[test]
fn response_writer_does_not_extend_its_deadline_after_a_partial_write() {
  let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
  let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
  let (mut stream, _) = listener.accept().unwrap();
  let mut writer = DeadlineWriter {
    stream: &mut stream,
    deadline: Instant::now() + SOCKET_TIMEOUT,
  };
  writer.write_all(b"first").unwrap();
  writer.deadline = Instant::now();
  let error = writer.write_all(b"second").unwrap_err();
  assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn http_parser_and_body_validation_do_not_allocate_declared_content_lengths() {
  for length in ["1073741824", "18446744073709551615", "not-a-number"] {
    let wire =
      format!("GET /api/catalog HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Length: {length}\r\n\r\n");
    let request = parse_http_request(wire.as_bytes())
      .unwrap_or_else(|_| panic!("parsed headers"))
      .expect("complete headers");
    let headers = request
      .headers()
      .iter()
      .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
      .collect::<Vec<_>>();
    assert_error(validate_body(&headers).expect_err("body rejected"), 400);
  }
  assert!(validate_body(&[]).is_ok());
  assert!(validate_body(&[("Content-Length", "0")]).is_ok());
  for headers in [
    vec![("Content-Length", "0"), ("Content-Length", "0")],
    vec![("Transfer-Encoding", "chunked")],
  ] {
    assert_error(validate_body(&headers).expect_err("body rejected"), 400);
  }
  assert!(matches!(
    parse_http_request(b"GET / HTTP/1.1\r\n"),
    Ok(None)
  ));
  assert_error(
    parse_http_request(b"GET / HTTP/1.1\r\nBroken-Header\r\n\r\n").expect_err("malformed header"),
    400,
  );
  let oversized = format!(
    "GET / HTTP/1.1\r\nX-Large: {}\r\n\r\n",
    "x".repeat(HEADER_LIMIT)
  );
  assert_error(
    parse_http_request(oversized.as_bytes()).expect_err("header limit"),
    431,
  );
  let repeated = format!("GET / HTTP/1.1\r\n{}\r\n", "X-Value: x\r\n".repeat(65));
  assert_error(
    parse_http_request(repeated.as_bytes()).expect_err("header count limit"),
    431,
  );
  let duplicate_host = format!("GET / HTTP/1.1\r\nHost: {AUTHORITY}\r\nHost: {AUTHORITY}\r\n\r\n");
  let request = parse_http_request(duplicate_host.as_bytes())
    .unwrap_or_else(|_| panic!("parsed duplicate headers"))
    .unwrap();
  let headers = request
    .headers()
    .iter()
    .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
    .collect::<Vec<_>>();
  assert_error(
    validate_boundary(AUTHORITY, "GET", "/", &headers).expect_err("duplicate Host rejected"),
    403,
  );
}

#[test]
fn http_response_headers_are_bounded_and_head_omits_the_body() {
  let mut wire = Vec::new();
  write_response(&mut wire, Reply::json(json!({"ok": true})), true).unwrap();
  let text = String::from_utf8(wire).unwrap();
  assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
  assert!(text.contains("content-length: 11\r\n"));
  assert!(text.contains("connection: close\r\n"));
  assert!(text.contains("cache-control: no-store\r\n"));
  assert!(text.contains("x-content-type-options: nosniff\r\n"));
  assert!(text.contains("frame-ancestors 'none'"));
  assert!(text.ends_with("\r\n\r\n"));
  let mut wire = Vec::new();
  let mut chart = Reply::bytes("text/html; charset=utf-8", "<svg></svg>");
  chart.chart = true;
  write_response(&mut wire, chart, false).unwrap();
  let text = String::from_utf8(wire).unwrap();
  assert!(text.contains("frame-ancestors 'self'"));
  assert!(text.contains("style-src 'unsafe-inline'"));
  assert!(text.ends_with("<svg></svg>"));
  let mut wire = Vec::new();
  write_response(&mut wire, Reply::error(405, "method rejected"), false).unwrap();
  assert!(
    String::from_utf8(wire)
      .unwrap()
      .contains("allow: GET, HEAD\r\n")
  );
  assert_error(
    Reply::json(json!({"oversized": "x".repeat(JSON_LIMIT)})),
    413,
  );
  assert_error(
    Reply::bytes("text/html; charset=utf-8", vec![b'x'; HTML_LIMIT + 1]),
    413,
  );
  let mut writer = LimitedWriter(Vec::new(), JSON_LIMIT);
  writer.write_all(&vec![b'x'; JSON_LIMIT]).unwrap();
  assert!(writer.write_all(b"x").is_err());
  assert_eq!(writer.0.len(), JSON_LIMIT);
}

#[test]
fn service_failures_return_json_error_statuses() {
  assert_error(
    service_error(std::io::Error::other("read failure").into()),
    500,
  );
}

#[test]
fn chart_data_has_its_own_bounded_json_budget_without_frame_markup() {
  let mut data = crate::metric_charts::ChartData {
    schema_version: 1,
    x_axis: ChartXAxis::Step,
    runs: Vec::new(),
    plots: Vec::new(),
    warnings: Vec::new(),
    warnings_truncated: false,
    parameter_rows: Vec::new(),
    parameters_truncated: false,
  };
  data.runs.push("x".repeat(JSON_LIMIT));
  let reply = Reply::chart_data(data.clone());
  assert_eq!(reply.status, 200);
  assert_eq!(reply.content_type, "application/json; charset=utf-8");
  assert!(!reply.chart);
  assert!(reply.body.len() > JSON_LIMIT && reply.body.len() < HTML_LIMIT);
  assert!(serde_json::from_slice::<Value>(&reply.body).is_ok());
  data.runs[0] = "x".repeat(HTML_LIMIT);
  assert_error(Reply::chart_data(data), 413);
  assert_error(Reply::json(json!({"large":"x".repeat(JSON_LIMIT)})), 413);
}
