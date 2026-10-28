use std::io::{Read, Write};
use std::net::TcpStream;

use super::{Sentinel, target_host};

fn through(sentinel: &Sentinel, request: &str) -> String {
    let address = sentinel.base().trim_start_matches("http://").to_owned();
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer
}

#[test]
fn every_proxy_request_is_refused_and_its_host_recorded() {
    let sentinel = Sentinel::start().unwrap();
    let connect = through(
        &sentinel,
        "CONNECT api.mistral.ai:443 HTTP/1.1\r\nhost: api.mistral.ai:443\r\n\r\n",
    );
    assert!(connect.starts_with("HTTP/1.1 403"), "{connect}");
    let absolute = through(
        &sentinel,
        "GET http://pypi.org/pypi/x/json HTTP/1.1\r\nhost: pypi.org\r\n\r\n",
    );
    assert!(absolute.starts_with("HTTP/1.1 403"), "{absolute}");
    assert_eq!(sentinel.hosts(), ["api.mistral.ai:443", "pypi.org:80"]);
    sentinel.clear();
    assert!(sentinel.hosts().is_empty());
}

#[test]
fn an_absolute_uri_names_its_port_or_the_scheme_default() {
    assert_eq!(
        target_host("GET", "https://a.example/x").as_deref(),
        Some("a.example:443")
    );
    assert_eq!(
        target_host("POST", "http://a.example:8080/x?y").as_deref(),
        Some("a.example:8080")
    );
    assert_eq!(target_host("GET", "/relative"), None);
}
