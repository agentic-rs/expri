use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

pub(in crate::service::client) struct HttpRequest {
  pub(in crate::service::client) path: String,
  pub(in crate::service::client) headers: BTreeMap<String, String>,
  pub(in crate::service::client) body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> HttpRequest {
  stream.set_nonblocking(false).unwrap();
  stream
    .set_read_timeout(Some(Duration::from_secs(3)))
    .unwrap();
  let mut reader = BufReader::new(stream.try_clone().unwrap());
  let mut first = String::new();
  reader.read_line(&mut first).unwrap();
  let mut headers = BTreeMap::new();
  loop {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "incomplete test request");
    if line == "\r\n" {
      break;
    }
    let (name, value) = line.split_once(':').unwrap();
    headers.insert(name.to_ascii_lowercase(), value.trim().into());
  }
  let length = headers
    .get("content-length")
    .map(|value: &String| value.parse::<usize>().unwrap())
    .unwrap_or(0);
  assert!(length <= 1024 * 1024);
  let mut body = vec![0; length];
  reader.read_exact(&mut body).unwrap();
  HttpRequest {
    path: first.split_whitespace().nth(1).unwrap().into(),
    headers,
    body,
  }
}

pub(in crate::service::client) fn mock(
  count: usize,
  mut handler: impl FnMut(HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let origin = url.clone();
  let task = thread::spawn(move || {
    for _ in 0..count {
      let deadline = Instant::now() + Duration::from_secs(10);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(Instant::now() < deadline, "test client did not connect");
            thread::sleep(Duration::from_millis(5));
          }
          Err(error) => panic!("test accept: {error}"),
        }
      };
      let (status, headers, body) = handler(read_request(&mut stream), &origin);
      write!(stream, "HTTP/1.1 {status} Test\r\nConnection: close\r\n").unwrap();
      if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
      {
        write!(stream, "Content-Length: {}\r\n", body.len()).unwrap();
      }
      for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
      }
      stream.write_all(b"\r\n").unwrap();
      stream.write_all(&body).unwrap();
    }
  });
  (url, task)
}

pub(in crate::service::client) fn concurrent_mock(
  count: usize,
  handler: impl Fn(HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
) -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let origin = url.clone();
  let handler = std::sync::Arc::new(handler);
  let task = thread::spawn(move || {
    let mut workers = Vec::new();
    for _ in 0..count {
      let deadline = Instant::now() + Duration::from_secs(10);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(
              Instant::now() < deadline,
              "concurrent test client did not connect"
            );
            thread::sleep(Duration::from_millis(5));
          }
          Err(error) => panic!("test accept: {error}"),
        }
      };
      let handler = handler.clone();
      let origin = origin.clone();
      workers.push(thread::spawn(move || {
        let (status, headers, body) = handler(read_request(&mut stream), &origin);
        write!(
          stream,
          "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n",
          body.len()
        )
        .unwrap();
        for (name, value) in headers {
          write!(stream, "{name}: {value}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(&body).unwrap();
      }));
    }
    for worker in workers {
      worker.join().unwrap();
    }
  });
  (url, task)
}
