use bytes::BytesMut;
use std::error::Error;
use std::fmt;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod http_parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Listening on 127.0.0.1:8080");
    loop {
        let (socket, addr) = listener.accept().await?;
        println!("Accepted connection from {}", addr);

        tokio::spawn(async move {
            let mut handler = ProxyConnection::new(socket, addr);
            if let Err(e) = handler.handle_request().await {
                eprintln!("Error handling request for {}: {}", addr, e);
            }
        });
    }
}

#[derive(Debug)]
enum ProxyError {
    Io(io::Error),
    Parse(Box<dyn Error + Send + Sync>),
    BufferFull,
    ClientDisconnected,
    HttpErrorSent, 
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "IO error: {}", e),
            ProxyError::Parse(e) => write!(f, "Parse error: {}", e),
            ProxyError::BufferFull => write!(f, "Client buffer full"),
            ProxyError::ClientDisconnected => write!(f, "Client disconnected prematurely"),
            ProxyError::HttpErrorSent => write!(f, "HTTP error response was sent to client"),
        }
    }
}

impl Error for ProxyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProxyError::Io(e) => Some(e),
            ProxyError::Parse(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for ProxyError {
    fn from(err: io::Error) -> ProxyError {
        ProxyError::Io(err)
    }
}

impl From<Box<dyn Error + Send + Sync>> for ProxyError {
    fn from(err: Box<dyn Error + Send + Sync>) -> ProxyError {
        ProxyError::Parse(err)
    }
}

struct ProxyConnection {
    client_socket: TcpStream,
    client_addr: std::net::SocketAddr,
    client_buffer: BytesMut,
}

impl ProxyConnection {
    fn new(client_socket: TcpStream, client_addr: std::net::SocketAddr) -> Self {
        Self {
            client_socket,
            client_addr,
            client_buffer: BytesMut::with_capacity(4096),
        }
    }

    async fn send_http_error(&mut self, status_code: u16, message: &str) -> Result<(), io::Error> {
        let response_body = format!("<h1>{} {}</h1>", status_code, message);
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_code, message, response_body.len(), response_body
        );
        self.client_socket.write_all(response.as_bytes()).await?;
        self.client_socket.shutdown().await?; 
        Ok(())
    }

    async fn read_and_parse_request(&mut self) -> Result<http_parser::ParsedRequest, ProxyError> {
        loop {
            match http_parser::parse_http_request(&mut self.client_buffer) {
                Ok(Some(parsed_req)) => {
                    println!("Parsed request from {}: {:?}", self.client_addr, parsed_req);
                    return Ok(parsed_req);
                }
                Ok(None) => { // Partial request
                    if self.client_buffer.len() == self.client_buffer.capacity() {
                        eprintln!("Request buffer full for client {}, but request still incomplete.", self.client_addr);
                        self.send_http_error(413, "Payload Too Large").await.map_err(|e| eprintln!("Error sending 413: {}", e)).ok();
                        return Err(ProxyError::HttpErrorSent);
                    }

                    match self.client_socket.read_buf(&mut self.client_buffer).await {
                        Ok(0) => {
                            if self.client_buffer.is_empty() {
                                println!("Client {} disconnected before sending any data.", self.client_addr);
                            } else {
                                println!("Client {} disconnected before sending a complete request.", self.client_addr);
                            }
                            return Err(ProxyError::ClientDisconnected);
                        }
                        Ok(n) => {
                            println!("Read an additional {} bytes from {} (total buffer: {})", n, self.client_addr, self.client_buffer.len());
                        }
                        Err(e) => return Err(ProxyError::Io(e)),
                    }
                }
                Err(e) => {
                    eprintln!("Error parsing HTTP request from {}: {}", self.client_addr, e);
                    self.send_http_error(400, "Bad Request").await.map_err(|e| eprintln!("Error sending 400: {}", e)).ok();
                    return Err(ProxyError::HttpErrorSent);
                }
            }
        }
    }

    async fn connect_to_upstream(&mut self, _req: &http_parser::ParsedRequest) -> Result<TcpStream, ProxyError> {
        let upstream_addr = "192.168.0.40:80";
        println!("Connecting to upstream server: {} for client {}", upstream_addr, self.client_addr);
        match TcpStream::connect(upstream_addr).await {
            Ok(socket) => {
                println!("Connected to upstream for client {}", self.client_addr);
                Ok(socket)
            }
            Err(e) => {
                eprintln!("Failed to connect to upstream server {} for client {}: {}", upstream_addr, self.client_addr, e);
                self.send_http_error(502, "Bad Gateway").await.map_err(|e| eprintln!("Error sending 502: {}", e)).ok();
                Err(ProxyError::HttpErrorSent)
            }
        }
    }

    async fn forward_request_to_upstream(&mut self, upstream_socket: &mut TcpStream, req: &http_parser::ParsedRequest) -> Result<(), ProxyError> {
        let request_line = format!("{} {} HTTP/1.{}\r\n", req.method, req.path, req.version);
        upstream_socket.write_all(request_line.as_bytes()).await?;

        for (name, value) in &req.headers {
            let header_line = format!("{}: {}\r\n", name, value);
            upstream_socket.write_all(header_line.as_bytes()).await?;
        }
        upstream_socket.write_all(b"\r\n").await?; // End of headers

        if !self.client_buffer.is_empty() { // Send remaining body from initial parse buffer
            println!("Writing {} bytes of initial body from client_buffer to upstream for {}", self.client_buffer.len(), self.client_addr);
            upstream_socket.write_all(&self.client_buffer).await?;
        }
        Ok(())
    }

    async fn proxy_bidirectional_data(&mut self, upstream_socket: &mut TcpStream) -> Result<(), ProxyError> {
        let (mut client_read, mut client_write) = self.client_socket.split();
        let (mut upstream_read, mut upstream_write) = upstream_socket.split();

        let client_to_upstream = tokio::io::copy(&mut client_read, &mut upstream_write);
        let upstream_to_client = tokio::io::copy(&mut upstream_read, &mut client_write);

        println!("Starting proxying data between client {} and upstream...", self.client_addr);
        match tokio::try_join!(client_to_upstream, upstream_to_client) {
            Ok((bytes_c_to_u, bytes_u_to_c)) => {
                println!(
                    "Proxy finished for client {}. Sent {} bytes to upstream, received {} bytes from upstream.",
                    self.client_addr, bytes_c_to_u, bytes_u_to_c
                );
                Ok(())
            }
            Err(e) => {
                eprintln!("Error during proxying for client {}: {}", self.client_addr, e);
                Err(ProxyError::Io(e))
            }
        }
    }

    pub async fn handle_request(&mut self) -> Result<(), ProxyError> {
        let req = self.read_and_parse_request().await?;
        
        let mut upstream_socket = self.connect_to_upstream(&req).await?;
        
        match self.forward_request_to_upstream(&mut upstream_socket, &req).await {
            Ok(_) => {
            }
            Err(forward_err) => { 
                eprintln!("Error forwarding request to upstream for {}: {}", self.client_addr, forward_err);
                if let Err(e) = self.send_http_error(502, "Bad Gateway (Upstream Write Error)").await {
                    eprintln!("Error sending 502 to client {}: {}", self.client_addr, e);
                }
                return Err(ProxyError::HttpErrorSent); 
            }
        }

        self.proxy_bidirectional_data(&mut upstream_socket).await?;

        Ok(())
    }
}
