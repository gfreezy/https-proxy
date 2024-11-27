use anyhow::Result;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::Uri;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type HttpsClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Incoming,
>;

#[tokio::main]
async fn main() -> Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], 9999));
    let listener = TcpListener::bind(addr).await?;
    println!("Proxy server listening on http://{}", addr);
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .build();
    let client: HttpsClient = Client::builder(TokioExecutor::default()).build(https);
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        let client = client.clone();
        tokio::task::spawn(async move {
            let stream = io.into_inner();
            let mut peek_buf = [0u8; 1024];

            match stream.peek(&mut peek_buf).await {
                Ok(n) if n >= 7 => {
                    if &peek_buf[..7] == b"CONNECT" {
                        if let Err(e) = handle_https_proxy(stream).await {
                            // eprintln!("Error handling HTTPS proxy: {:?}", e);
                        }
                    } else {
                        let io = TokioIo::new(stream);
                        if let Err(err) = http1::Builder::new()
                            .serve_connection(
                                io,
                                hyper::service::service_fn(move |req| {
                                    handle_http_request(client.clone(), req)
                                }),
                            )
                            .await
                        {
                            eprintln!("Error serving connection: {:?}", err);
                        }
                    }
                }
                _ => {
                    // eprintln!("Failed to peek connection data");
                }
            }
        });
    }
}

fn is_local_request(uri: &hyper::Uri) -> bool {
    let local_prefixes = [
        "/@",
        "/src",
        "/node_modules",
        "/__vite_dev_proxy__",
        "/lecturer-membership",
    ];
    let path = uri.path();
    local_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix) || path == "/")
}

fn match_and_rewrite_uri(uri: &Uri) -> Result<Uri> {
    if is_local_request(uri) {
        Ok(Uri::builder()
            .scheme("http")
            .authority("localhost:5173")
            .path_and_query(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
            .build()?)
    } else if uri.host() == Some("www.xiachufang.com") {
        Ok(Uri::builder()
            .scheme("https")
            .authority(uri.authority().unwrap().as_str())
            .path_and_query(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
            .build()?)
    } else {
        Ok(uri.clone())
    }
}

async fn handle_http_request(
    client: HttpsClient,
    mut req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Incoming>> {
    let uri = req.uri_mut();
    let new_uri = match_and_rewrite_uri(uri)?;
    // println!("proxy {} to {}", uri, new_uri);
    *uri = new_uri;

    let host = req.uri().authority().and_then(|a| a.as_str().parse().ok());
    if let Some(host) = host {
        req.headers_mut().insert(hyper::header::HOST, host);
        req.headers_mut().remove(hyper::header::ORIGIN);
        req.headers_mut().remove(hyper::header::REFERER);
    }

    Ok(client.request(req).await?)
}

async fn handle_https_proxy(mut stream: TcpStream) -> Result<()> {
    // Read the CONNECT request
    let mut buf = [0; 4096];
    let n = stream.read(&mut buf).await?;

    // Parse the CONNECT request
    let request = String::from_utf8_lossy(&buf[..n]);
    let lines: Vec<&str> = request.lines().collect();
    if lines.is_empty() {
        return Err(anyhow::anyhow!("Empty request"));
    }

    let first_line: Vec<&str> = lines[0].split_whitespace().collect();
    if first_line.len() != 3 || first_line[0] != "CONNECT" {
        return Err(anyhow::anyhow!("Invalid request"));
    }

    let mut addr = first_line[1].to_string();
    let port = addr.split(":").last().unwrap_or("");

    // Send 200 Connection Established
    let response = "HTTP/1.1 200 Connection Established\r\n\r\n";
    stream.write_all(response.as_bytes()).await?;

    // Create bidirectional tunnel
    let (mut ri, mut wi) = stream.split();

    // Read the CONNECT request body
    let size = ri.read(&mut buf).await?;
    // Only websocket ws:// might use port 80
    if port == "80" {
        // println!("detect websocket");
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        let parsed = req.parse(&buf[..size]);
        match parsed {
            Ok(_) => {
                addr = target_addr_for_websocket(&mut req, &addr)?;
            }
            Err(_) => {}
        }
    }

    // Connect to the target server
    // println!("https proxy to {}", addr);
    let mut outbound = TcpStream::connect(addr).await?;
    let (mut ro, mut wo) = outbound.split();
    // Forward the CONNECT request body to the target server
    wo.write_all(&buf[..size]).await?;

    let client_to_server = async {
        tokio::io::copy(&mut ri, &mut wo).await?;
        wo.shutdown().await
    };

    let server_to_client = async {
        tokio::io::copy(&mut ro, &mut wi).await?;
        wi.shutdown().await
    };

    tokio::try_join!(client_to_server, server_to_client)?;
    Ok(())
}

fn target_addr_for_websocket(req: &mut httparse::Request, addr: &str) -> Result<String> {
    // println!(
    //     "parsed request: {:?}, path: {:?}, method: {:?}, headers: {:?}",
    //     req, req.path, req.method, req.headers
    // );
    let is_websocket = req
        .headers
        .iter()
        .find(|h| h.name.to_ascii_lowercase() == "connection")
        .map(|h| h.value.to_ascii_lowercase() == b"upgrade")
        .unwrap_or(false);

    if !is_websocket {
        return Ok(addr.to_string());
    }

    // println!("is websocket");
    let uri = hyper::Uri::builder()
        .scheme("ws")
        .authority(addr)
        .path_and_query(req.path.unwrap_or(""))
        .build()?;
    let new_uri = match_and_rewrite_uri(&uri)?;
    let Some(new_addr) = new_uri.authority() else {
        return Ok(addr.to_string());
    };

    // println!("proxy websocket {} to {}", uri, new_uri);
    Ok(new_addr.as_str().to_string())
}
