//! A local HTTP server for the engine's end-to-end delta tests.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Serves fixed bodies by path until the test process exits and records
/// every requested path.
pub(super) struct Server {
    pub(super) base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    pub(super) fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let log = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let response = match files.get(&path) {
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        Self { base, hits }
    }

    pub(super) fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}
