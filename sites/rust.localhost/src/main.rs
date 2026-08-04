use std::{
    env,
    io::{Read, Write},
    net::TcpListener,
};

fn main() -> std::io::Result<()> {
    let port = env::var("PORT").expect("PORT is required");
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request)?;
        let body = b"Rust executable started on demand\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
    }
    Ok(())
}
