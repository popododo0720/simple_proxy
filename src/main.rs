use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod http_parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Listening on 127.0.0.1:8080");

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("Accepted connection from {}", addr);

        tokio::spawn(async move {
            // let mut buf = [0; 1024];
            let mut client_socket = socket;
            let mut client_buffer = BytesMut::with_capacity(4096);

            let req: http_parser::ParsedRequest;

            loop {
                match http_parser::parse_http_request(&mut client_buffer) {
                    Ok(Some(parsed_req)) => {
                        req = parsed_req;
                        println!("Parsed request from {}: {:?}", addr, req);
                        break;
                    }
                    Ok(None) => {
                        if client_buffer.len() == client_buffer.capacity() {
                            eprintln!("Request buffer full for client {}, but request still incomplete. Sending 413.", addr);
                            let response = b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            if let Err(e) = client_socket.write_all(response).await {
                                eprintln!("Error sending 413 to client {}: {}", addr, e);
                            }
                            return;
                        }

                        match client_socket.read_buf(&mut client_buffer).await {
                            Ok(0) => {
                                if client_buffer.is_empty() {
                                    println!("Client {} disconnected before sending any data.", addr);
                                } else {
                                    println!("Client {} disconnected before sending a complete request.", addr);
                                }
                                return;
                            }
                            Ok(n) => {
                                println!("Read an additional {} bytes from {} (total buffer: {})", n, addr, client_buffer.len());
                                // Continue loop to try parsing again
                            }
                            Err(e) => {
                                eprintln!("Failed to read from client {}: {}", addr, e);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error parsing HTTP request from {}: {}", addr, e);
                        let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        if let Err(write_err) = client_socket.write_all(response).await {
                            eprintln!("Error sending 400 to client {}: {}", addr, write_err);
                        }
                        return;
                    }
                }
            }

            let upstream_addr = "192.168.0.40:80";
            println!("Connecting to upstream server: {} for client {}", upstream_addr, addr);

            let mut upstream_socket = match tokio::net::TcpStream::connect(upstream_addr).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to connect to upstream server {} for client {}: {}", upstream_addr, addr, e);
                    let response = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    if let Err(write_err) = client_socket.write_all(response).await {
                        eprintln!("Error sending 502 to client {}: {}", addr, write_err);
                    }
                    return;
                }
            };
            println!("Connected to upstream for client {}", addr);

            let request_line = format!("{} {} HTTP/1.{}\r\n", req.method, req.path, req.version);
            if let Err(e) = upstream_socket.write_all(request_line.as_bytes()).await {
                eprintln!("Error writing request line to upstream for {}: {}", addr, e);
                return;
            }

            for (name, value) in &req.headers {
                let header_line = format!("{}: {}\r\n", name, value);
                if let Err(e) = upstream_socket.write_all(header_line.as_bytes()).await {
                    eprintln!("Error writing header to upstream for {}: {}", addr, e);
                    return;
                }
            }

            if let Err(e) = upstream_socket.write_all(b"\r\n").await { // End of headers
                eprintln!("Error writing end of headers to upstream for {}: {}", addr, e);
                return;
            }

            if !client_buffer.is_empty() {
                println!("Writing {} bytes of initial body from client_buffer to upstream for {}", client_buffer.len(), addr);
                if let Err(e) = upstream_socket.write_all(&client_buffer).await {
                    eprintln!("Error writing initial body to upstream for {}: {}", addr, e);
                    return;
                }
            }

            let (mut client_read, mut client_write) = client_socket.split();
            let (mut upstream_read, mut upstream_write) = upstream_socket.split();

            let client_to_upstream = tokio::io::copy(&mut client_read, &mut upstream_write);
            let upstream_to_client = tokio::io::copy(&mut upstream_read, &mut client_write);
            
            println!("Starting proxying data between client {} and upstream...", addr);
            match tokio::try_join!(client_to_upstream, upstream_to_client) {
                Ok((bytes_c_to_u, bytes_u_to_c)) => {
                    println!(
                        "Proxy finished for client {}. Sent {} bytes to upstream, received {} bytes from upstream.",
                        addr, bytes_c_to_u, bytes_u_to_c
                    );
                }
                Err(e) => {
                    eprintln!("Error during proxying for client {}: {}", addr, e);
                }
            }
        } );
    }
}
